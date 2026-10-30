//! User configuration — TOML format, persisted in
//! `$XDG_CONFIG_HOME/favnyr/config.toml`.
//!
//! It groups the application's global preferences: language, theme,
//! window dimensions, view behavior, and keyboard shortcuts.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::i18n::{Lang, Theme};
use crate::workspace::ItemArrangement;

/// Default logical width of the main window (px).
pub const DEFAULT_WINDOW_WIDTH: u32 = 1100;
/// Default logical height of the main window (px).
pub const DEFAULT_WINDOW_HEIGHT: u32 = 700;
pub const DEFAULT_THUMBNAIL_ZOOM: i32 = 2;
pub const MIN_THUMBNAIL_ZOOM: i32 = 1;
pub const MAX_THUMBNAIL_ZOOM: i32 = 8;

fn default_thumbnail_zoom() -> i32 {
    DEFAULT_THUMBNAIL_ZOOM
}

/// How a workspace load handles changes in the current workspace.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceLoadPolicy {
    Discard,
    Save,
    #[default]
    #[serde(other)]
    Ask,
}

impl WorkspaceLoadPolicy {
    pub fn from_index(index: i32) -> Self {
        match index {
            1 => Self::Discard,
            2 => Self::Save,
            _ => Self::Ask,
        }
    }

    pub fn index(self) -> i32 {
        match self {
            Self::Ask => 0,
            Self::Discard => 1,
            Self::Save => 2,
        }
    }
}

/// Stable identity of a reorderable sidebar section.
///
/// The order is a global application preference: the serialized names
/// stay readable in `config.toml` and the compact codes are shared with
/// the Slint UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum SidebarSection {
    Drives = 0,
    Shortcuts = 1,
    Favorites = 2,
    Network = 3,
    /// The address tree of the active view. Added last, hence the highest
    /// discriminant: the codes are shared with the Slint UI, so existing values
    /// must never move.
    Path = 4,
}

impl SidebarSection {
    pub const COUNT: usize = 5;
    pub const DEFAULT_ORDER: [Self; Self::COUNT] = [
        Self::Path,
        Self::Favorites,
        Self::Drives,
        Self::Shortcuts,
        Self::Network,
    ];

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn from_index(index: usize) -> Option<Self> {
        match index {
            0 => Some(Self::Drives),
            1 => Some(Self::Shortcuts),
            2 => Some(Self::Favorites),
            3 => Some(Self::Network),
            4 => Some(Self::Path),
            _ => None,
        }
    }

    /// Repairs an order that is short, empty or full of duplicates (a config
    /// written by an older build, or edited by hand).
    ///
    /// The list became a `Vec` when `Path` was added: the field used to be a
    /// fixed-size array of four, so an older `config.toml` simply has four
    /// entries and would fail to deserialize into anything longer.
    pub fn sanitized_order(order: &[Self]) -> Vec<Self> {
        // Whether the saved config already knew about `Path` decides what
        // happens below: a missing entry is a migration, a present one is the
        // user's own placement and must be kept exactly as it is.
        let knew_path = order.contains(&Self::Path);

        let mut normalized: Vec<Self> = Vec::with_capacity(Self::COUNT);
        for section in order {
            if !normalized.contains(section) {
                normalized.push(*section);
            }
        }
        // Fill in whatever is missing, in the default order.
        for section in Self::DEFAULT_ORDER {
            if !normalized.contains(&section) {
                normalized.push(section);
            }
        }
        normalized.truncate(Self::COUNT);

        // A config predating `Path` just had it appended at the bottom. Put it
        // at the top instead, which is where it belongs.
        if !knew_path
            && let Some(from) = normalized.iter().position(|s| *s == Self::Path)
        {
            normalized.remove(from);
            normalized.insert(0, Self::Path);
        }
        normalized
    }

    /// Moves `section` to its final index in the global order.
    /// Returns `true` only if the order actually changed.
    pub fn reorder(order: &mut Vec<Self>, section: Self, target_index: usize) -> bool {
        *order = Self::sanitized_order(order);
        let Some(source_index) = order.iter().position(|item| *item == section) else {
            return false;
        };
        let target_index = target_index.min(Self::COUNT - 1);
        if source_index == target_index {
            return false;
        }
        let moved = order.remove(source_index);
        order.insert(target_index, moved);
        true
    }
}

fn default_sidebar_section_order() -> Vec<SidebarSection> {
    SidebarSection::DEFAULT_ORDER.to_vec()
}

fn sidebar_section_order_is_default(order: &[SidebarSection]) -> bool {
    order == SidebarSection::DEFAULT_ORDER
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub language: Lang,
    pub theme: Theme,
    pub window_width: u32,
    pub window_height: u32,
    /// Asks for confirmation before permanent deletion (bypassing the trash).
    pub confirm_permanent_delete: bool,
    /// Logical width (px) of the sidebar.
    ///
    /// There is deliberately no field for the sidebar's OPEN state: it always
    /// starts open. Collapsing it is a momentary gesture rather than a
    /// preference, so remembering it would mean one collapse disables the
    /// sidebar for every later launch. A `left_panel` key in an older config file
    /// is simply ignored (the struct tolerates unknown keys).
    pub sidebar_width: u32,
    /// Global order of the sidebar sections. Unlike their collapsed
    /// state, this order follows the user when they switch workspace.
    #[serde(
        default = "default_sidebar_section_order",
        skip_serializing_if = "sidebar_section_order_is_default"
    )]
    pub sidebar_section_order: Vec<SidebarSection>,
    /// Favnyr-side redirects for standard sidebar shortcuts renamed from
    /// inside the application. Keys remain the OS-provided original paths, so
    /// a later OS-side location change naturally supersedes stale entries.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub sidebar_shortcut_redirects: std::collections::BTreeMap<String, String>,
    /// Default columns — applied on 1st launch and on "Reset".
    #[serde(default = "crate::columns::default_columns")]
    pub default_columns: Vec<crate::columns::ColumnSpec>,
    /// Keyboard shortcut overrides: action_id → chord (`""` = not
    /// assigned). Actions not present use their default (`shortcuts::ACTIONS`).
    #[serde(default)]
    pub shortcut_overrides: std::collections::BTreeMap<String, String>,
    /// Keyboard layout assumed for letter shortcuts, empty for none.
    ///
    /// A choice and not a switch: several Cyrillic arrangements exist and they
    /// place the same letter on different keys, so one of them has to be
    /// named. Stored as its identifier rather than as a number, so the file
    /// stays readable and reordering the list never changes what it means.
    #[serde(default)]
    pub letter_shortcut_layout: String,
    /// Whether the one-off hint about `letter_shortcut_layout` was shown.
    ///
    /// The hint appears when a letter shortcut goes unanswered on a non-Latin
    /// keyboard, which is the only moment it helps. Showing it a second time
    /// would be nagging, so the fact is kept rather than the question.
    #[serde(default)]
    pub letter_layout_notice_seen: bool,
    /// Depth for computing folders' **recursive** modification date.
    /// `0` = disabled (folder's own mtime, default); `N > 0` = show the
    /// most recent date among descendants up to `N` levels. Bounded →
    /// controlled cost. Computed in the background (never freezes the UI).
    #[serde(default)]
    pub recursive_mtime_depth: i32,
    /// Depth for computing folders' **recursive size** (sum of descendant
    /// files' bytes). `0` = disabled (default, folders show no size); `N > 0` =
    /// sum down to `N` levels. Bounded → controlled cost. Computed in the
    /// background, sharing ONE directory walk with the recursive mtime above.
    #[serde(default)]
    pub recursive_size_depth: i32,
    /// Enables the dedicated font size used for every value displayed in file
    /// views. Other interface text continues to follow the regular UI scale.
    #[serde(default)]
    pub view_text_font_size_override: bool,
    /// Logical font size for view values when the override is enabled. Kept
    /// while disabled so re-enabling restores the user's choice.
    #[serde(default = "default_view_text_font_size")]
    pub view_text_font_size: i32,
    /// Extra vertical space between entries in file views. This expands only
    /// the row band used by scrolling and hit-testing, not its visual content.
    #[serde(default)]
    pub view_item_spacing: i32,
    /// DEFAULT tab bar position for views created "from
    /// scratch" (new workspace, reset): 0 = top · 1 = left · 2 =
    /// right. Existing views keep their PER-VIEW setting (workspace);
    /// a split inherits from the view it split off, not this default.
    #[serde(default)]
    pub default_tab_bar_mode: u8,
    /// Display mode used by newly created tabs and views. New installations
    /// start in Previews; an absent field in an existing config keeps List.
    /// Restored, duplicated, and transferred tabs keep their own per-tab mode.
    #[serde(default)]
    pub default_preview_mode: bool,
    /// Size level used for new tabs and when thumbnails are explicitly enabled.
    #[serde(default = "default_thumbnail_zoom")]
    pub default_thumbnail_zoom: i32,
    /// Arrangement for new tabs. Missing preferences preserve the list layout.
    #[serde(default)]
    pub default_item_arrangement: ItemArrangement,
    /// Whether newly created tabs and views start with hidden entries shown.
    /// Off when the field is absent, so an existing configuration keeps the
    /// historical behaviour. Restored, duplicated and transferred tabs keep
    /// their own per-tab setting.
    #[serde(default)]
    pub default_show_hidden: bool,
    /// Shows the full path as a tooltip when hovering a tab. `true` by default
    /// (`default = true` → earlier configs without the field keep it on too);
    /// the user can uncheck it.
    #[serde(default = "default_true")]
    pub tab_path_tooltip: bool,
    /// Shows the full item name when hovering its caption in grid view.
    #[serde(default = "default_true")]
    pub grid_name_tooltip: bool,
    /// Shows the result path when hovering an item in subfolder search.
    #[serde(default = "default_true")]
    pub search_path_tooltip: bool,
    /// Legacy manual-load preference. Used only when migrating older configs.
    #[serde(default, skip_serializing, rename = "warn_unsaved_workspace")]
    pub legacy_warn_unsaved_workspace: Option<bool>,
    /// Policy for loading or starting a workspace from the panel.
    #[serde(default)]
    pub workspace_panel_save: Option<WorkspaceLoadPolicy>,
    /// Independent policy for numbered workspace shortcuts.
    #[serde(default)]
    pub workspace_shortcut_save: WorkspaceLoadPolicy,
    /// In Preview mode, keeps the enlarged row height only for types
    /// capable of showing a thumbnail (images, audio covers, videos, PDF, SVG). Folders
    /// and plain-icon files stay at the normal list height.
    /// Enabled by default, including for older configurations.
    #[serde(default = "default_true")]
    pub compact_icon_rows_in_preview: bool,
    /// Shows the "Modified" date in **UTC** rather than **local time**.
    /// `#[serde(default)]` = `false` keeps local time by default.
    #[serde(default)]
    pub clock_utc: bool,
    /// Shows Windows context-menu entries (shell extensions from
    /// installed programs: PeaZip, Git, TortoiseGit…) in views' context
    /// menu. This setting allows disabling their loading in case of a
    /// slow or unstable third-party extension, since they run in-process.
    #[serde(default = "default_true")]
    pub shell_ctx_menu: bool,
    /// Windows context-menu entries HIDDEN by the user,
    /// identified by their top-level LABEL (stable per machine/OS
    /// language). Checked/unchecked in Settings › Open with.
    #[serde(default)]
    pub shell_menu_disabled: Vec<String>,
    /// Linux: has the one-time "`ffmpeg` missing → no video
    /// thumbnails" warning already been shown? Set to `true` after the first
    /// display so it's never shown again. Not applicable on Windows (native video).
    #[serde(default)]
    pub ffmpeg_hint_shown: bool,
    /// UI zoom chosen by the user (factor MULTIPLIED by the screen's
    /// scale). `1.0` = OS-native scale. Applied at startup and on every
    /// change via `Window::dispatch_event(ScaleFactorChanged)`. Clamped
    /// to `[0.5, 2.0]` on application to avoid an unusable UI.
    #[serde(default = "default_ui_scale")]
    pub ui_scale: f32,
}

fn default_ui_scale() -> f32 {
    1.0
}

pub const DEFAULT_VIEW_TEXT_FONT_SIZE: i32 = 13;
pub const MIN_VIEW_TEXT_FONT_SIZE: i32 = 9;
pub const MAX_VIEW_TEXT_FONT_SIZE: i32 = 20;
pub const MIN_VIEW_ITEM_SPACING: i32 = 0;
pub const MAX_VIEW_ITEM_SPACING: i32 = 16;
/// Deepest walk the two folder-statistics settings accept. The spin
/// boxes in the settings panel carry the same bound; raising one alone
/// leaves the other to snap the value back.
pub const MAX_FOLDER_STAT_DEPTH: i32 = 16;

fn default_view_text_font_size() -> i32 {
    DEFAULT_VIEW_TEXT_FONT_SIZE
}

fn default_true() -> bool {
    true
}

/// Default logical width of the sidebar (px).
pub const DEFAULT_SIDEBAR_WIDTH: u32 = 210;

impl Default for Config {
    fn default() -> Self {
        Self {
            language: Lang::default(),
            theme: Theme::default(),
            window_width: DEFAULT_WINDOW_WIDTH,
            window_height: DEFAULT_WINDOW_HEIGHT,
            confirm_permanent_delete: true,
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            sidebar_section_order: SidebarSection::DEFAULT_ORDER.to_vec(),
            sidebar_shortcut_redirects: std::collections::BTreeMap::new(),
            default_columns: crate::columns::default_columns(),
            shortcut_overrides: std::collections::BTreeMap::new(),
            letter_shortcut_layout: String::new(),
            letter_layout_notice_seen: false,
            recursive_mtime_depth: 0,
            recursive_size_depth: 0,
            view_text_font_size_override: false,
            view_text_font_size: DEFAULT_VIEW_TEXT_FONT_SIZE,
            view_item_spacing: MIN_VIEW_ITEM_SPACING,
            default_tab_bar_mode: 0,
            default_preview_mode: true,
            default_thumbnail_zoom: DEFAULT_THUMBNAIL_ZOOM,
            default_item_arrangement: ItemArrangement::List,
            default_show_hidden: false,
            tab_path_tooltip: true,
            grid_name_tooltip: true,
            search_path_tooltip: true,
            legacy_warn_unsaved_workspace: None,
            workspace_panel_save: Some(WorkspaceLoadPolicy::Ask),
            workspace_shortcut_save: WorkspaceLoadPolicy::Ask,
            compact_icon_rows_in_preview: true,
            clock_utc: false,
            shell_ctx_menu: true,
            shell_menu_disabled: Vec::new(),
            ffmpeg_hint_shown: false,
            ui_scale: default_ui_scale(),
        }
    }
}

impl Config {
    /// The stored layout, or none when the value names nothing known.
    pub fn letter_layout(&self) -> crate::shortcuts::LetterLayout {
        crate::shortcuts::LetterLayout::from_id(&self.letter_shortcut_layout)
    }

    /// Loads from `path`. If the file doesn't exist, writes the default
    /// config to disk then returns it.
    pub fn load_or_default(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(content) => {
                let cfg: Config = toml::from_str(&content).map_err(|e| {
                    // The caller falls back to defaults and will save them, so
                    // the settings that could not be read are kept aside rather
                    // than overwritten a moment later.
                    crate::paths::preserve_unreadable(path);
                    crate::error::Error::Config(format!("parse {}: {e}", path.display()))
                })?;
                Ok(cfg.sanitized())
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let cfg = Self::default();
                cfg.save(path)?;
                Ok(cfg)
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Serializes to TOML and writes to disk (creates parent folders).
    pub fn save(&self, path: &Path) -> Result<()> {
        crate::store::save(path, self, |e| {
            crate::error::Error::Config(format!("serialize {}: {e}", path.display()))
        })
    }

    /// Normalizes only the preferences a manually edited TOML could break
    /// the invariants of. The default order is stable and allocation-free.
    pub fn sanitized(mut self) -> Self {
        self.workspace_panel_save = Some(self.panel_save_policy());
        self.legacy_warn_unsaved_workspace = None;
        self.default_thumbnail_zoom = self
            .default_thumbnail_zoom
            .clamp(MIN_THUMBNAIL_ZOOM, MAX_THUMBNAIL_ZOOM);
        self.sidebar_section_order = SidebarSection::sanitized_order(&self.sidebar_section_order);
        self.view_text_font_size = self
            .view_text_font_size
            .clamp(MIN_VIEW_TEXT_FONT_SIZE, MAX_VIEW_TEXT_FONT_SIZE);
        self.view_item_spacing = self
            .view_item_spacing
            .clamp(MIN_VIEW_ITEM_SPACING, MAX_VIEW_ITEM_SPACING);
        self
    }

    pub fn panel_save_policy(&self) -> WorkspaceLoadPolicy {
        self.workspace_panel_save.unwrap_or_else(|| {
            if self.legacy_warn_unsaved_workspace == Some(false) {
                WorkspaceLoadPolicy::Discard
            } else {
                WorkspaceLoadPolicy::Ask
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn tooltip_preferences_keep_existing_configs_enabled_and_restore_user_choices() {
        let previous: Config = toml::from_str("tab_path_tooltip = false").unwrap();
        assert!(!previous.tab_path_tooltip);
        assert!(previous.grid_name_tooltip);
        assert!(previous.search_path_tooltip);

        let changed = Config {
            grid_name_tooltip: false,
            search_path_tooltip: false,
            ..Config::default()
        };
        let restored: Config = toml::from_str(&toml::to_string(&changed).unwrap()).unwrap();
        assert!(!restored.grid_name_tooltip);
        assert!(!restored.search_path_tooltip);
        assert!(restored.tab_path_tooltip);
    }

    #[test]
    fn workspace_load_policies_migrate_independently_and_round_trip() {
        for defaults in [Config::default(), toml::from_str::<Config>("").unwrap()] {
            let defaults = defaults.sanitized();
            assert_eq!(defaults.panel_save_policy(), WorkspaceLoadPolicy::Ask);
            assert_eq!(defaults.workspace_shortcut_save, WorkspaceLoadPolicy::Ask);
        }
        let old: Config = toml::from_str("warn_unsaved_workspace = false").unwrap();
        assert_eq!(old.workspace_shortcut_save, WorkspaceLoadPolicy::Ask);
        assert_eq!(old.panel_save_policy(), WorkspaceLoadPolicy::Discard);
        let migrated = old.sanitized();
        let serialized = toml::to_string(&migrated).unwrap();
        assert!(serialized.contains("workspace_panel_save = \"discard\""));
        assert!(!serialized.contains("warn_unsaved_workspace"));
        let old_enabled: Config = toml::from_str("warn_unsaved_workspace = true").unwrap();
        assert_eq!(old_enabled.panel_save_policy(), WorkspaceLoadPolicy::Ask);
        let explicit: Config =
            toml::from_str("warn_unsaved_workspace = false\nworkspace_panel_save = 'save'")
                .unwrap();
        assert_eq!(explicit.panel_save_policy(), WorkspaceLoadPolicy::Save);
        let unknown: Config = toml::from_str("workspace_shortcut_save = 'unknown'").unwrap();
        assert_eq!(unknown.workspace_shortcut_save, WorkspaceLoadPolicy::Ask);
        for policy in [
            WorkspaceLoadPolicy::Ask,
            WorkspaceLoadPolicy::Discard,
            WorkspaceLoadPolicy::Save,
        ] {
            let config = Config {
                workspace_panel_save: Some(policy),
                workspace_shortcut_save: policy,
                ..Config::default()
            };
            let restored: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
            assert_eq!(restored.workspace_shortcut_save, policy);
            assert_eq!(restored.panel_save_policy(), policy);
            assert_eq!(WorkspaceLoadPolicy::from_index(policy.index()), policy);
        }
        assert_eq!(
            WorkspaceLoadPolicy::from_index(99),
            WorkspaceLoadPolicy::Ask
        );
    }

    #[test]
    fn preference_thumbnail_zoom_defaults_and_bounds() {
        let old: Config = toml::from_str("").unwrap();
        assert_eq!(old.default_thumbnail_zoom, DEFAULT_THUMBNAIL_ZOOM);
        for (input, expected) in [(-10, 1), (0, 1), (5, 5), (99, 8)] {
            let config: Config =
                toml::from_str(&format!("default_thumbnail_zoom = {input}")).unwrap();
            let config = config.sanitized();
            assert_eq!(config.default_thumbnail_zoom, expected);
            let restored: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
            assert_eq!(restored.default_thumbnail_zoom, expected);
        }
    }

    #[test]
    fn hidden_default_is_off_when_absent_and_survives_a_round_trip() {
        // An existing configuration has no such field, and must keep the
        // historical behaviour rather than start showing hidden entries.
        let old: Config = toml::from_str("default_preview_mode = true").unwrap();
        assert!(!old.default_show_hidden);
        let on: Config = toml::from_str("default_show_hidden = true").unwrap();
        assert!(on.default_show_hidden);
        let restored: Config = toml::from_str(&toml::to_string(&on).unwrap()).unwrap();
        assert!(restored.default_show_hidden);
        assert!(!Config::default().default_show_hidden);
    }

    #[test]
    fn grid_default_is_optional_and_independent_of_previews() {
        let old: Config = toml::from_str("default_preview_mode = true").unwrap();
        assert_eq!(old.default_item_arrangement, ItemArrangement::List);
        assert!(old.default_preview_mode);
        let grid: Config =
            toml::from_str("default_item_arrangement = 'grid'\ndefault_preview_mode = false")
                .unwrap();
        assert_eq!(grid.default_item_arrangement, ItemArrangement::Grid);
        assert!(!grid.default_preview_mode);
        let restored: Config = toml::from_str(&toml::to_string(&grid).unwrap()).unwrap();
        assert_eq!(restored.default_item_arrangement, ItemArrangement::Grid);
        assert!(!restored.default_preview_mode);
        let unknown: Config = toml::from_str("default_item_arrangement = 'unknown'").unwrap();
        assert_eq!(unknown.default_item_arrangement, ItemArrangement::List);
    }

    #[test]
    fn the_letter_layout_defaults_to_none_and_survives_a_round_trip() {
        use crate::shortcuts::LetterLayout;

        // A file written before this setting existed assumes nothing.
        let old: Config = toml::from_str("").unwrap();
        assert_eq!(old.letter_layout(), LetterLayout::Off);

        for layout in LetterLayout::ALL {
            let config = Config {
                letter_shortcut_layout: layout.id().to_string(),
                shortcut_overrides: std::collections::BTreeMap::from([(
                    "refresh".into(),
                    "Ctrl+с".into(),
                )]),
                ..Config::default()
            };
            let restored: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
            assert_eq!(restored.letter_layout(), layout);
            // A chord stored as typed is never rewritten by the setting.
            assert_eq!(restored.shortcut_overrides, config.shortcut_overrides);
        }
    }

    #[test]
    fn an_unknown_layout_costs_only_that_setting() {
        // Hand-edited, or written by a later version that knows more
        // layouts. The rest of the file must still load.
        let config: Config =
            toml::from_str("letter_shortcut_layout = 'klingon'\nlanguage = 'fr'").unwrap();
        assert_eq!(config.letter_layout(), crate::shortcuts::LetterLayout::Off);
        assert_eq!(config.language, Lang::Fr);
    }

    #[test]
    fn default_round_trip() {
        let original = Config::default();
        let s = toml::to_string_pretty(&original).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.language, original.language);
        assert_eq!(back.letter_shortcut_layout, original.letter_shortcut_layout);
        assert_eq!(back.theme, original.theme);
        assert_eq!(back.window_width, original.window_width);
        assert_eq!(back.window_height, original.window_height);
        assert_eq!(
            back.confirm_permanent_delete,
            original.confirm_permanent_delete
        );
        assert_eq!(
            back.compact_icon_rows_in_preview,
            original.compact_icon_rows_in_preview
        );
        assert_eq!(back.default_preview_mode, original.default_preview_mode);
        assert_eq!(
            back.view_text_font_size_override,
            original.view_text_font_size_override
        );
        assert_eq!(back.view_text_font_size, original.view_text_font_size);
        assert_eq!(back.view_item_spacing, original.view_item_spacing);
        assert_eq!(back.sidebar_section_order, SidebarSection::DEFAULT_ORDER.to_vec());
        assert!(back.sidebar_shortcut_redirects.is_empty());
    }

    #[test]
    fn explicit_list_mode_round_trips_despite_the_new_install_default() {
        let original = Config {
            default_preview_mode: false,
            ..Config::default()
        };
        let serialized = toml::to_string_pretty(&original).unwrap();
        let restored: Config = toml::from_str(&serialized).unwrap();
        assert!(!restored.default_preview_mode);
    }

    #[test]
    fn load_creates_default_when_missing() {
        let dir = tempdir();
        let path = dir.join("config.toml");
        let cfg = Config::load_or_default(&path).unwrap();
        assert_eq!(cfg.language, Lang::default());
        assert!(cfg.default_preview_mode);
        assert!(Config::load_or_default(&path).unwrap().default_preview_mode);
        assert!(path.exists());
    }

    #[test]
    fn load_existing_file() {
        let dir = tempdir();
        let path = dir.join("config.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "language = \"fr\"\ntheme = \"dark\"").unwrap();
        let cfg = Config::load_or_default(&path).unwrap();
        assert_eq!(cfg.language, Lang::Fr);
        assert_eq!(cfg.theme, Theme::Dark);
        // Field absent from an earlier config → safeguard KEPT (true).
        assert_eq!(cfg.panel_save_policy(), WorkspaceLoadPolicy::Ask);
        // Same backward compatibility for compact rows in preview.
        assert!(cfg.compact_icon_rows_in_preview);
        // Earlier configurations keep the historical List default.
        assert!(!cfg.default_preview_mode);
        assert!(!cfg.view_text_font_size_override);
        assert_eq!(cfg.view_text_font_size, DEFAULT_VIEW_TEXT_FONT_SIZE);
        assert_eq!(cfg.view_item_spacing, MIN_VIEW_ITEM_SPACING);
        // The global order absent from an old config falls back to the historical order.
        assert_eq!(cfg.sidebar_section_order, SidebarSection::DEFAULT_ORDER.to_vec());
    }

    #[test]
    fn sidebar_order_is_global_round_trips_and_reorders() {
        let mut cfg = Config::default();
        assert!(SidebarSection::reorder(
            &mut cfg.sidebar_section_order,
            SidebarSection::Network,
            0
        ));
        assert_eq!(
            cfg.sidebar_section_order,
            vec![
                SidebarSection::Network,
                SidebarSection::Path,
                SidebarSection::Favorites,
                SidebarSection::Drives,
                SidebarSection::Shortcuts,
            ]
        );
        assert!(!SidebarSection::reorder(
            &mut cfg.sidebar_section_order,
            SidebarSection::Network,
            0
        ));
        assert!(SidebarSection::reorder(
            &mut cfg.sidebar_section_order,
            SidebarSection::Drives,
            1
        ));

        let serialized = toml::to_string_pretty(&cfg).unwrap();
        assert!(serialized.contains("sidebar_section_order"));
        let restored: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(restored.sidebar_section_order, cfg.sidebar_section_order);
    }

    #[test]
    fn sidebar_shortcut_redirects_are_optional_and_persistent() {
        let legacy: Config = toml::from_str("language = \"en\"\ntheme = \"dark\"").unwrap();
        assert!(legacy.sidebar_shortcut_redirects.is_empty());

        let mut config = Config::default();
        config
            .sidebar_shortcut_redirects
            .insert("root/directory_a".into(), "root/directory_b".into());
        let serialized = toml::to_string_pretty(&config).unwrap();
        let restored: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(
            restored
                .sidebar_shortcut_redirects
                .get("root/directory_a")
                .map(String::as_str),
            Some("root/directory_b")
        );
    }

    #[test]
    fn sidebar_order_sanitization_repairs_duplicate_manual_config() {
        let cfg = Config {
            sidebar_section_order: vec![
                SidebarSection::Network,
                SidebarSection::Network,
                SidebarSection::Drives,
                SidebarSection::Shortcuts,
            ],
            ..Config::default()
        };
        let dir = tempdir();
        let path = dir.join("config.toml");
        cfg.save(&path).unwrap();
        let repaired = Config::load_or_default(&path).unwrap();
        // Duplicates dropped, the missing sections appended, and `Path` — which
        // the input predates — moved from the bottom to the top.
        assert_eq!(
            repaired.sidebar_section_order,
            vec![
                SidebarSection::Path,
                SidebarSection::Network,
                SidebarSection::Drives,
                SidebarSection::Shortcuts,
                SidebarSection::Favorites,
            ]
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn sidebar_order_gains_the_path_section_when_an_old_config_lacks_it() {
        // Exactly the shape a previous build wrote: four entries, no `path`.
        let order = SidebarSection::sanitized_order(&[
            SidebarSection::Drives,
            SidebarSection::Favorites,
            SidebarSection::Shortcuts,
            SidebarSection::Network,
        ]);
        assert_eq!(
            order,
            vec![
                SidebarSection::Path,
                SidebarSection::Drives,
                SidebarSection::Favorites,
                SidebarSection::Shortcuts,
                SidebarSection::Network,
            ]
        );

        // A config that already names `Path` keeps the user's own placement.
        let moved = SidebarSection::sanitized_order(&[
            SidebarSection::Path,
            SidebarSection::Drives,
            SidebarSection::Favorites,
            SidebarSection::Shortcuts,
            SidebarSection::Network,
        ]);
        assert_eq!(
            moved,
            vec![
                SidebarSection::Path,
                SidebarSection::Drives,
                SidebarSection::Favorites,
                SidebarSection::Shortcuts,
                SidebarSection::Network,
            ]
        );
    }

    #[test]
    fn view_text_font_size_is_clamped_when_loading_manual_config() {
        let dir = tempdir();
        let path = dir.join("config.toml");
        let cfg = Config {
            view_text_font_size_override: true,
            view_text_font_size: MAX_VIEW_TEXT_FONT_SIZE + 50,
            ..Config::default()
        };
        cfg.save(&path).unwrap();
        let repaired = Config::load_or_default(&path).unwrap();
        assert!(repaired.view_text_font_size_override);
        assert_eq!(repaired.view_text_font_size, MAX_VIEW_TEXT_FONT_SIZE);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn view_item_spacing_is_clamped_when_loading_manual_config() {
        let dir = tempdir();
        let path = dir.join("config.toml");
        let cfg = Config {
            view_item_spacing: MAX_VIEW_ITEM_SPACING + 50,
            ..Config::default()
        };
        cfg.save(&path).unwrap();
        let repaired = Config::load_or_default(&path).unwrap();
        assert_eq!(repaired.view_item_spacing, MAX_VIEW_ITEM_SPACING);
        std::fs::remove_dir_all(dir).ok();
    }

    fn tempdir() -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("favnyr-test-{}", rand_suffix()));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn rand_suffix() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }
}
