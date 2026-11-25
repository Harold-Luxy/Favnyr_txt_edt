//! The panel model: what a view holds, and every way it can be created,
//! moved, split, torn off or closed.
//!
//! A [Panel] owns a [TabBook] (its tabs and a cursor), the row models its
//! Slint delegate binds to, and its own display settings. The layout tree in
//! favnyr-core owns where panels sit on screen; this module owns what they
//! contain.
//!
//! Panels are addressed by INDEX, and most mutations here renumber every panel
//! after the one that changed. Two pieces of bookkeeping follow from that and
//! are easy to forget: the editor buffer map is keyed by panel index, and a
//! programmatic swap must not mark the workspace dirty. Removal routes here
//! therefore end in `drop_editor_buffer` (or in [take_view], which does the
//! same), and the in-place workspace swap runs under
//! `suppress_workspace_persist`.

use super::*;

/// A tab groups a current path, its navigation history, and its
/// sort order. Selection is not persisted between tabs; it is,
/// however, preserved when refreshing the same folder via the
/// `same-dir` mechanism of `refresh_listing`.
#[derive(Debug)]
pub(super) struct Tab {
    pub(super) current_path: PathBuf,
    pub(super) history: NavHistory,
    pub(super) sort: SortState,
    /// Selection anchor for Shift+click. `-1` if none.
    pub(super) selection_anchor: i32,
    /// Content thumbnails, derived from list zoom only while using the list.
    /// Grid thumbnails are independent of either size setting.
    pub(super) preview: bool,
    /// Zoom level of entries (Ctrl+wheel). Mapped to a row height by
    /// `zoom_to_height`, and persisted per tab in the workspace TOML: a view
    /// left at a chosen thumbnail size reopens at that size, not at the
    /// default for its mode.
    pub(super) zoom: i32,
    pub(super) item_arrangement: ItemArrangement,
    /// None preserves the absence of a grid preference in older workspaces.
    /// The grid renderer supplies its default when the user first opens it.
    pub(super) grid_zoom: Option<i32>,
    /// Whether hidden files are shown (dotfiles + Windows HIDDEN attribute).
    /// `false` by default. Persisted per tab in the workspace TOML.
    pub(super) show_hidden: bool,
    /// Grouping mode (folders, coloured folders or files first, or mixed).
    /// Persisted per tab in the workspace TOML.
    pub(super) group_mode: GroupMode,
    /// "Cursor" row (head of arrow-key keyboard navigation). `-1`
    /// = none. Session only (not persisted).
    pub(super) cursor: i32,
    /// Counter incremented on each keyboard cursor movement → triggers
    /// scroll-into-view on the Slint side (without doing so on other refreshes).
    pub(super) scroll_gen: i32,
    /// Extension filter: bar enabled from the columns menu.
    /// `ext_filter_on` = bar shown; `ext_filter` = free text ("jpg, png",
    /// loose syntax). Session only (not persisted in the workspace).
    pub(super) ext_filter_on: bool,
    pub(super) ext_filter: String,
}

#[derive(Clone, Copy)]
pub(super) struct TabDisplayDefaults {
    pub(super) preview: bool,
    pub(super) thumbnail_zoom: i32,
    pub(super) arrangement: ItemArrangement,
    pub(super) show_hidden: bool,
}

impl From<&Config> for TabDisplayDefaults {
    fn from(config: &Config) -> Self {
        Self {
            preview: config.default_preview_mode,
            thumbnail_zoom: config
                .default_thumbnail_zoom
                .clamp(MIN_THUMBNAIL_ZOOM, MAX_THUMBNAIL_ZOOM),
            arrangement: config.default_item_arrangement,
            show_hidden: config.default_show_hidden,
        }
    }
}

#[cfg(test)]
impl From<bool> for TabDisplayDefaults {
    fn from(preview: bool) -> Self {
        Self {
            preview,
            arrangement: ItemArrangement::List,
            thumbnail_zoom: DEFAULT_THUMBNAIL_ZOOM,
            show_hidden: false,
        }
    }
}

impl Tab {
    pub(super) fn new(initial: PathBuf, display: impl Into<TabDisplayDefaults>) -> Self {
        let display = display.into();
        let preview = display.preview;
        let mut h = NavHistory::default();
        h.push(initial.clone());
        let zoom = if preview {
            display.thumbnail_zoom
        } else {
            LIST_DEFAULT_ZOOM
        };
        Self {
            current_path: initial,
            history: h,
            sort: SortState::default(),
            selection_anchor: -1,
            preview,
            zoom,
            item_arrangement: display.arrangement,
            grid_zoom: (display.thumbnail_zoom != GRID_DEFAULT_ZOOM)
                .then_some(display.thumbnail_zoom),
            show_hidden: display.show_hidden,
            // Same default as a tab restored without the field: new views open
            // grouped by type.
            group_mode: GroupMode::ByType,
            cursor: -1,
            scroll_gen: 0,
            ext_filter_on: false,
            ext_filter: String::new(),
        }
    }

    /// Builds a tab restored from a workspace: path + sort + display
    /// mode. History restarts with a single entry (back/forward not
    /// persisted).
    pub(super) fn restored(path: PathBuf, saved: &TabState) -> Self {
        let mut h = NavHistory::default();
        h.push(path.clone());
        // A workspace saved before the level was persisted carries none: the
        // tab reopens at the default for the mode it was in, exactly as it
        // used to. A hand-edited file is clamped rather than trusted.
        let zoom = match saved.zoom {
            Some(z) => z.clamp(MIN_ZOOM, MAX_ZOOM),
            None if saved.preview => THUMB_DEFAULT_ZOOM,
            None => LIST_DEFAULT_ZOOM,
        };
        Self {
            current_path: path,
            history: h,
            sort: SortState {
                column: saved.sort_column,
                order: saved.sort_order,
                secondary: saved
                    .secondary_sort
                    .filter(|s| !s.column.same_field(saved.sort_column)),
            },
            selection_anchor: -1,
            // Keep the historical list precedence, including hand-edited
            // workspaces. Grid representation has its own explicit choice.
            preview: match saved.item_arrangement {
                ItemArrangement::List => zoom >= THUMB_ZOOM,
                ItemArrangement::Grid => saved.preview,
            },
            zoom,
            item_arrangement: saved.item_arrangement,
            grid_zoom: saved
                .grid_zoom
                .map(|zoom| zoom.clamp(MIN_GRID_ZOOM, MAX_GRID_ZOOM)),
            show_hidden: saved.show_hidden,
            group_mode: saved.group_mode,
            cursor: -1,
            scroll_gen: 0,
            ext_filter_on: false,
            ext_filter: String::new(),
        }
    }
}

pub(super) fn tab_to_state(tab: &Tab) -> TabState {
    TabState {
        path: tab.current_path.display().to_string(),
        sort_column: tab.sort.column,
        sort_order: tab.sort.order,
        secondary_sort: tab.sort.secondary,
        preview: tab.preview,
        show_hidden: tab.show_hidden,
        group_mode: tab.group_mode,
        zoom: Some(tab.zoom),
        item_arrangement: tab.item_arrangement,
        grid_zoom: tab.grid_zoom,
    }
}

pub(super) fn tab_from_state(tab: TabState) -> Tab {
    Tab::restored(resolve_restored_path(&tab.path), &tab)
}

#[derive(Debug)]
pub(super) struct TabBook {
    pub(super) tabs: Vec<Tab>,
    pub(super) active: usize,
}

impl TabBook {
    /// Inserts a tab at a visible gap, clamping stale UI coordinates, and
    /// activates it. Shared by every source that creates or transfers a tab.
    pub(super) fn insert_tab_at(&mut self, at: usize, tab: Tab) -> usize {
        let at = at.min(self.tabs.len());
        self.tabs.insert(at, tab);
        self.active = at;
        at
    }

    pub(super) fn open(&mut self, path: PathBuf, display: impl Into<TabDisplayDefaults>) -> usize {
        self.insert_tab_at(self.tabs.len(), Tab::new(path, display))
    }

    /// Inserts `tab` right after tab `idx` and activates the new entry.
    /// Common primitive for contextual openings and duplication.
    pub(super) fn insert_after(&mut self, idx: usize, tab: Tab) -> Option<usize> {
        if idx >= self.tabs.len() {
            return None;
        }
        Some(self.insert_tab_at(idx + 1, tab))
    }

    /// Opens a target right after the active tab. A `TabBook` always has
    /// at least one tab; the fallback at the end nonetheless protects this invariant.
    pub(super) fn open_after_active(
        &mut self,
        path: PathBuf,
        display: impl Into<TabDisplayDefaults>,
    ) -> usize {
        if self.active >= self.tabs.len() {
            return self.open(path, display);
        }
        self.insert_after(self.active, Tab::new(path, display))
            .expect("active tab was validated")
    }

    /// Duplicates tab `idx`: inserts a copy (same path + view settings)
    /// RIGHT AFTER it and activates it. `false` if the index is out of bounds.
    pub(super) fn duplicate(&mut self, idx: usize) -> bool {
        let Some(src) = self.tabs.get(idx) else {
            return false;
        };
        let dup = Tab::restored(src.current_path.clone(), &tab_to_state(src));
        self.insert_after(idx, dup).is_some()
    }

    /// Closes tab `idx` and returns it. Refuses to close the last one.
    pub(super) fn close(&mut self, idx: usize) -> Option<Tab> {
        if self.tabs.len() <= 1 || idx >= self.tabs.len() {
            return None;
        }
        let closed = self.tabs.remove(idx);
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if idx < self.active {
            self.active -= 1;
        }
        Some(closed)
    }

    pub(super) fn select(&mut self, idx: usize) -> bool {
        if idx < self.tabs.len() && idx != self.active {
            self.active = idx;
            true
        } else {
            false
        }
    }

    /// Moves the tab at `from` to an **insertion position** `to_pre`
    /// computed BEFORE the removal: the user saw the cursor (and the
    /// preview line) at index position `to_pre` in the current list
    /// (so `to_pre ∈ [0, len]` — `len` = insert at the end).
    ///
    /// Internally, this is converted to a "post-remove index":
    ///   - if `to_pre > from`, the post-remove index is `to_pre - 1` (because
    ///     removing `from` shifted the elements to its right);
    ///   - otherwise, the post-remove index stays `to_pre`.
    ///
    /// Adjusts `active` so it keeps pointing at the same Tab.
    pub(super) fn move_tab(&mut self, from: usize, to_pre: usize) -> bool {
        let n = self.tabs.len();
        if from >= n {
            return false;
        }
        let to = if to_pre > from { to_pre - 1 } else { to_pre };
        let to = to.min(n - 1);
        if to == from {
            return false;
        }
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        if self.active == from {
            self.active = to;
        } else if from < self.active && to >= self.active {
            self.active -= 1;
        } else if from > self.active && to <= self.active {
            self.active += 1;
        }
        true
    }
}

// View panel ----------

/// A panel represents an independent file view: tabs, row
/// model, and selection. A global watcher tracks the active panel and rearms on
/// each navigation.
pub(super) struct Panel {
    pub(super) tabs: TabBook,
    pub(super) rows_model: Rc<VecModel<FileRow>>,
    /// Virtualized window over `rows_model`. The filter follows the
    /// technical field `FileRow.rendered`; operations keep using the
    /// full model and its stable indices.
    pub(super) rendered_rows_model: ModelRc<FileRow>,
    /// Revision of order/geometry. Unlike content notifications
    /// (selection, thumbnail...), it forces hit-tests under a motionless pointer.
    pub(super) rows_revision: Cell<i32>,
    /// Context change (folder/tab) requiring an exact return to the top.
    pub(super) viewport_reset_gen: Cell<i32>,
    /// Last exact viewport published by Slint, in content coordinates.
    pub(super) viewport_top: Cell<f32>,
    pub(super) viewport_height: Cell<f32>,
    pub(super) item_grid: Cell<Option<GridLayout>>,
    /// Half-open interval currently accepted by `rendered_rows_model`.
    pub(super) rendered_first: Cell<usize>,
    pub(super) rendered_end: Cell<usize>,
    /// Path actually loaded into `rows_model`. Lets us distinguish
    /// a refresh on the same folder (preserve selection) from a
    /// context change like a tab/panel switch (reset selection).
    pub(super) displayed_path: PathBuf,
    /// Columns specific to the panel (order, visibility, and width).
    /// Independent from other panels; persisted per panel in the
    /// workspace .toml. Always normalized (`columns::sanitize`).
    pub(super) columns: Vec<ColumnSpec>,
    /// Number of hidden entries in the displayed folder, recomputed on
    /// each listing. Feeds the "· K hidden" footer reminder when
    /// hidden entries aren't shown. Session only (not persisted).
    pub(super) hidden_count: usize,
    /// The displayed folder is UNAVAILABLE (doesn't exist / network drive not
    /// started) → "not found" banner + automatic re-check. Session.
    pub(super) unavailable: bool,
    /// Last listing failed, including access denials which use a notice.
    pub(super) listing_failed: bool,
    /// Throttles explicit recovery clicks without changing rendered models.
    pub(super) last_recovery: Option<Instant>,
    /// Current scroll of the tab bar along its MAIN AXIS
    /// (`content-x` in horizontal mode, `content-y` in vertical mode, ≤ 0),
    /// REPORTED by the view (`panel-tabs-scrolled`). Used for the hit-test of the
    /// insertion gap for a tab received from another instance. Session.
    pub(super) tabs_viewport_x: f32,
    /// Tab bar position: 0 = horizontal at the top,
    /// 1 = vertical on the left, 2 = vertical on the right. Persisted per view.
    pub(super) tab_bar_mode: u8,
    /// USER width of the vertical bar (logical px), set via the
    /// handle. `0` = automatic (clamped 40% formula). Persisted per view.
    pub(super) vbar_user_w: f32,
    /// INITIAL listing still awaited from the startup thread: the
    /// startup population is asynchronous so as not to block the display
    /// on a slow network path. Cleared by delivery or by any more
    /// recent listing (navigation, F5, watcher) so as to discard a stale result.
    pub(super) pending_initial: bool,
    /// Ordinary network listing in progress. Distinct from `pending_initial` so
    /// that a stale initial result can't win a race during an
    /// A → B → A navigation.
    pub(super) pending_listing: bool,
    /// Identifier of the network request awaited by this panel.
    pub(super) listing_gen: u64,
    /// Child to select after an asynchronous upward navigation.
    pub(super) pending_select: Option<String>,
    pub(super) search: Option<search::Session>,
    /// Non-`None` marks this view as an EDITOR view: instead of listing a
    /// folder, it shows the contents of a text file in a Slint `TextEdit`.
    /// Such a view always carries exactly one tab (no multi-tab editing), and
    /// that tab's `current_path` is the edited FILE's folder. Every other field
    /// stays valid so layout, splitting, closing and workspace persistence keep
    /// working unchanged; the listing machinery is simply never driven.
    pub(super) editor: Option<EditorTarget>,
}

/// What an editor view edits.
///
/// The path is kept apart from `Panel::displayed_path` on purpose: the latter
/// always names a directory, and conflating the two would make a restored
/// workspace try to list the text file as if it were a folder.
#[derive(Clone)]
pub(super) struct EditorTarget {
    /// Absolute path of the edited file, re-checked when a workspace restores
    /// the view.
    pub(super) file: PathBuf,
}

/// Live editing state of one editor view.
///
/// `saved` is the text as it currently exists on disk and `text` what the user
/// has typed: the difference is what "dirty" means, so returning to the saved
/// content by hand clears the marker on its own, with no extra bookkeeping.
#[derive(Default)]
pub(super) struct EditorBuffer {
    /// Current content of the edit box.
    pub(super) text: String,
    /// Content last read from or written to disk.
    pub(super) saved: String,
}

impl EditorBuffer {
    pub(super) fn is_dirty(&self) -> bool {
        self.text != self.saved
    }
}

impl Panel {
    /// New panel with an explicit tab bar position (settings
    /// default, inherited on split, instance detached via tear-off).
    pub(super) fn with_mode(
        initial: PathBuf,
        columns: Vec<ColumnSpec>,
        tab_bar_mode: u8,
        display: impl Into<TabDisplayDefaults>,
    ) -> Self {
        Self::from_tab(Tab::new(initial, display), columns, tab_bar_mode, 0.0)
    }

    /// Shared panel initialization for a new location and a tab moved by drag.
    /// The latter keeps its history and view options instead of rebuilding it.
    pub(super) fn from_tab(tab: Tab, columns: Vec<ColumnSpec>, tab_bar_mode: u8, vbar_user_w: f32) -> Self {
        Self::from_book(
            TabBook {
                tabs: vec![tab],
                active: 0,
            },
            columns,
            tab_bar_mode,
            vbar_user_w,
        )
    }

    /// Restored and newly created views share all transient model state.
    pub(super) fn from_book(
        tabs: TabBook,
        columns: Vec<ColumnSpec>,
        tab_bar_mode: u8,
        vbar_user_w: f32,
    ) -> Self {
        let initial = tabs.tabs[tabs.active].current_path.clone();
        let (rows_model, rendered_rows_model) = new_row_models();
        Self {
            tabs,
            rows_model,
            rendered_rows_model,
            rows_revision: Cell::new(0),
            viewport_reset_gen: Cell::new(0),
            viewport_top: Cell::new(0.0),
            viewport_height: Cell::new(DEFAULT_RENDER_VIEWPORT_HEIGHT),
            item_grid: Cell::new(None),
            rendered_first: Cell::new(0),
            rendered_end: Cell::new(0),
            displayed_path: initial,
            columns: columns::sanitize(columns),
            hidden_count: 0,
            unavailable: false,
            listing_failed: false,
            last_recovery: None,
            tabs_viewport_x: 0.0,
            tab_bar_mode: tab_bar_mode.min(2),
            vbar_user_w: vbar_user_w.max(0.0),
            pending_initial: false,
            pending_listing: false,
            listing_gen: 0,
            pending_select: None,
            search: None,
            editor: None,
        }
    }

    /// Constructor for an EMBEDDED EDITOR view.
    ///
    /// `file` must be an existing regular file: an editor view has no blank
    /// state, so it is only ever created from a concrete target. The view's
    /// single tab points at the file's FOLDER — that keeps every navigation
    /// helper (breadcrumbs, parent, address bar) meaningful if the view is
    /// ever turned back into a browser — while `editor.file` remembers what is
    /// actually being edited.
    pub(super) fn editor_view(
        file: PathBuf,
        columns: Vec<ColumnSpec>,
        tab_bar_mode: u8,
        display: impl Into<TabDisplayDefaults>,
    ) -> Self {
        let folder = file
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let mut panel = Self::with_mode(folder, columns, tab_bar_mode, display);
        panel.editor = Some(EditorTarget { file });
        panel
    }

    /// The file an editor view edits, or `None` for a folder view.
    pub(super) fn editor_file(&self) -> Option<&Path> {
        self.editor.as_ref().map(|target| target.file.as_path())
    }

    pub(super) fn bump_rows_revision(&self) {
        self.rows_revision
            .set(self.rows_revision.get().wrapping_add(1));
    }

    pub(super) fn from_detached(view: &ViewState, dirs: &[PathBuf]) -> Option<Self> {
        if dirs.is_empty() || dirs.len() != view.tabs.len() {
            return None;
        }
        let tabs = dirs
            .iter()
            .zip(&view.tabs)
            .map(|(path, saved)| Tab::restored(path.clone(), saved))
            .collect();
        Some(Self::from_book(
            TabBook {
                tabs,
                active: view.active_tab.min(dirs.len() - 1),
            },
            view.columns.clone(),
            view.tab_bar_mode,
            view.vbar_width,
        ))
    }

    pub(super) fn reset_rows_viewport(&self) {
        self.viewport_top.set(0.0);
        self.viewport_reset_gen
            .set(self.viewport_reset_gen.get().wrapping_add(1));
    }

    /// Replaces the logical model, marking BEFORE the notification the small
    /// interval to render around the current viewport.
    pub(super) fn replace_rows(&self, mut rows: Vec<FileRow>) {
        let (first, end) = mark_render_window(
            &mut rows,
            self.viewport_top.get(),
            self.viewport_height.get(),
            self.item_grid.get(),
        );
        self.rendered_first.set(first);
        self.rendered_end.set(end);
        self.rows_model.set_vec(rows);
        self.bump_rows_revision();
    }
}

pub(super) fn file_row_is_rendered(row: &FileRow) -> bool {
    row.rendered
}

pub(super) fn panel_to_state(panel: &Panel) -> ViewState {
    ViewState {
        // Split proportions live in the workspace layout tree.
        stretch: 1.0,
        active_tab: panel.tabs.active,
        tabs: panel.tabs.tabs.iter().map(tab_to_state).collect(),
        columns: panel.columns.clone(),
        tab_bar_mode: panel.tab_bar_mode,
        vbar_width: panel.vbar_user_w,
        // `Some` marks the view as an editor on restore; `None` keeps it a
        // folder view. The path is the FILE, not the folder its tab points at.
        editor_file: panel
            .editor_file()
            .map(|f| f.display().to_string()),
    }
}

pub(super) fn new_row_models() -> (Rc<VecModel<FileRow>>, ModelRc<FileRow>) {
    let rows = Rc::new(VecModel::<FileRow>::default());
    let rendered = FilterModel::new(rows.clone(), file_row_is_rendered as fn(&FileRow) -> bool);
    (rows, ModelRc::new(rendered))
}

/// Moves the `from` tab of panel `src` to panel `target` (at
/// position `insert_at`). If `src` becomes empty, it is **closed** (panel removed
/// and its area reclaimed by its sibling). Returns the new panel index to
/// activate, or `None` if the operation is invalid. `src` ≠ `target` required.
/// Removes view `idx` from the layout tree AND from `panels`, re-indexing the
/// active view.
///
/// Returns what was there, or `None` when there is nothing to take: the last
/// view, an index out of range, or a tree that refuses — and the `Vec` is then
/// left untouched, so the two never fall out of step.
///
/// Shared by closing a view and by tearing one off. What differs between the
/// two is what the caller does with the view it gets back, not how it leaves.
/// Turning an editor view back into a folder view when it is closed.
///
/// Closing a note used to leave a HOLE: the view disappeared and the layout
/// shuffled. The folder the note lives in is the useful thing to have in its
/// place, and it keeps the layout the user arranged, so closing an editor view
/// converts it in place.
///
/// Returns `true` when `idx` was an editor view and has been converted — the
/// caller must then not close anything. A view whose file has no folder (a
/// bare drive root, a pseudo-path) returns `false` so the ordinary close still
/// applies.
pub(super) fn close_editor_into_its_folder(window: &MainWindow, state: &AppState, idx: usize) -> bool {
    let file = {
        let panels = state.panels.borrow();
        let Some(panel) = panels.get(idx) else {
            return false;
        };
        let Some(file) = panel.editor_file().map(Path::to_path_buf) else {
            return false;
        };
        file
    };
    let Some(folder) = file.parent().map(Path::to_path_buf) else {
        return false;
    };
    if folder.as_os_str().is_empty() {
        return false;
    }
    // Any pending edit is written first, exactly as closing a view normally
    // does: the buffer is about to be dropped.
    save_editor(state, idx);
    let config = state.snapshot_config();
    let replacement = Panel::with_mode(
        folder.clone(),
        config.default_columns.clone(),
        config.default_tab_bar_mode.min(2),
        TabDisplayDefaults::from(&config),
    );
    {
        let mut panels = state.panels.borrow_mut();
        let Some(panel) = panels.get_mut(idx) else {
            return false;
        };
        *panel = replacement;
    }
    // The entry goes WITHOUT the renumbering `drop_editor_buffer` does: no panel
    // left the list, so every other index stays where it is.
    state.editor_buffers.borrow_mut().remove(&idx);
    *state.active_panel.borrow_mut() = idx;
    switch_active_panel(window, state);
    refresh_all_panels(window, state);
    state.persist_workspace();
    true
}

pub(super) fn take_view(state: &AppState, idx: usize) -> Option<Panel> {
    let mut panels = state.panels.borrow_mut();
    if panels.len() <= 1 || idx >= panels.len() {
        return None;
    }
    if !state.layout.borrow_mut().remove_view(idx) {
        return None;
    }
    let taken = panels.remove(idx);
    // `editor_buffers` is keyed by panel index, so every entry after the one
    // removed has to move down with its view — otherwise the text of a note
    // would follow the view that lands on its old index, and two views would
    // overwrite each other's file.
    drop_editor_buffer(state, idx);
    let mut active = state.active_panel.borrow_mut();
    if *active >= panels.len() {
        *active = panels.len() - 1;
    } else if idx < *active {
        *active -= 1;
    }
    Some(taken)
}


/// Whether a drop at these WINDOW coordinates landed outside the window.
///
/// The 24px margin keeps a plain overshoot of an edge from counting as a
/// tear-off, and covers most of the title bar so going a little too far up is
/// not one either.
pub(super) fn dropped_outside(window: &MainWindow, x: f32, y: f32) -> bool {
    let size = window.window().size();
    let scale = window.window().scale_factor().max(0.1);
    const MARGIN: f32 = 24.0;
    x < -MARGIN
        || y < -MARGIN
        || x > size.width as f32 / scale + MARGIN
        || y > size.height as f32 / scale + MARGIN
}

/// VIEW tear-off: the whole view leaves for a window of its own, with ALL its
/// tabs.
///
/// The new instance is launched BEFORE anything is removed here, and it carries
/// every tab in a single launch: the operation therefore succeeds whole or
/// fails whole, and no tab is ever left stranded between two windows.
///
/// Works on BOTH systems, like the tab tear-off it is modelled on: starting an
/// instance needs no cross-instance messaging, which is the part still missing
/// outside Windows. Only the placement of the new window degrades where the
/// compositor decides it.
///
/// Refused for the only view of a window, which would empty itself just to
/// reopen identical — the same guard the tab tear-off already carries.
pub(super) fn tear_off_view(state: &AppState, src: usize, at: (i32, i32)) -> bool {
    let (dirs, saved) = {
        let panels = state.panels.borrow();
        if src >= panels.len() || panels.len() <= 1 {
            return false;
        }
        let view = &panels[src];
        let dirs: Vec<_> = view
            .tabs
            .tabs
            .iter()
            .map(|tab| tab.current_path.clone())
            .collect();
        (dirs, panel_to_state(view))
    };
    if actions::spawn_detached_view(&saved, &dirs, Some(at)).is_err() {
        return false;
    }
    // Taken, not "closed": the view did not disappear, it moved. Offering to
    // reopen it here would put a copy of it beside the window it just left.
    take_view(state, src).is_some()
}

/// Tab tear-off (browser-style tear-off): opens a NEW
/// Favnyr instance on the folder of panel `src`'s `from` tab, then
/// removes that tab from here. Refuses if it's the sole tab of the sole panel
/// (otherwise the window would end up empty for a simple duplicate — Firefox
/// behavior). The new instance is launched BEFORE the removal: on failure,
/// the tab isn't lost. If the source panel becomes empty, it is closed (like
/// a cross-view move). Returns `true` if the tab was torn off.
pub(super) fn tear_off_tab(state: &AppState, src: usize, from: usize, at: Option<(i32, i32)>) -> bool {
    let (path, saved) = {
        let panels = state.panels.borrow();
        if src >= panels.len() || from >= panels[src].tabs.tabs.len() {
            return false;
        }
        // Last tab of the only panel → don't empty the window.
        if panels.len() == 1 && panels[src].tabs.tabs.len() == 1 {
            return false;
        }
        let view = &panels[src];
        let tab = &view.tabs.tabs[from];
        let mut saved = panel_to_state(view);
        saved.tabs = vec![tab_to_state(tab)];
        saved.active_tab = 0;
        (tab.current_path.clone(), saved)
    };
    if actions::spawn_detached_view(&saved, &[path], at).is_err() {
        return false;
    }
    // The removal and pruning shared with the cross-instance transfer also
    // handle re-indexing the active panel. The guard above forbids an
    // empty instance.
    let _ = remove_tab_pruning(state, src, from);
    state.persist_workspace();
    true
}

// Cross-INSTANCE tab transfer (Windows) ----------

pub(super) static SUPPRESS_WORKSPACE_PERSIST: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// True if the workspace persistence on close must be SKIPPED: instance
/// emptied by transferring ALL its tabs to another instance → otherwise we'd
/// overwrite the shared workspace with an empty state.
pub fn suppress_workspace_persist() -> bool {
    SUPPRESS_WORKSPACE_PERSIST.load(std::sync::atomic::Ordering::SeqCst)
}

/// Serializes a tab for cross-instance transfer: path + view state +
/// drop point (screen), separated by `\t` (invalid in a Windows
/// file name → unambiguous).
pub(super) fn serialize_tab(tab: &Tab, drop: (i32, i32)) -> String {
    format!(
        // Optional fields are appended after the drop point, so the two
        // directions stay compatible between instances of different versions:
        // an older receiver ignores fields it does not know, while a newer
        // receiver falls back when one is absent.
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        tab.current_path.display(),
        tab.sort.column.code(),
        u8::from(matches!(tab.sort.order, SortOrder::Asc)),
        u8::from(tab.preview),
        u8::from(tab.show_hidden),
        tab.group_mode.code(),
        drop.0,
        drop.1,
        tab.zoom,
        tab.item_arrangement.code(),
        tab.grid_zoom
            .map(|zoom| zoom.to_string())
            .unwrap_or_default(),
        tab.sort
            .secondary
            .map(|s| s.column.code())
            .unwrap_or_default(),
        u8::from(
            tab.sort
                .secondary
                .is_none_or(|s| matches!(s.order, SortOrder::Asc))
        ),
        u8::from(tab.sort.secondary.is_some_and(|s| s.group_dates_by_day)),
    )
}

/// Rebuilds a tab from a serialized payload, with the (physical) SCREEN
/// drop point if present. `None` if malformed.
pub(super) fn deserialize_tab(payload: &str) -> Option<(Tab, Option<(i32, i32)>)> {
    let p: Vec<&str> = payload.split('\t').collect();
    if p.len() < 6 || p[0].is_empty() {
        return None;
    }
    let column = SortColumn::from_code(p[1]).unwrap_or(SortColumn::Name);
    let order = if p[2] == "1" {
        SortOrder::Asc
    } else {
        SortOrder::Desc
    };
    let tab = Tab::restored(
        PathBuf::from(p[0]),
        &TabState {
            path: p[0].into(),
            sort_column: column,
            sort_order: order,
            secondary_sort: p
                .get(11)
                .and_then(|s| SortColumn::from_code(s))
                .map(|column| SecondarySort {
                    column,
                    order: if p.get(12) == Some(&"0") {
                        SortOrder::Desc
                    } else {
                        SortOrder::Asc
                    },
                    group_dates_by_day: p.get(13) == Some(&"1"),
                }),
            preview: p[3] == "1",
            zoom: p.get(8).and_then(|z| z.parse().ok()),
            show_hidden: p[4] == "1",
            group_mode: GroupMode::from_code(p[5]).unwrap_or(GroupMode::ByType),
            item_arrangement: p
                .get(9)
                .map_or(ItemArrangement::List, |s| ItemArrangement::from_code(s)),
            grid_zoom: p.get(10).and_then(|z| z.parse().ok()),
        },
    );
    let drop = match (p.get(6), p.get(7)) {
        (Some(x), Some(y)) => x.parse().ok().zip(y.parse().ok()),
        _ => None,
    };
    Some((tab, drop))
}

/// Removes the `from` tab of panel `src` and prunes the panel if it empties out.
/// Returns `true` — WITHOUT REMOVING ANYTHING — if it was the last tab of the last
/// panel (empty instance): the caller quits. We NEVER leave an empty
/// `TabBook` in the state: the event loop still processes a few events
/// before stopping, and any `with_tabs` access would panic.
pub(super) fn remove_tab_pruning(state: &AppState, src: usize, from: usize) -> bool {
    let mut panels = state.panels.borrow_mut();
    if src >= panels.len() || from >= panels[src].tabs.tabs.len() {
        return false;
    }
    if panels.len() == 1 && panels[src].tabs.tabs.len() == 1 {
        return true; // empty instance → quit (the state stays consistent to the end)
    }
    panels[src].tabs.tabs.remove(from);
    {
        let b = &mut panels[src].tabs;
        if !b.tabs.is_empty() && b.active >= b.tabs.len() {
            b.active = b.tabs.len() - 1;
        }
    }
    if panels[src].tabs.tabs.is_empty() && state.layout.borrow_mut().remove_view(src) {
        panels.remove(src);
        drop_editor_buffer(state, src);
        // Re-indexing the active panel (same rule as panel closing):
        // clamp if the active one pointed past the end, decrement if it was AFTER `src`
        // (the indices shifted by one).
        let mut a = state.active_panel.borrow_mut();
        if *a >= panels.len() {
            *a = panels.len().saturating_sub(1);
        } else if src < *a {
            *a -= 1;
        }
    }
    false
}

/// Outcome of a cross-instance tab transfer attempt.
pub(super) enum Transfer {
    /// Transferred; the instance stays open → the caller refreshes the UI.
    Moved,
    /// Transferred; the instance has EMPTIED and is closing → refresh nothing.
    Emptied,
    /// Failed (invalid target) → the caller falls back to tear-off.
    Failed,
    /// The last tab must remain available while file work is protected.
    Blocked,
}

/// Transfers the `from` tab of panel `src` to the Favnyr instance `target_hwnd`
/// (window under the cursor), then removes it from here. If the instance empties out,
/// it quits (without overwriting the shared workspace).
pub(super) fn transfer_tab_to(
    window: &MainWindow,
    state: &AppState,
    src: usize,
    from: usize,
    target_hwnd: isize,
    drop: (i32, i32),
) -> Transfer {
    let payload = {
        let panels = state.panels.borrow();
        if src >= panels.len() || from >= panels[src].tabs.tabs.len() {
            return Transfer::Failed;
        }
        if panels.len() == 1 && panels[src].tabs.tabs.len() == 1 && shutdown::pending(state) {
            std::mem::drop(panels);
            shutdown::request(window, state);
            return Transfer::Blocked;
        }
        serialize_tab(&panels[src].tabs.tabs[from], drop)
    };
    if !crate::winmsg::send(target_hwnd, &payload) {
        return Transfer::Failed;
    }
    if remove_tab_pruning(state, src, from) {
        SUPPRESS_WORKSPACE_PERSIST.store(true, std::sync::atomic::Ordering::SeqCst);
        // Recheck after the native send, which may dispatch nested messages.
        window.invoke_request_close();
        Transfer::Emptied
    } else {
        // The target now owns the tab: persist the source immediately so
        // an unexpected shutdown doesn't restore it on the next launch.
        state.persist_workspace();
        Transfer::Moved
    }
}

/// Slint coordinates (logical client) → physical screen. On Windows we
/// must go through `ClientToScreen`: `Window::position()` describes the outer
/// window, and therefore introduces the border thickness + the title bar.
pub(super) fn window_logical_to_screen(window: &MainWindow, x: f32, y: f32) -> (i32, i32) {
    let scale = window.window().scale_factor().max(0.1);
    let client = ((x * scale).round() as i32, (y * scale).round() as i32);
    #[cfg(windows)]
    if let Some(screen) = crate::winmsg::client_to_screen(client.0, client.1) {
        return screen;
    }
    let outer = window.window().position();
    (outer.x + client.0, outer.y + client.1)
}

/// Physical screen → logical Slint client coordinates. The fallback is only used
/// before HWND initialization or outside Windows.
pub(super) fn screen_to_window_logical(window: &MainWindow, x: i32, y: i32) -> (f32, f32) {
    let scale = window.window().scale_factor().max(0.1);
    #[cfg(windows)]
    if let Some((client_x, client_y)) = crate::winmsg::screen_to_client(x, y) {
        return (client_x as f32 / scale, client_y as f32 / scale);
    }
    let outer = window.window().position();
    ((x - outer.x) as f32 / scale, (y - outer.y) as f32 / scale)
}

/// Insertion point (panel, gap) for a received tab, from the screen
/// drop point `(sx, sy)` in physical pixels. The geometry relies solely on the
/// container exported by Slint, the fractional rectangles (`panel_rects`),
/// the tab widths (`tab_layout`), and the scroll reported by
/// `panel-tabs-scrolled`. `gap = None` denotes an insertion at the end of the bar.
pub(super) fn locate_tab_drop(
    window: &MainWindow,
    state: &AppState,
    sx: i32,
    sy: i32,
) -> (usize, Option<usize>) {
    let active = *state.active_panel.borrow();
    // Physical screen → logical window → panel container frame of reference.
    let (wx, wy) = screen_to_window_logical(window, sx, sy);
    let cx = wx - window.get_panels_cont_x();
    let cy = wy - window.get_panels_cont_y();
    let (cw, ch) = (window.get_panels_cont_w(), window.get_panels_cont_h());
    if cw <= 0.0 || ch <= 0.0 || !(0.0..cw).contains(&cx) || !(0.0..ch).contains(&cy) {
        return (active, None); // outside the panels area → active panel, at the end
    }
    let panels = state.panels.borrow();
    let rects = panel_rects(&current_geom(state), panels.len());
    let Some(p) = rects.iter().position(|r| {
        cx >= r.x * cw && cx < (r.x + r.w) * cw && cy >= r.y * ch && cy < (r.y + r.h) * ch
    }) else {
        return (active, None);
    };
    // Position local to the panel's RECT (the PanelComponent is inset by
    // +panel-gap=4px within its rect; the bands below absorb that).
    let lx = cx - rects[p].x * cw;
    let ly = cy - rects[p].y * ch;
    let mode = panels[p].tab_bar_mode;
    // Does the drop target the panel's TAB BAR (→ precise gap) or its
    // body (→ predictable insertion at the end)? The band depends on the mode.
    let in_bar = match mode {
        // Horizontal: gap 4 + panel padding 7 + toolbar padding 6 + strip 26,
        // + tolerance → 44 px below the TOP of the panel.
        0 => ly <= 44.0,
        // Vertical: bar column — gap 4 + panel padding 7 + clamped
        // width (shared `vbar_width` formula), + 4 px tolerance.
        _ => {
            let rect_w = rects[p].w * cw;
            let bw = vbar_width(
                rect_w - 2.0 * 4.0, /* panel-gap */
                panels[p].vbar_user_w,
            );
            match mode {
                1 => lx <= 4.0 + 7.0 + bw + 4.0,
                _ => lx >= rect_w - 4.0 - 7.0 - bw - 4.0,
            }
        }
    };
    if !in_bar {
        return (p, None);
    }
    // Gap in the bar: coordinate along the MAIN AXIS, local to the
    // start of the tab list (insets below), converted to the Flickable's
    // CONTENT frame of reference (viewport ≤ 0, reported by the view). Same rule as
    // reordering: first half of a tab → before it, second half →
    // after. Insets: horizontal = 13 px (padding 7 + toolbar 6, see
    // `tabs-row-x`); vertical = gap 4 + padding 7 + bar padding 4 + "+"
    // head 24 + spacing 4 + rule 1 + spacing 4 = 48 px (MUST == `vtabs-col-y`
    // on the Slint side, 44 px excluding panel-gap).
    let strip_pos = match mode {
        0 => lx - 13.0 - panels[p].tabs_viewport_x,
        _ => ly - 48.0 - panels[p].tabs_viewport_x,
    };
    let titles: Vec<String> = panels[p]
        .tabs
        .tabs
        .iter()
        .map(|t| tab_title(&t.current_path))
        .collect();
    let geo = tab_layout_for(mode, &titles);
    let gap = geo
        .iter()
        .position(|(w, off)| strip_pos < off + w / 2.0)
        .unwrap_or(geo.len());
    (p, Some(gap))
}

/// Receives a tab transferred from ANOTHER instance: inserts it at the drop
/// point (panel + gap in the bar, see `locate_tab_drop`) and activates it. The
/// window has already been brought to the foreground by the sender.
pub(super) fn on_tab_received(window: &MainWindow, state: &AppState, payload: &str) {
    let Some((tab, drop)) = deserialize_tab(payload) else {
        clear_external_hover(window);
        return;
    };
    // Case B: drop onto a FAVORITES folder of THIS instance. The async
    // hover may LAG BEHIND the final position → we REPLAY the
    // drop point (authoritative, carried in the payload) via
    // `set_external_hover`, then READ `fav-hover-container` PULL-BASED (the binding
    // is re-evaluated on read → reflects that exact point). Non-empty = we save it
    // as a favorite (the tab was already removed from the source by the transfer: it
    // "merges into" the favorites folder instead of attaching to a panel).
    let fav_container = match drop {
        Some((sx, sy)) => {
            set_external_hover(window, sx, sy);
            window.get_fav_hover_container().to_string()
        }
        None => String::new(),
    };
    clear_external_hover(window); // clears the insertion preview in all cases
    if !fav_container.is_empty()
        && add_paths_to_favorite(
            window,
            state,
            std::slice::from_ref(&tab.current_path),
            &fav_container,
        )
    {
        return;
    }
    let (panel, gap) = match drop {
        Some((sx, sy)) => locate_tab_drop(window, state, sx, sy),
        None => (*state.active_panel.borrow(), None),
    };
    let path = tab.current_path.clone();
    let panel = panel.min(state.panels.borrow().len().saturating_sub(1));
    {
        let mut panels = state.panels.borrow_mut();
        let book = &mut panels[panel].tabs;
        let at = gap.unwrap_or(book.tabs.len());
        book.insert_tab_at(at, tab);
    }
    *state.active_panel.borrow_mut() = panel;
    info!(path = %path.display(), panel, "tab received from another instance");
    load_directory(window, state, &path, false);
    state.persist_workspace();
}

pub(super) fn move_tab_between(
    state: &AppState,
    src: usize,
    from: usize,
    mut target: usize,
    insert_at: usize,
) -> Option<usize> {
    let mut panels = state.panels.borrow_mut();
    let n = panels.len();
    if src >= n || target >= n || src == target {
        return None;
    }
    if from >= panels[src].tabs.tabs.len() {
        return None;
    }

    // Extract the tab from the source.
    let tab = panels[src].tabs.tabs.remove(from);
    {
        let b = &mut panels[src].tabs;
        if !b.tabs.is_empty() && b.active >= b.tabs.len() {
            b.active = b.tabs.len() - 1;
        }
    }
    let src_empty = panels[src].tabs.tabs.is_empty();

    // Insert into the target.
    {
        panels[target].tabs.insert_tab_at(insert_at, tab);
    }

    // Source emptied (it was its last tab) → close the source panel.
    if src_empty && state.layout.borrow_mut().remove_view(src) {
        panels.remove(src);
        drop_editor_buffer(state, src);
        if src < target {
            target -= 1;
        }
    }
    Some(target.min(panels.len().saturating_sub(1)))
}

/// Dropping a tab on the EDGE of a panel: splits
/// `target_panel` according to `dir` and places the `from_tab` tab (moved from
/// `source_panel`) into the newly created panel. `new_first` = the new
/// panel is the first child (west/north edge). If the source panel becomes
/// empty, it is removed (merge). Returns `true` if the operation took place.
pub(super) fn split_with_tab(
    state: &AppState,
    source_panel: usize,
    from_tab: usize,
    target_panel: usize,
    dir: SplitDir,
    new_first: bool,
) -> bool {
    let mut panels = state.panels.borrow_mut();
    let n = panels.len();
    if source_panel >= n || target_panel >= n {
        return false;
    }
    if from_tab >= panels[source_panel].tabs.tabs.len() {
        return false;
    }
    // Splitting your own panel with its only tab doesn't make sense.
    if target_panel == source_panel && panels[source_panel].tabs.tabs.len() <= 1 {
        return false;
    }
    // Cap: a split creates +1 panel. But if the source then empties out
    // (last tab → closes), the net change is zero → allowed even at 16.
    let source_will_close =
        target_panel != source_panel && panels[source_panel].tabs.tabs.len() == 1;
    if n >= MAX_PANELS && !source_will_close {
        return false;
    }

    // 1. Extract the tab from the source panel.
    let tab = panels[source_panel].tabs.tabs.remove(from_tab);
    {
        let b = &mut panels[source_panel].tabs;
        if !b.tabs.is_empty() && b.active >= b.tabs.len() {
            b.active = b.tabs.len() - 1;
        }
    }
    let source_empty = panels[source_panel].tabs.tabs.is_empty();

    // 2. New panel containing the moved tab. It inherits the columns
    //    and the tab bar position of the source view (visual
    // consistency during a cross-view drag).
    let new_idx = panels.len();
    let inherited_cols = panels[source_panel].columns.clone();
    let inherited_mode = panels[source_panel].tab_bar_mode;
    let inherited_vbar = panels[source_panel].vbar_user_w;
    panels.push(Panel::from_tab(
        tab,
        inherited_cols,
        inherited_mode,
        inherited_vbar,
    ));

    // 3. Split the target leaf in the tree.
    let ok = state
        .layout
        .borrow_mut()
        .split_leaf(target_panel, dir, new_idx, 0.5, new_first);
    if !ok {
        // Rollback: remove the created panel, put the tab back into the source.
        if let Some(p) = panels.pop()
            && let Some(t) = p.tabs.tabs.into_iter().next()
        {
            let dst = &mut panels[source_panel].tabs;
            let at = from_tab.min(dst.tabs.len());
            dst.tabs.insert(at, t);
        }
        return false;
    }

    // 4. Source emptied → remove the source panel (merge).
    let mut final_active = new_idx;
    if source_empty && state.layout.borrow_mut().remove_view(source_panel) {
        panels.remove(source_panel);
        drop_editor_buffer(state, source_panel);
        if source_panel < final_active {
            final_active -= 1;
        }
    }

    *state.active_panel.borrow_mut() = final_active.min(panels.len().saturating_sub(1));
    true
}

/// Splits a target view with a newly opened directory. Unlike
/// `split_with_tab`, no source view is consumed: the panel count always grows
/// by one, and the new view inherits the target chrome.
pub(super) fn split_with_path(
    state: &AppState,
    path: PathBuf,
    target_panel: usize,
    dir: SplitDir,
    new_first: bool,
) -> bool {
    let default_display = TabDisplayDefaults::from(&*state.config.borrow());
    let mut panels = state.panels.borrow_mut();
    if target_panel >= panels.len() || panels.len() >= MAX_PANELS {
        return false;
    }

    let new_idx = panels.len();
    let inherited_cols = panels[target_panel].columns.clone();
    let inherited_mode = panels[target_panel].tab_bar_mode;
    let inherited_vbar = panels[target_panel].vbar_user_w;
    let panel = Panel::from_tab(
        Tab::new(path, default_display),
        inherited_cols,
        inherited_mode,
        inherited_vbar,
    );
    if !state
        .layout
        .borrow_mut()
        .split_leaf(target_panel, dir, new_idx, 0.5, new_first)
    {
        return false;
    }
    panels.push(panel);
    *state.active_panel.borrow_mut() = new_idx;
    true
}

/// Builds `(panels, stretches, active_panel)` from a `WorkspaceState`.
/// Shared by `from_workspace` (creation) and `replace_with_workspace`
/// (in-place loading). Validates the paths (fallback `$HOME`).
pub(super) fn build_panels(ws: WorkspaceState) -> (Vec<Panel>, LayoutNode, usize) {
    let ws = ws.sanitized();
    // `sanitized()` guarantees a valid tree covering exactly the panels.
    let layout = ws.layout_tree();
    let mut panels = Vec::with_capacity(ws.panels.len());
    for ps in &ws.panels {
        let mut tabs = Vec::with_capacity(ps.tabs.len());
        for ts in &ps.tabs {
            let path = resolve_restored_path(&ts.path);
            tabs.push(Tab::restored(path, ts));
        }
        // `tabs` guaranteed non-empty (sanitized removes empty panels).
        let active = ps.active_tab.min(tabs.len() - 1);
        let mut panel = Panel::from_book(
            TabBook { tabs, active },
            ps.columns.clone(),
            ps.tab_bar_mode,
            ps.vbar_width,
        );
        // A view that was editing a file comes back as an editor view. The
        // tabs above already carry the file's FOLDER, which is what keeps every
        // navigation helper meaningful if the file has since disappeared; the
        // file itself is remembered separately so the buffer can be reloaded.
        //
        // Whether the file still exists is decided later, by
        // `reload_editor_buffers`, which is the only place that can turn a
        // missing file into the "not found" banner.
        if let Some(file) = &ps.editor_file {
            panel.editor = Some(EditorTarget {
                file: PathBuf::from(file),
            });
        }
        panels.push(panel);
    }
    let active_panel = ws.active_panel.min(panels.len() - 1);
    (panels, layout, active_panel)
}

/// Resolves a path restored from the workspace: we KEEP the path as-is
/// (even if it isn't accessible — network drive not started, mount missing).
/// Only an empty path falls back to `$HOME`. The inaccessible tab shows a
/// "not found" banner via `refresh_listing` and refreshes automatically
/// as soon as the folder becomes accessible again.
pub(super) fn resolve_restored_path(raw: &str) -> PathBuf {
    if raw.is_empty() {
        home_dir()
    } else {
        PathBuf::from(raw)
    }
}
