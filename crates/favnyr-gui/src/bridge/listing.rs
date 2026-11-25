//! Directory listings: how a folder becomes rows, and which answer wins.
//!
//! Reading a folder happens off the event loop, and the answer arrives well
//! after the user may have navigated somewhere else. Every path into the models
//! is therefore generation-checked: an [AsyncListingDelivery] carries the
//! generation and the path it was requested for, and [async_listing_is_current]
//! is the single gate that decides whether that answer still describes the
//! folder on screen. Dropping a stale one is not an optimization -- applying it
//! would put one folder's rows under another folder's breadcrumbs.
//!
//! Ordering lives here too. Entries are filtered, sorted and grouped before
//! they become rows, so the model already holds display order and a scroll
//! never has to sort.

use super::*;

/// Connection target for the network credentials prompt: a
/// `\\HOST` server root authenticates via `\\HOST\IPC$`; otherwise the
/// resource is connected as-is (`\\HOST\share`).
pub(super) fn net_connect_target(path: &Path) -> String {
    let s = path.to_string_lossy();
    if rfs::unc_server_root(path).is_some() {
        format!(r"{}\IPC$", s.trim_end_matches('\\'))
    } else {
        s.into_owned()
    }
}

/// Is a listing result an "access denied"? (→ attempt a network login.)
pub(super) fn listing_denied(r: &Result<(Vec<Entry>, usize), favnyr_core::Error>) -> bool {
    matches!(
        r,
        Err(favnyr_core::Error::Io(e)) if e.kind() == std::io::ErrorKind::PermissionDenied
    )
}

/// Routes network paths to a worker. Local listings keep the
/// proven synchronous path, limiting the behavior change to the only
/// I/O likely to block for a long time (SMB, mapped drive, UNC).
pub(super) fn refresh_listing(window: &MainWindow, state: &AppState, path: &Path) {
    // The address tree describes the folder about to be shown. `path` is used
    // rather than the tab's stored path: that one only catches up once the
    // listing arrives, so the tree would otherwise keep describing the folder
    // being left.
    push_path_tree(window, path);
    if search::refresh(window, state, path) {
        return;
    }
    if favnyr_core::places::is_network_path(path) || rfs::is_unc_path(path) {
        let panel_idx = *state.active_panel.borrow();
        let name_filter = state.filter.borrow().clone();
        request_async_panel_listing(window, state, panel_idx, path, name_filter, true);
    } else {
        refresh_listing_sync(window, state, path);
    }
}

pub(super) fn request_async_panel_listing(
    window: &MainWindow,
    state: &AppState,
    panel_idx: usize,
    path: &Path,
    name_filter: String,
    authenticate: bool,
) {
    let path = path.to_path_buf();
    let lang = state.snapshot_config().language;
    let manual_order = folder_state_now(state).is_manual(&path);

    let (
        same_dir,
        show_hidden,
        sort,
        group,
        manual_order,
        ext_on,
        ext_text,
        preserved_selected,
        preserved_anchor,
        recover_favorites,
    ) = {
        let mut panels = state.panels.borrow_mut();
        let Some(panel) = panels.get_mut(panel_idx) else {
            return;
        };
        let active = panel.tabs.active;
        let tab = &mut panel.tabs.tabs[active];
        tab.current_path = path.clone();
        let same_dir = panel.displayed_path == path;
        let model = panel.rows_model.clone();
        let selected = if same_dir {
            (0..model.row_count())
                .filter_map(|i| model.row_data(i))
                .filter(|row| row.selected)
                .map(|row| row.name.to_string())
                .collect()
        } else {
            Vec::new()
        };
        let anchor = if same_dir && tab.selection_anchor >= 0 {
            model
                .row_data(tab.selection_anchor as usize)
                .map(|row| row.name.to_string())
        } else {
            None
        };
        (
            same_dir,
            tab.show_hidden,
            tab.sort,
            tab.group_mode,
            manual_order,
            tab.ext_filter_on,
            tab.ext_filter.clone(),
            selected,
            anchor,
            !same_dir || panel.listing_failed,
        )
    };

    let r#gen = state.listing_serial.fetch_add(1, Ordering::SeqCst) + 1;
    {
        let mut panels = state.panels.borrow_mut();
        let Some(panel) = panels.get_mut(panel_idx) else {
            return;
        };
        panel.pending_initial = false;
        panel.pending_listing = true;
        panel.listing_gen = r#gen;
        panel.pending_select = None;
        panel.displayed_path = path.clone();
        panel.unavailable = false;
        let active = panel.tabs.active;
        panel.tabs.tabs[active].current_path = path.clone();
        if !same_dir {
            panel.last_recovery = None;
            panel.reset_rows_viewport();
            panel.replace_rows(Vec::new());
            panel.hidden_count = 0;
            panel.tabs.tabs[active].selection_anchor = -1;
        }
    }
    // Only the active panel has a watcher. Refreshing a non-active
    // split must not invalidate the one used by the active view.
    if panel_idx == *state.active_panel.borrow() {
        state.watcher_gen.fetch_add(1, Ordering::SeqCst);
    }
    update_panels_ui(window, state);

    let queue = state.async_listings.clone();
    let weak = window.as_weak();
    std::thread::spawn(move || {
        let mut listing = rfs::list_dir_counted(&path, show_hidden || manual_order);
        if authenticate
            && listing_denied(&listing)
            && rfs::is_unc_path(&path)
            && favnyr_core::places::net_connect_prompt(&net_connect_target(&path))
        {
            listing = rfs::list_dir_counted(&path, show_hidden || manual_order);
        }
        let result = match listing {
            Ok((mut entries, hidden)) => {
                rfs::sort_with_secondary_and_type_order(
                    &mut entries,
                    sort.column,
                    sort.order,
                    group,
                    sort.secondary,
                    mtime_offset(),
                    &current_type_order(),
                );
                if !manual_order {
                    apply_name_filter(&mut entries, &name_filter);
                    apply_ext_filter(&mut entries, ext_on, &ext_text);
                }
                Ok((entries, hidden))
            }
            Err(err) => {
                let denied = matches!(
                    &err,
                    favnyr_core::Error::Io(e)
                        if e.kind() == std::io::ErrorKind::PermissionDenied
                );
                error!(error = %err, path = %path.display(), "async network list_dir failed");
                Err(denied)
            }
        };
        let delivery = AsyncListingDelivery {
            panel: panel_idx,
            r#gen,
            path,
            result,
            lang,
            preserved_selected,
            preserved_anchor,
            recover_favorites,
        };
        let queued = queue
            .lock()
            .map(|mut pending| pending.push_back(delivery))
            .is_ok();
        if queued {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.invoke_async_listing_drain();
                }
            });
        }
    });
}

pub(super) fn apply_async_listing(window: &MainWindow, state: &AppState, delivery: AsyncListingDelivery) {
    let succeeded = delivery.result.is_ok();
    let is_active = delivery.panel == *state.active_panel.borrow();
    let (compact_icon_rows, item_spacing) = {
        let config = state.config.borrow();
        (
            config.compact_icon_rows_in_preview,
            config.view_item_spacing,
        )
    };
    let manual_order = folder_state_now(state).is_manual(&delivery.path);
    let mut denied_notice = false;
    let mut count = 0usize;
    let pending_focus = {
        let mut panels = state.panels.borrow_mut();
        let Some(panel) = panels.get_mut(delivery.panel) else {
            return;
        };
        let active = panel.tabs.active;
        if !async_listing_is_current(panel, delivery.r#gen, &delivery.path) {
            return; // stale response: a more recent navigation has won
        }
        let zoom = panel.tabs.tabs[active].zoom;
        let group_mode = panel.tabs.tabs[active].group_mode;
        let show_hidden = panel.tabs.tabs[active].show_hidden;
        panel.pending_listing = false;
        panel.listing_failed = !succeeded;
        if succeeded {
            panel.last_recovery = None;
        }
        let pending_select = panel.pending_select.take();
        let pending_focus = pending_select.clone();
        match delivery.result {
            Ok((mut entries, hidden_count)) => {
                if manual_order {
                    apply_manual_order(state, &delivery.path, &mut entries, show_hidden);
                }
                let parent_display = delivery.path.display().to_string();
                let now = now_unix();
                let mut rows = entries_to_rows(
                    &entries,
                    &parent_display,
                    &RowContext {
                        lang: delivery.lang,
                        now_unix: now,
                        big_icon: panel.tabs.tabs[active].preview,
                        grid: panel.tabs.tabs[active].item_arrangement == ItemArrangement::Grid,
                        zoom,
                        compact_icon_rows,
                        item_spacing,
                        group_mode: effective_group_mode(group_mode, manual_order),
                        annotations: &annotations_now(state),
                    },
                );

                let selected: std::collections::HashSet<&str> = delivery
                    .preserved_selected
                    .iter()
                    .map(String::as_str)
                    .collect();
                for row in &mut rows {
                    if selected.contains(row.name.as_str()) {
                        row.selected = true;
                    }
                }
                let select_pending = pending_select.is_some();
                let anchor_name = pending_select.or(delivery.preserved_anchor);
                let anchor = anchor_name
                    .as_deref()
                    .and_then(|name| rows.iter().position(|row| row.name.as_str() == name))
                    .map(|idx| idx as i32)
                    .unwrap_or(-1);
                if select_pending {
                    for row in &mut rows {
                        row.selected = Some(row.name.as_str()) == anchor_name.as_deref();
                    }
                }

                let clip = state.clipboard.borrow();
                if matches!(clip.op, Some(ClipOp::Cut)) {
                    let cut_names: std::collections::HashSet<&str> = clip
                        .paths
                        .iter()
                        .filter(|p| p.parent() == Some(delivery.path.as_path()))
                        .filter_map(|p| p.file_name().and_then(|name| name.to_str()))
                        .collect();
                    for row in &mut rows {
                        if cut_names.contains(row.name.as_str()) {
                            row.cut = true;
                        }
                    }
                }
                count = rows.len();
                panel.replace_rows(rows);
                panel.tabs.tabs[active].selection_anchor = anchor;
                panel.tabs.tabs[active].cursor = anchor;
                panel.hidden_count = hidden_count;
                panel.unavailable = false;
            }
            Err(denied) => {
                panel.replace_rows(Vec::new());
                panel.tabs.tabs[active].selection_anchor = -1;
                panel.tabs.tabs[active].cursor = -1;
                panel.hidden_count = 0;
                panel.unavailable = !denied;
                denied_notice = denied && is_active;
            }
        }
        panel.displayed_path = delivery.path.clone();
        pending_focus
    };
    if denied_notice {
        show_notice(window, i18n::access_denied(delivery.lang));
    }
    if succeeded && delivery.recover_favorites {
        recovery::request_favorites(window, state);
    }
    update_panels_ui(window, state);
    if let Some(name) = pending_focus {
        schedule_focus_entry_by_name(window, state, delivery.path.clone(), name);
    }
    // The panel may have become inactive during the network listing while
    // remaining visible in the split: its previews must still start.
    request_thumbnails(state);
    request_folder_stats(state);
    request_imgmeta(state);
    if is_active {
        install_watcher(state, window, &delivery.path);
    }
    debug!(path = %delivery.path.display(), count, "async network listing applied");
}

pub(super) fn async_listing_is_current(panel: &Panel, r#gen: u64, path: &Path) -> bool {
    panel.pending_listing
        && panel.listing_gen == r#gen
        && panel
            .tabs
            .tabs
            .get(panel.tabs.active)
            .is_some_and(|tab| tab.current_path == path)
}

pub(super) fn refresh_listing_sync(window: &MainWindow, state: &AppState, path: &Path) {
    let cfg = state.snapshot_config();
    let lang = cfg.language;

    // Same-dir = the active panel's `displayed_path` hasn't changed.
    let active_idx = *state.active_panel.borrow();
    let same_dir = {
        let mut panels = state.panels.borrow_mut();
        // Synchronous listing is fresher than the deferred initial population.
        panels[active_idx].pending_initial = false;
        panels[active_idx].pending_listing = false;
        panels[active_idx].pending_select = None;
        let active = panels[active_idx].tabs.active;
        panels[active_idx].tabs.tabs[active].current_path = path.to_path_buf();
        let same_dir = panels[active_idx].displayed_path == path;
        if !same_dir {
            panels[active_idx].reset_rows_viewport();
        }
        same_dir
    };

    // Preservation: we read from the active panel's model if same_dir.
    let model = state.active_rows_model();
    let (preserved_selected, preserved_anchor_name) = if same_dir {
        let preserved: Vec<String> = (0..model.row_count())
            .filter_map(|i| model.row_data(i))
            .filter(|r| r.selected)
            .map(|r| r.name.to_string())
            .collect();
        let anchor_name = state.with_tabs(|book| {
            let a = book.active;
            let anchor = book.tabs[a].selection_anchor;
            if anchor >= 0 {
                model.row_data(anchor as usize).map(|r| r.name.to_string())
            } else {
                None
            }
        });
        (preserved, anchor_name)
    } else {
        (Vec::new(), None)
    };

    let show_hidden = state.with_tabs(|book| {
        let tab = &book.tabs[book.active];
        tab.show_hidden
    });
    let manual_order = folder_state_now(state).is_manual(path);
    // Local listing only. `refresh_listing` routes every network path to the
    // asynchronous version, which is where the system credentials prompt and
    // its retry live — a copy of them here could never run, since the path is
    // local by construction, and a blocking prompt has no business on this
    // thread anyway.
    let (mut entries, hidden_count) = match rfs::list_dir_counted(path, show_hidden || manual_order)
    {
        Ok(ec) => ec,
        Err(err) => {
            error!(error = %err, path = %path.display(), "list_dir failed");
            // An access denial on a protected folder produces a notification
            // instead of being presented as an empty list. Detected via io::Error.
            let denied = matches!(
                &err,
                favnyr_core::Error::Io(e) if e.kind() == std::io::ErrorKind::PermissionDenied
            );
            // Everything that ISN'T an access denial (missing path, network drive
            // not started, mount gone) → "unavailable" tab: we keep the
            // path + a persistent banner + auto re-check.
            let unavailable = !denied;
            if denied {
                show_notice(window, i18n::access_denied(lang));
            }
            {
                let panels = state.panels.borrow();
                panels[active_idx].replace_rows(Vec::new());
            }
            state.with_tabs_mut(|book| {
                let a = book.active;
                book.tabs[a].selection_anchor = -1;
                book.tabs[a].current_path = path.to_path_buf();
            });
            {
                let mut panels = state.panels.borrow_mut();
                panels[active_idx].displayed_path = path.to_path_buf();
                panels[active_idx].hidden_count = 0;
                panels[active_idx].unavailable = unavailable;
                panels[active_idx].listing_failed = true;
            }
            install_watcher(state, window, path);
            update_panels_ui(window, state);
            return;
        }
    };

    let sort_state = state.with_tabs(|book| {
        let a = book.active;
        book.tabs[a].sort
    });
    let group_mode = state.with_tabs(|book| book.tabs[book.active].group_mode);
    let big_icon = state.with_tabs(|book| book.tabs[book.active].preview);
    let grid =
        state.with_tabs(|book| book.tabs[book.active].item_arrangement == ItemArrangement::Grid);
    let zoom = state.with_tabs(|book| book.tabs[book.active].zoom);
    rfs::sort_with_secondary_and_type_order(
        &mut entries,
        sort_state.column,
        sort_state.order,
        group_mode,
        sort_state.secondary,
        mtime_offset(),
        &current_type_order(),
    );
    if manual_order {
        apply_manual_order(state, path, &mut entries, show_hidden);
    }

    // Active view's "type-ahead" filter: searches for the fragment anywhere
    // in the name (case-insensitive).
    if !manual_order {
        apply_name_filter(&mut entries, &state.filter.borrow());
    }
    // Extension filter — orthogonal to type-ahead; doesn't touch
    // folders. Read from the active tab.
    {
        let (on, txt) = state.with_tabs(|book| {
            let a = book.active;
            (book.tabs[a].ext_filter_on, book.tabs[a].ext_filter.clone())
        });
        if !manual_order {
            apply_ext_filter(&mut entries, on, &txt);
        }
    }

    let parent_display = path.display().to_string();
    let now = now_unix();
    let mut rows = entries_to_rows(
        &entries,
        &parent_display,
        &RowContext {
            lang,
            now_unix: now,
            big_icon,
            grid,
            zoom,
            compact_icon_rows: cfg.compact_icon_rows_in_preview,
            item_spacing: cfg.view_item_spacing,
            group_mode: effective_group_mode(group_mode, manual_order),
            annotations: &annotations_now(state),
        },
    );

    if !preserved_selected.is_empty() {
        let set: std::collections::HashSet<&str> =
            preserved_selected.iter().map(|s| s.as_str()).collect();
        for r in rows.iter_mut() {
            if set.contains(r.name.as_str()) {
                r.selected = true;
            }
        }
    }
    {
        let clip = state.clipboard.borrow();
        if matches!(clip.op, Some(ClipOp::Cut)) {
            let cut_names: std::collections::HashSet<&str> = clip
                .paths
                .iter()
                .filter(|p| p.parent() == Some(path))
                .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
                .collect();
            for r in rows.iter_mut() {
                if cut_names.contains(r.name.as_str()) {
                    r.cut = true;
                }
            }
        }
    }

    let count = rows.len();
    let new_anchor: i32 = preserved_anchor_name
        .and_then(|n| rows.iter().position(|r| r.name.as_str() == n))
        .map(|p| p as i32)
        .unwrap_or(-1);

    state.with_tabs_mut(|book| {
        let a = book.active;
        let t = &mut book.tabs[a];
        t.current_path = path.to_path_buf();
        t.selection_anchor = new_anchor;
        t.cursor = new_anchor;
    });
    {
        let mut panels = state.panels.borrow_mut();
        panels[active_idx].replace_rows(rows);
        panels[active_idx].displayed_path = path.to_path_buf();
        panels[active_idx].hidden_count = hidden_count;
        panels[active_idx].unavailable = false; // listing OK → available again
        panels[active_idx].listing_failed = false;
        panels[active_idx].last_recovery = None;
    }

    install_watcher(state, window, path);
    update_panels_ui(window, state);
    request_thumbnails(state);
    request_folder_stats(state);
    request_imgmeta(state);
    debug!(path = %path.display(), count, "listing refreshed");
}

/// Adjusts a panel's delegate window. The filtered model then automatically
/// relays selection, cut, metadata, and thumbnails of the admitted rows.
/// Returns the STRICTLY visible range for the worker's priority.
pub(super) fn update_panel_render_window(
    state: &AppState,
    panel_idx: usize,
    top: f32,
    height: f32,
) -> (i32, i32) {
    let top = top.max(0.0);
    let height = height.max(0.0);
    let Some((model, preview, old_first, old_end, new_first, new_end, visible)) =
        state.panels.try_borrow().ok().and_then(|panels| {
            let panel = panels.get(panel_idx)?;
            panel.viewport_top.set(top);
            if height > 0.0 {
                panel.viewport_height.set(height);
            }
            let effective_height = panel.viewport_height.get().max(1.0);
            let render_range = arranged_model_geometry(&*panel.rows_model, panel.item_grid.get())
                .render_range(top, effective_height);
            let (new_first, new_end) = (render_range.start, render_range.end);
            let visible = if height > 0.0 {
                let range = arranged_model_geometry(&*panel.rows_model, panel.item_grid.get())
                    .visible_range(top, top + height);
                (range.start, range.end)
            } else {
                (0, 0)
            };
            let old_first = panel.rendered_first.replace(new_first);
            let old_end = panel.rendered_end.replace(new_end);
            Some((
                panel.rows_model.clone(),
                panel.tabs.tabs[panel.tabs.active].preview,
                old_first,
                old_end,
                new_first,
                new_end,
                visible,
            ))
        })
    else {
        return (-1, -1);
    };

    // The symmetric difference is enough to materialize/dematerialize the
    // delegates. We ALWAYS add to it the strictly visible range: after a
    // zoom relayout, `replace_rows` has already pre-marked the new window and
    // old == new on the next callback. Without this bounded reconciliation, a
    // missing texture (LRU evicted, request cancelled while switching to list mode)
    // could stay visible without being either pending or in-flight.
    let mut inspected = if (old_first, old_end) != (new_first, new_end) {
        render_window_changed_indices(old_first, old_end, new_first, new_end)
    } else {
        Vec::new()
    };
    inspected.extend(visible.0..visible.1);
    inspected.sort_unstable();
    inspected.dedup();

    let row_count = model.row_count();
    let mut requests: HashMap<PathBuf, ThumbRequest> = HashMap::new();
    for index in inspected {
        if index >= row_count {
            continue;
        }
        let should_render = index >= new_first && index < new_end;
        let Some(mut row) = model.row_data(index) else {
            continue;
        };
        let mut row_changed = false;
        if row.rendered != should_render {
            row.rendered = should_render;
            row_changed = true;
        }

        if !should_render {
            // The texture stays in the LRU, not in the thousands of off-screen
            // rows. This makes the cache's memory bound actually effective.
            if row.thumbnail.size().width > 0 {
                row.thumbnail = Image::default();
                row_changed = true;
            }
        } else if preview && row.preview_capable && row.thumbnail.size().width == 0 {
            let full = row_path(&row);
            let key = full.to_string_lossy().to_string();
            if let Some(img) = state.thumb_cache.borrow_mut().get(&key) {
                row.thumbnail = img;
                row_changed = true;
            } else if let Some((kind, svg)) = thumbnail_kind_for_row(row.kind, row.ext.as_str()) {
                push_thumbnail_request(
                    &mut requests,
                    full,
                    kind,
                    svg,
                    ThumbLocation {
                        panel: panel_idx,
                        row: index,
                        row_count,
                    },
                );
            }
        }
        if row_changed {
            model.set_row_data(index, row);
        }
    }
    if !requests.is_empty() {
        state
            .thumb_scheduler
            .merge_pending(requests.into_values().collect());
    }

    if visible.0 < visible.1 {
        (visible.0 as i32, visible.1 as i32 - 1)
    } else {
        (-1, -1)
    }
}

/// Parses the extension filter's free text into lowercase extensions
/// without a dot. FLEXIBLE syntax: comma AND/OR space separators
/// ("jpg, png", "jpg png", ".JPG,.PNG" → `["jpg","png"]`). Empty if nothing.
pub(super) fn parse_ext_filter(text: &str) -> Vec<String> {
    text.split(|c: char| c == ',' || c.is_whitespace())
        .map(|t| t.trim_start_matches('.').to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Applies the extension filter to `entries` (in place): keeps only the
/// files whose extension is in the filter. FOLDERS remain visible
/// (navigation preserved). No-op if the filter is empty/disabled.
pub(super) fn apply_ext_filter(entries: &mut Vec<Entry>, on: bool, filter: &str) {
    if !on {
        return;
    }
    let exts = parse_ext_filter(filter);
    if exts.is_empty() {
        return;
    }
    entries.retain(|e| {
        e.is_dir || {
            let ext = ops::ext_of(&e.name);
            !ext.is_empty() && exts.contains(&ext)
        }
    });
}

/// Reconciles and applies a directory's explicit order after the regular sort has
/// established a deterministic starting point. `entries` must be a complete
/// listing, including hidden items, so names missing from disk can be removed
/// without confusing them with entries hidden only from the current display.
pub(super) fn apply_manual_order(
    state: &AppState,
    directory: &Path,
    entries: &mut Vec<Entry>,
    show_hidden: bool,
) -> bool {
    let names: Vec<String> = entries.iter().map(|entry| entry.name.clone()).collect();
    let (order, changed) = {
        let mut folder_state = folder_state_for_update(state);
        let changed = folder_state.reconcile(directory, &names);
        let order = folder_state.order(directory).map(<[String]>::to_vec);
        if changed {
            save_folder_state(state, &folder_state);
        }
        (order, changed)
    };
    let Some(order) = order else {
        if !show_hidden {
            entries.retain(|entry| !entry.hidden);
        }
        return false;
    };
    let ranks: HashMap<&str, usize> = order
        .iter()
        .enumerate()
        .map(|(index, name)| (name.as_str(), index))
        .collect();
    entries.sort_by_key(|entry| {
        ranks
            .get(entry.name.as_str())
            .copied()
            .unwrap_or(usize::MAX)
    });
    if !show_hidden {
        entries.retain(|entry| !entry.hidden);
    }
    changed
}

pub(super) fn effective_group_mode(group_mode: GroupMode, manual_order: bool) -> GroupMode {
    if manual_order {
        GroupMode::Mixed
    } else {
        group_mode
    }
}

/// Everything turning an `Entry` into a displayable row needs beyond the entry
/// itself. Bundled because both builders below take the same set, and threading
/// it as loose parameters had grown past the point where a call site reads.
/// Every field is `Copy`, so the two functions destructure it back into the
/// plain names their bodies use.
pub(super) struct RowContext<'a> {
    pub(super) lang: Lang,
    pub(super) now_unix: i64,
    pub(super) big_icon: bool,
    pub(super) grid: bool,
    pub(super) zoom: i32,
    pub(super) compact_icon_rows: bool,
    pub(super) item_spacing: i32,
    pub(super) group_mode: GroupMode,
    pub(super) annotations: &'a favnyr_core::annotations::AnnotationStore,
}

pub(super) fn entry_to_row(e: &Entry, parent_display: &str, ctx: &RowContext) -> FileRow {
    let actual_parent = e
        .path
        .parent()
        .filter(|path| *path != Path::new(parent_display))
        .map(|path| path.display().to_string());
    let parent_display = actual_parent.as_deref().unwrap_or(parent_display);
    let RowContext {
        lang,
        now_unix,
        big_icon,
        grid,
        compact_icon_rows,
        annotations,
        ..
    } = *ctx;
    let size = if e.is_dir {
        "—".to_string()
    } else {
        e.size_bytes
            .map(|b| rfs::format_size(b, i18n::size_units(lang)))
            .unwrap_or_else(|| "—".to_string())
    };
    let modified = e
        .mtime_unix
        .map(|m| rfs::format_mtime(m, mtime_offset()))
        .unwrap_or_else(|| "—".to_string());
    let created = e
        .created_unix
        .map(|value| rfs::format_mtime(value, mtime_offset()))
        .unwrap_or_else(|| "—".to_string());
    // "age" column: compact text + warm-to-cold color bucket.
    let (age, age_bucket) = match e.mtime_unix {
        Some(m) => (
            rfs::format_age(m, now_unix, i18n::age_units(lang)),
            rfs::age_bucket(m, now_unix),
        ),
        None => ("—".to_string(), -1),
    };
    // Splits name/extension for coloring (files only: a
    // folder named "x.y" has no "extension" to highlight).
    // Name DISPLAY (stem + colored extension): `split_name` leaves
    // dotfiles whole. TYPING (the "ext" column, icon, sort, filter): `ext_of`
    // treats a dotfile's suffix as an extension (`.gitignore` → gitignore).
    let (name_base, name_ext) = if e.is_dir {
        (e.name.clone(), String::new())
    } else {
        let (stem, ext) = ops::split_name(&e.name);
        if ext.is_empty() {
            (e.name.clone(), String::new())
        } else {
            (stem, format!(".{ext}"))
        }
    };
    let ext_raw = if e.is_dir {
        String::new()
    } else {
        ops::ext_of(&e.name)
    };
    #[cfg(windows)]
    let drop_runnable = !e.is_dir && matches!(ext_raw.as_str(), "exe" | "com" | "cmd" | "bat");
    // The executable bit alone gives false positives (FAT/NTFS/exFAT mounts at
    // 0777, files copied from Windows): an image/video/document… is NOT
    // a program, so no "launch with these files" (see can_be_program).
    #[cfg(not(windows))]
    let drop_runnable = !e.is_dir && e.executable && e.kind.can_be_program();
    let preview_capable = thumbnail_kind_for_row(e.kind.as_i32(), &ext_raw).is_some();
    // Actual OS app icon for this extension — empty for a folder
    // or if unavailable (falls back to the view's type icon). Cached.
    // Resolution suited to the mode: 256 px in previews (enlarged), 32 px in list.
    // For `.lnk` shortcuts: we resolve the target → target app icon
    // (file) OR "folder + arrow" marker (folder) via `link_folder`.
    let use_large_icon = grid || big_icon && (!compact_icon_rows || preview_capable);
    let (app_icon, link_folder) =
        row_app_icon(parent_display, &e.name, &ext_raw, e.is_dir, use_large_icon);
    FileRow {
        name: e.name.clone().into(),
        name_base: name_base.into(),
        name_ext: name_ext.into(),
        ext: ext_raw.into(),
        path: parent_display.to_string().into(),
        size: size.into(),
        modified: modified.into(),
        created: created.into(),
        is_dir: e.is_dir,
        is_symlink: e.is_symlink,
        // Only consulted for folders, so a stray slot on a file costs nothing.
        folder_slot: i32::from(annotations.color_of(&e.path)),
        comment: annotations.note_of(&e.path).into(),
        drop_runnable,
        kind: e.kind.as_i32(),
        selected: false,
        cut: false,
        hidden: e.hidden,
        age: age.into(),
        age_bucket,
        // Image metadata: empty here; filled in place by the
        // imgmeta worker when the resolution/depth column is visible.
        resolution: SharedString::default(),
        depth: SharedString::default(),
        // Thumbnail: empty here; filled in place by the worker in
        // preview mode, or immediately from the cache in update_panels_ui.
        thumbnail: slint::Image::default(),
        app_icon,
        link_folder,
        preview_capable,
        visual_y: 0.0,
        visual_h: COMPACT_ICON_ROW_HEIGHT,
        model_index: 0,
        group_start: false,
        rendered: false,
    }
}

pub(super) fn entries_to_rows(entries: &[Entry], parent_display: &str, ctx: &RowContext) -> Vec<FileRow> {
    let RowContext {
        zoom,
        compact_icon_rows,
        item_spacing,
        group_mode,
        grid,
        ..
    } = *ctx;
    let mut rows: Vec<FileRow> = entries
        .iter()
        .map(|entry| entry_to_row(entry, parent_display, ctx))
        .collect();
    if group_mode == GroupMode::ColoredFoldersFirst {
        group_colored_folder_rows(&mut rows);
    }
    layout_rows(
        &mut rows,
        zoom,
        compact_icon_rows,
        item_spacing,
        // Same rule as `separates_type_sections`, spelled out here because this
        // path only has the context, not the tab.
        group_mode == GroupMode::ByType && !grid,
    );
    rows
}

/// Stable subdivision applied after folder colours have been projected onto
/// rows. The active column sort already ordered the folder and file blocks;
/// preserving equal-rank order keeps that criterion intact inside all three
/// resulting blocks.
pub(super) fn group_colored_folder_rows(rows: &mut [FileRow]) {
    rows.sort_by_key(|row| {
        if row.is_dir && row.folder_slot > 0 {
            0
        } else if row.is_dir {
            1
        } else {
            2
        }
    });
}

pub(super) fn apply_name_filter(entries: &mut Vec<Entry>, filter: &str) {
    if filter.is_empty() {
        return;
    }
    let needle = filter.to_lowercase();
    entries.retain(|entry| name_contains_filter(&entry.name, &needle));
}

/// `needle` is already normalized to lowercase by the listing's caller, so as
/// not to redo this work for every entry of a large folder.
pub(super) fn name_contains_filter(name: &str, needle: &str) -> bool {
    name.to_lowercase().contains(needle)
}
