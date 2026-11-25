//! Settings: the values a preferences pane shows, and how they are pushed.
//!
//! Three surfaces meet here. Column widths and order are read by the header
//! row and by the settings list, so they are computed once and shared. The UI
//! scale is a preset list plus a nearest-match rule, because the stored factor
//! is free-form and has to survive hand-editing. Shortcuts are a keymap
//! rendered into per-language captions, and they show the binding that
//! actually applies rather than the one that was configured -- an override
//! that no longer matches anything would otherwise look like it had.
//!
//! Almost nothing here owns state: these functions read AppState and push the
//! result, so the pane stays a projection of what is stored. The single
//! writer, `apply_shortcut_override`, only records the choice and lets the
//! next push pick it up.

use super::*;

/// Width (px) of a column by its id, or the default width.
pub(super) fn col_width(cols: &[ColumnSpec], id: &str) -> f32 {
    cols.iter()
        .find(|c| c.id == id)
        .map(|c| c.width)
        .unwrap_or_else(|| columns::default_width(id))
}

/// Applies a width to column `id` (if present).
pub(super) fn set_col_width(cols: &mut [ColumnSpec], id: &str, w: f32) {
    if let Some(c) = cols.iter_mut().find(|c| c.id == id) {
        c.width = w;
    }
}

/// Reorders column `id` based on a **horizontal move** `delta_px`
/// (signed). Computes the new slot from the widths of the
/// visible columns (the `name` anchor stays first). Hidden columns are
/// kept (pushed to the end of the list).
pub(super) fn reorder_column_by_delta(cols: &mut Vec<ColumnSpec>, id: &str, delta_px: f32) {
    if id == "name" {
        return;
    }
    // Visible columns (id, effective width) in order.
    let vis: Vec<(String, f32)> = cols
        .iter()
        .filter(|c| c.visible)
        .map(|c| {
            let w = if c.width > 0.0 {
                c.width
            } else {
                columns::default_width(&c.id)
            };
            (c.id.clone(), w)
        })
        .collect();
    let Some(cur) = vis.iter().position(|(i, _)| i == id) else {
        return;
    };
    // Includes the inter-column gap (= `Tokens.col-gap`) to match the
    // offsets/preview on the GUI side.
    const COL_GAP: f32 = 8.0;
    // Current center of the dragged column + movement = target center.
    let left_before: f32 = vis[..cur].iter().map(|(_, w)| w + COL_GAP).sum();
    let new_center = left_before + vis[cur].1 / 2.0 + delta_px;

    // Target "gap" in the STATIC layout (columns in place, like the
    // preview): the 1st gap whose column midpoint is past the target center.
    let mut gap = vis.len();
    let mut x = 0.0_f32;
    for (k, (_, w)) in vis.iter().enumerate() {
        if new_center < x + w / 2.0 {
            gap = k;
            break;
        }
        x += w + COL_GAP;
    }
    let gap = gap.max(1); // ≥ 1 → always after the `name` anchor

    // Removes the dragged column then adjusts the gap, whose index shifts when
    // the source was before the target.
    let mut order: Vec<String> = vis.iter().map(|(i, _)| i.clone()).collect();
    order.remove(cur);
    let t = if gap > cur { gap - 1 } else { gap };
    let t = t.clamp(1, order.len());
    order.insert(t, id.to_string());

    // Rebuilds: visible columns in the new order, then the hidden ones.
    let mut new: Vec<ColumnSpec> = Vec::with_capacity(cols.len());
    for oid in &order {
        if let Some(spec) = cols.iter().find(|c| &c.id == oid) {
            new.push(spec.clone());
        }
    }
    for c in cols.iter().filter(|c| !c.visible) {
        new.push(c.clone());
    }
    *cols = columns::sanitize(new);
}

/// Pushes the DEFAULT columns (config) to the Settings panel.
pub(super) fn push_settings_columns(window: &MainWindow, lang: Lang, cols: &[ColumnSpec]) {
    let infos: Vec<ColumnInfo> = columns::sanitize(cols.to_vec())
        .iter()
        .map(|c| column_info_explicit(lang, c, 0.0))
        .collect();
    window.set_settings_columns(ModelRc::new(VecModel::from(infos)));
}

/// [`column_info`] with the wording used wherever a column is CHOSEN from a
/// list — the Settings checkboxes and the header's context menu. Both name the
/// columns reserved for images outright ("Resolution (image)"), because a list
/// of column names read out of context gives no clue what "Depth" measures.
///
/// The header itself keeps the short form from [`column_info`]: it sits in a
/// resizable width the user may have narrowed, and there the surrounding
/// values say what the column holds.
pub(super) fn column_info_explicit(lang: Lang, c: &ColumnSpec, offset: f32) -> ColumnInfo {
    let mut info = column_info(lang, c, offset);
    // The i18n keys keep their `settings_` prefix: the wording is the same in
    // both lists, and renaming them across five catalogues would buy nothing.
    match c.id.as_str() {
        "resolution" => info.label = i18n::tr(lang, "settings_col_resolution").into(),
        "depth" => info.label = i18n::tr(lang, "settings_col_depth").into(),
        _ => {}
    }
    info
}

/// Builds a `ColumnInfo` (id + i18n label + visible + offset) for the GUI.
pub(super) fn column_info(lang: Lang, c: &ColumnSpec, offset: f32) -> ColumnInfo {
    let label = match c.id.as_str() {
        "name" => i18n::tr(lang, "col_name").into(),
        "path" => i18n::tr(lang, "col_path").into(),
        "size" => i18n::tr(lang, "col_size").into(),
        "modified" => i18n::tr(lang, "col_modified").into(),
        "created" => i18n::tr(lang, "col_created").into(),
        "age" => i18n::tr(lang, "col_age").into(),
        "ext" => i18n::tr(lang, "col_ext").into(),
        "resolution" => i18n::tr(lang, "col_resolution").into(),
        "depth" => i18n::tr(lang, "col_depth").into(),
        other => other.into(),
    };
    ColumnInfo {
        id: c.id.clone().into(),
        label,
        visible: c.visible,
        offset,
    }
}

// ---------- UI zoom (global setting) ----------

/// Proposed UI zoom factors (× the screen scale). `1.0` = the OS's
/// native scale. Startup default = the index of `1.0`.
pub(super) const UI_SCALE_PRESETS: [f32; 6] = [0.8, 0.9, 1.0, 1.1, 1.25, 1.5];

/// Picker labels ("80%", "100%", …) — language-independent.
pub(super) fn ui_scale_labels() -> Vec<SharedString> {
    UI_SCALE_PRESETS
        .iter()
        .map(|f| format!("{}%", (f * 100.0).round() as i32).into())
        .collect()
}

/// Index of the preset closest to a stored factor (robust to a config
/// value outside the list).
pub(super) fn ui_scale_nearest_index(factor: f32) -> i32 {
    UI_SCALE_PRESETS
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| (**a - factor).abs().total_cmp(&(**b - factor).abs()))
        .map(|(i, _)| i as i32)
        .unwrap_or(2)
}

/// Applies the UI zoom: (SCREEN scale captured once) × `factor`, clamped, via
/// the public `dispatch_event(ScaleFactorChanged)` API → Slint re-scales and
/// re-lays-out the whole UI (fonts, margins, icons). Doesn't redispatch if
/// the scale is already correct (avoids an unnecessary relayout). The raw OS scale is
/// remembered on the 1st call so it stays the reference even after our own zooms.
pub(super) fn apply_ui_scale(window: &MainWindow, state: &AppState, factor: f32) {
    let base = state.ui_base_scale.get();
    let base = if base > 0.0 {
        base
    } else {
        let os = window.window().scale_factor().max(0.1);
        state.ui_base_scale.set(os);
        os
    };
    let target = (base * factor).clamp(0.5, 4.0);
    let win = window.window();
    if (win.scale_factor() - target).abs() <= 0.001 {
        return;
    }
    win.dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged {
        scale_factor: target,
    });
    // `ScaleFactorChanged` only SETS the factor: it realigns neither the
    // geometry nor the rendering. Outside fullscreen, a spontaneous `Resized` follows and
    // everything realigns; MAXIMIZED/fullscreen, the physical size is locked by
    // the OS → no `Resized` → the zoom used to only apply after un-maximizing.
    // So we force a `Resized` at the SAME physical size (logical = physical /
    // scale): Slint recomputes the layout and redraws at the new scale WITHOUT
    // resizing the OS window (the event only affects Slint's internal state).
    let phys = win.size();
    if phys.width > 0 && phys.height > 0 {
        win.dispatch_event(slint::platform::WindowEvent::Resized {
            size: slint::LogicalSize::new(
                (phys.width as f32 / target).max(1.0),
                (phys.height as f32 / target).max(1.0),
            ),
        });
    }
}

// Configurable shortcuts ----------

/// Display label of a canonical key (arrows handled separately via `kind`).
///
/// The canonical name is the STORAGE form and never changes; only what the
/// user reads does. It matters beyond politeness: a German keyboard prints
/// "Strg" where an English one prints "Ctrl", so a hardcoded label would name
/// a key that is not on the reader's keyboard.
pub(super) fn cap_label(lang: Lang, key: &str) -> String {
    match key {
        // Glyphs printed identically on every keyboard.
        "Backslash" => "\\".to_string(),
        "Comma" => ",".to_string(),
        "PageUp" => i18n::tr(lang, "key_pageup"),
        "PageDown" => i18n::tr(lang, "key_pagedown"),
        other => other.to_string(),
    }
}

/// Display "caps" of a serialized chord (empty if unassigned/unreadable).
pub(super) fn chord_to_caps(lang: Lang, chord: &str) -> Vec<ShortcutCap> {
    let mut caps = Vec::new();
    let Some(c) = Chord::parse(chord) else {
        return caps;
    };
    if c.ctrl {
        caps.push(ShortcutCap {
            label: i18n::tr(lang, "key_ctrl").into(),
            kind: 0,
        });
    }
    if c.alt {
        caps.push(ShortcutCap {
            label: i18n::tr(lang, "key_alt").into(),
            kind: 0,
        });
    }
    if c.shift {
        caps.push(ShortcutCap {
            label: i18n::tr(lang, "key_shift").into(),
            kind: 0,
        });
    }
    let (label, kind) = match c.key.as_str() {
        "ArrowUp" => (String::new(), 1),
        "ArrowDown" => (String::new(), 2),
        "ArrowLeft" => (String::new(), 3),
        "ArrowRight" => (String::new(), 4),
        k => (cap_label(lang, k), 0),
    };
    caps.push(ShortcutCap {
        label: label.into(),
        kind,
    });
    caps
}

/// Builds the shortcut groups (filtered by the current search) +
/// a boolean "at least one override exists" (enables "Reset all").
pub(super) fn build_shortcut_groups(state: &AppState) -> (Vec<ShortcutGroup>, bool) {
    let lang = state.config.borrow().language;
    let overrides = state.config.borrow().shortcut_overrides.clone();
    let has_overrides = !overrides.is_empty();
    let km = state.keymap.borrow();
    let filter = state.shortcut_filter.borrow().to_lowercase();

    let mut groups: Vec<ShortcutGroup> = Vec::new();
    let mut cur_code: Option<&'static str> = None;
    let mut cur_name = String::new();
    let mut cur_rows: Vec<ShortcutRow> = Vec::new();
    for a in shortcuts::ACTIONS {
        let code = a.group.code();
        if cur_code != Some(code) {
            if !cur_rows.is_empty() {
                groups.push(ShortcutGroup {
                    name: cur_name.as_str().into(),
                    rows: ModelRc::new(VecModel::from(std::mem::take(&mut cur_rows))),
                });
            }
            cur_code = Some(code);
            cur_name = i18n::shortcut_group_name(lang, code);
        }
        let name = i18n::shortcut_action_name(lang, a.id);
        let chord = km.chord_of(a.id);
        if !filter.is_empty()
            && !name.to_lowercase().contains(&filter)
            && !chord.to_lowercase().contains(&filter)
        {
            continue;
        }
        cur_rows.push(ShortcutRow {
            action_id: a.id.into(),
            name: name.into(),
            caps: ModelRc::new(VecModel::from(chord_to_caps(lang, chord))),
            assigned: !chord.is_empty(),
            overridden: overrides.contains_key(a.id),
        });
    }
    if !cur_rows.is_empty() {
        groups.push(ShortcutGroup {
            name: cur_name.as_str().into(),
            rows: ModelRc::new(VecModel::from(cur_rows)),
        });
    }
    (groups, has_overrides)
}

/// Pushes the shortcut list + the overrides state to the UI.
pub(super) fn push_shortcuts_ui(window: &MainWindow, state: &AppState) {
    let (groups, has_overrides) = build_shortcut_groups(state);
    window.set_shortcut_groups(ModelRc::new(VecModel::from(groups)));
    window.set_shortcut_has_overrides(has_overrides);
    // Context menus reflect the SAME effective map (dynamic).
    push_menu_shortcuts(window, state);
}

/// Renders a serialized chord ("Ctrl+Shift+N") into a SHORT label for a context
/// menu. Empty if the chord is empty (unassigned action → no shortcut
/// displayed). The serialized form is the storage one and stays canonical;
/// what is rendered uses each keyboard's own key names, so the same chord
/// reads "Ctrl+Shift+N" in English and "Strg+Umschalt+N" in German.
pub(super) fn chord_display(lang: Lang, chord: &str) -> String {
    let Some(c) = shortcuts::Chord::parse(chord) else {
        return String::new();
    };
    let mut s = String::new();
    if c.ctrl {
        s.push_str(&i18n::tr(lang, "key_ctrl"));
        s.push('+');
    }
    if c.alt {
        s.push_str(&i18n::tr(lang, "key_alt"));
        s.push('+');
    }
    if c.shift {
        s.push_str(&i18n::tr(lang, "key_shift"));
        s.push('+');
    }
    // Arrows and punctuation are drawn the same on every keyboard; the named
    // keys come from the catalogue.
    s.push_str(&match c.key.as_str() {
        "Delete" => i18n::tr(lang, "key_delete"),
        "Backslash" => "\\".to_string(),
        "Comma" => ",".to_string(),
        "ArrowUp" => "↑".to_string(),
        "ArrowDown" => "↓".to_string(),
        "ArrowLeft" => "←".to_string(),
        "ArrowRight" => "→".to_string(),
        "PageUp" => i18n::tr(lang, "key_pageup"),
        "PageDown" => i18n::tr(lang, "key_pagedown"),
        k => k.to_string(),
    });
    s
}

/// Pushes the EFFECTIVE shortcut labels displayed in the menus and the
/// rail tooltips — recomputed on every map change (rebind,
/// reset, unassignment) via `push_shortcuts_ui`.
pub(super) fn push_menu_shortcuts(window: &MainWindow, state: &AppState) {
    let lang = state.snapshot_config().language;
    let km = state.keymap.borrow();
    let d = |id: &str| SharedString::from(chord_display(lang, km.chord_of(id)));
    let panel_key = chord_display(lang, km.chord_of("open-workspaces"));
    let panel_title = i18n::tr(lang, "settings_ws_panel_label");
    window.set_ws_panel_settings_title(if panel_key.is_empty() {
        panel_title.into()
    } else {
        format!("{panel_title} ({panel_key})").into()
    });
    let shortcut_title = i18n::tr(lang, "settings_ws_shortcut_label");
    let shortcut_keys = workspace_slot_shortcuts_label(lang, &km);
    window.set_ws_shortcut_settings_title(format!("{shortcut_title} ({shortcut_keys})").into());
    window.set_menu_shortcuts(MenuShortcuts {
        open: d("open"),
        terminal: d("terminal"),
        copy: d("copy"),
        cut: d("cut"),
        paste: d("paste"),
        rename: d("rename"),
        delete: d("delete"),
        properties: d("properties"),
        new_folder: d("new-folder"),
        new_file: d("new-file"),
        split_side: d("split-side"),
        split_stack: d("split-stack"),
        equalize: d("equalize-views"),
        tab_reopen_closed: d("tab-reopen-closed"),
        open_settings: d("open-settings"),
        open_workspaces: d("open-workspaces"),
    });
}

pub(super) fn workspace_slot_shortcuts_label(lang: Lang, km: &shortcuts::Keymap) -> String {
    let none = i18n::tr(lang, "ws_slot_none");
    let display = |slot| {
        let chord = chord_display(lang, km.chord_of(&format!("load-workspace-{slot}")));
        if chord.is_empty() {
            none.clone()
        } else {
            chord
        }
    };
    let first = display(1);
    let second = display(2);
    i18n::tr(lang, "settings_ws_shortcut_preview")
        .replace("{first}", &first)
        .replace("{second}", &second)
}

/// Applies an override (or removes it if it equals the default) + rebuilds the map.
pub(super) fn apply_shortcut_override(state: &AppState, action_id: &str, chord: &str) {
    let default = shortcuts::ACTIONS
        .iter()
        .find(|a| a.id == action_id)
        .map(|a| a.default)
        .unwrap_or("");
    let id = action_id.to_string();
    let chord = chord.to_string();
    state.persist_config(|c| {
        if chord == default {
            c.shortcut_overrides.remove(&id);
        } else {
            c.shortcut_overrides.insert(id.clone(), chord.clone());
        }
    });
    state.rebuild_keymap();
}

/// The nine categories, in the order the "add" dropdown lists them.
///
/// Its own list rather than "whatever the enum declares": a `FileKind` value is
/// persisted in rows and must stay stable, while the dropdown can be reordered
/// freely — as long as the index the UI sends back is resolved through here.
pub(super) const KIND_DROPDOWN: [favnyr_core::fs::FileKind; 9] = [
    favnyr_core::fs::FileKind::Folder,
    favnyr_core::fs::FileKind::Application,
    favnyr_core::fs::FileKind::Archive,
    favnyr_core::fs::FileKind::Image,
    favnyr_core::fs::FileKind::Video,
    favnyr_core::fs::FileKind::Audio,
    favnyr_core::fs::FileKind::Document,
    favnyr_core::fs::FileKind::Config,
    favnyr_core::fs::FileKind::File,
];

/// Pushes the "Type group order" page: the rows, the dropdown and the button
/// states. Called at startup, on a language change, and after every edit.
pub(super) fn push_type_group_ui(window: &MainWindow, state: &AppState) {
    let cfg = state.snapshot_config();
    let lang = cfg.language;
    let order = cfg.type_group_order();
    let rows: Vec<crate::TypeGroupRow> = order
        .rules()
        .iter()
        .enumerate()
        .map(|(index, rule)| crate::TypeGroupRow {
            number: index as i32 + 1,
            label: match rule {
                GroupRule::Kind(kind) => i18n::kind_name(lang, *kind).into(),
                GroupRule::Exts(exts) => exts.join(", ").into(),
            },
        })
        .collect();
    window.set_type_group_rows(ModelRc::new(VecModel::from(rows)));
    window.set_type_group_kind_options(ModelRc::new(VecModel::from(
        KIND_DROPDOWN
            .iter()
            .map(|kind| SharedString::from(i18n::kind_name(lang, *kind)))
            .collect::<Vec<_>>(),
    )));
    window.set_type_group_full(order.len() >= favnyr_core::type_groups::MAX_RULES);
}

/// Stores the order and makes the open views reflect it.
///
/// `Some(...)` even when empty: clearing the list is a decision that has to
/// survive a restart, so it is written as an explicit `[]` rather than dropped.
pub(super) fn apply_type_group_order(window: &MainWindow, state: &AppState, order: &TypeGroupOrder) {
    let tokens = order.to_tokens();
    state.persist_config(|c| c.type_group_order = Some(tokens));
    set_current_type_order(order.clone());
    push_type_group_ui(window, state);
    crate::bridge::refresh_all_panels(window, state);
}

/// The list as it stands, edits applied to a copy of the effective order.
pub(super) fn current_order_tokens(state: &AppState) -> Vec<String> {
    state.snapshot_config().type_group_order().to_tokens()
}

pub(super) fn apply_language(window: &MainWindow, lang: Lang) {
    i18n::apply_strings(window, lang);
    // Use Windows' Simplified Chinese UI font directly instead of relying on
    // a fallback chosen for the system locale. No font data is bundled.
    window.set_interface_font_family(
        if cfg!(windows) && lang == Lang::Zh {
            "Microsoft YaHei UI"
        } else {
            ""
        }
        .into(),
    );

    let lang_labels: Vec<SharedString> = i18n::language_labels();
    let lang_idx = Lang::all().iter().position(|l| *l == lang).unwrap_or(0) as i32;
    window.set_language_labels(ModelRc::new(VecModel::from(lang_labels)));
    window.set_language_index(lang_idx);

    let theme_labels: Vec<SharedString> = i18n::theme_labels(lang);
    window.set_theme_labels(ModelRc::new(VecModel::from(theme_labels)));

    // "Default tab bar position" setting: same
    // labels as the per-view menu.
    let tabbar_labels: Vec<SharedString> = i18n::tabbar_labels(lang);
    window.set_tabbar_default_labels(ModelRc::new(VecModel::from(tabbar_labels)));

    // The per-panel footer is recomputed by update_panels_ui (called after
    // a language change from the on_language_changed callback).
}
