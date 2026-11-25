//! The sidebar: favourites, drives, quick access, network and the path tree.
//!
//! Places are the least stable thing the window shows -- a drive can be
//! ejected, a share can disappear, a favourite can point at a folder that no
//! longer exists -- so each section carries its own cheap signature or stamp
//! and is rebuilt only when that changes. The sidebar is refreshed from many
//! call sites, and doing the work unconditionally would put filesystem calls
//! (and network calls, which block for seconds) on the UI thread.
//!
//! The path tree is the exception that proves the rule: it describes the
//! folder about to be shown, so it is pushed with the path being navigated
//! to rather than read from the tab, whose stored path only catches up once
//! the listing arrives.

use super::*;

/// Eject operation requested from the drive menu.
#[derive(Clone, Copy)]
pub(super) enum EjectOp {
    /// Safe removal of a removable drive (USB/CD).
    SafeRemove,
    /// Disconnection of a mapped network drive.
    Disconnect,
}

/// Runs the eject/disconnect on a background thread (may block) then
/// comes back to the UI: result toast + sidebar re-scan on
/// success.
///
/// Before the removal, Favnyr releases its own handles on the volumes. The active
/// panel's `notify` watcher holds a directory handle that can block
/// the eject. It is therefore dropped unconditionally, then `invoke_refresh()`
/// re-lists the active panel and re-arms the watcher on return.
/// `hotplug` says whether the hardware can actually be unplugged. It is NOT
/// recomputed here: the sidebar already established it per platform — from the
/// sysfs bus on Linux, from a storage IOCTL on Windows — and deriving it a
/// second time from the device string alone would silently lose the Windows
/// answer, leaving the menu and the toast contradicting each other.
pub(super) fn spawn_eject(window: &MainWindow, state: &AppState, device: String, op: EjectOp, hotplug: bool) {
    if shutdown::stopping(state) {
        return;
    }
    // First invalidate any watcher installation still in flight — otherwise
    // it could resurrect a handle on the volume being ejected.
    state.watcher_gen.fetch_add(1, Ordering::SeqCst);
    if let Ok(mut w) = state.watcher.lock() {
        *w = None; // releases the OS handle (otherwise it self-vetoes)
    }
    let weak = window.as_weak();
    let lang = state.snapshot_config().language;
    let lease = crate::background_work::acquire();
    std::thread::spawn(move || {
        let _lease = lease;
        let res = match op {
            EjectOp::SafeRemove => favnyr_core::eject::safe_remove(&device, hotplug),
            EjectOp::Disconnect => favnyr_core::eject::disconnect(&device),
        };
        let _ = slint::invoke_from_event_loop(move || {
            let Some(w) = weak.upgrade() else { return };
            let strings = w.global::<crate::Strings>();
            match res {
                Ok(()) => {
                    show_notice_ok(
                        &w,
                        match op {
                            EjectOp::SafeRemove if hotplug => strings.get_net_ejected(),
                            EjectOp::SafeRemove => strings.get_net_released(),
                            EjectOp::Disconnect => strings.get_net_disconnected(),
                        },
                    );
                    // This runs back on the UI thread but outside any state
                    // handle: the closure crossed a thread boundary, so it
                    // cannot carry one. The window's own refresh entry point
                    // is invoked instead — one implementation of the re-scan,
                    // not a second one that could drift.
                    w.invoke_sidebar_refresh(); // the drive is gone
                }
                Err(err) => {
                    let reason = i18n::eject_error_message(lang, &err);
                    show_notice(&w, format!("{}: {reason}", strings.get_net_eject_failed()));
                }
            }
            w.invoke_refresh(); // re-lists the active panel + re-arms the watcher
        });
    });
}

/// (Re)builds the "Places" sidebar model (drives, shortcuts,
/// network, trash) from `favnyr-core::places` and pushes it to the UI.
/// Fingerprint of what the capacity gauges currently DISPLAY.
///
/// Hashes the rendered figures and warning levels, never the raw byte counts:
/// a volume ticking over by a few kilobytes changes its bytes constantly while
/// the text stays "38.0 / 64.0 GB". Comparing what is drawn means the sidebar is
/// rebuilt only when the user would actually see a difference, which keeps the
/// existing "rebuild only if changed" contract intact — a rebuild replaces the
/// row models and would otherwise churn under an idle disk.
pub(super) fn drives_space_signature(lang: Lang) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for place in favnyr_core::places::drives() {
        let space = drive_space_ui(&place, lang);
        place.path.hash(&mut hasher);
        space.level.hash(&mut hasher);
        space.text.hash(&mut hasher);
        space.text_short.hash(&mut hasher);
        space.hint.hash(&mut hasher);
    }
    hasher.finish()
}

/// What the capacity gauge shows for one place. Level `-1` means no gauge.
///
/// Only a local volume that answered gets one. A share is never measured (the
/// call leaves the machine and the answer would describe the server), and
/// neither is a shortcut or the trash, which live on a volume already listed on
/// its own line — stating the same capacity twice under two names reads as a
/// contradiction, not as agreement.
pub(super) struct DriveSpaceUi {
    /// `-1` no gauge · `0` roomy · `1` low · `2` critical.
    pub(super) level: i32,
    /// Share of the volume in use, 0..1 — the bar grows with it.
    pub(super) used_ratio: f32,
    /// "26.0 / 57.3 GB", and the used figure alone for a narrow panel.
    pub(super) text: String,
    pub(super) text_short: String,
    /// One-line breakdown for the hover hint, where there is room to name
    /// each figure instead of leaving the reader to infer which is which.
    pub(super) hint: String,
}

impl DriveSpaceUi {
    /// No capacity to show: not a local volume, or one that did not answer.
    pub(super) fn none() -> Self {
        DriveSpaceUi {
            level: -1,
            used_ratio: 0.0,
            text: String::new(),
            text_short: String::new(),
            hint: String::new(),
        }
    }
}

pub(super) fn drive_space_ui(place: &favnyr_core::places::Place, lang: Lang) -> DriveSpaceUi {
    use favnyr_core::places::PlaceKind;
    // A volume nobody has mounted has a size but no occupancy: no filesystem is
    // open to report one. It shows the figure and no gauge — an empty bar would
    // claim the volume is empty, which is a different statement.
    if matches!(place.kind, PlaceKind::Volume | PlaceKind::LockedVolume) {
        if place.total_bytes == 0 {
            return DriveSpaceUi::none();
        }
        let size = rfs::format_size(place.total_bytes, i18n::size_units(lang));
        let hint = if place.kind == PlaceKind::LockedVolume {
            "volume_locked_hint"
        } else {
            "volume_not_mounted_hint"
        };
        return DriveSpaceUi {
            level: -1,
            used_ratio: 0.0,
            text: size.clone(),
            text_short: size.clone(),
            hint: i18n::tr(lang, hint).replace("{size}", &size),
        };
    }
    if place.kind != PlaceKind::Drive || place.total_bytes == 0 {
        return DriveSpaceUi::none();
    }
    let total = place.total_bytes;
    let free = place.free_bytes.min(total);
    let used = total - free;
    DriveSpaceUi {
        // The warning still comes from what is LEFT: that is the figure that
        // decides whether the next operation fits, whatever the gauge draws.
        level: rfs::free_space_level(free, total),
        used_ratio: used as f32 / total as f32,
        text: rfs::format_used_total(used, total, i18n::size_units(lang)),
        text_short: rfs::format_size(used, i18n::size_units(lang)),
        hint: i18n::tr(lang, "drive_space_hint")
            .replace("{free}", &rfs::format_size(free, i18n::size_units(lang)))
            .replace("{used}", &rfs::format_size(used, i18n::size_units(lang)))
            .replace("{total}", &rfs::format_size(total, i18n::size_units(lang))),
    }
}

/// Volumes seen but not mounted, re-read only when the block topology moved.
/// Listing them spawns a process, which is why the answer is kept: the sidebar
/// is rebuilt far more often than a disk is plugged in.
pub(super) fn cached_unmounted_volumes(state: &AppState) -> Vec<favnyr_core::places::Place> {
    // Two independent facts invalidate the answer: a disk appearing or
    // leaving, which moves the block topology; and a volume of this very list
    // becoming mounted, which must drop it from the list — and mounting does
    // NOT touch `/sys/class/block`. Both reads are plain files.
    let signature = favnyr_core::places::block_signature()
        ^ favnyr_core::places::drives_signature().rotate_left(32);
    let mut cache = state.volumes_cache.borrow_mut();
    if cache.0 != signature {
        *cache = (signature, favnyr_core::places::unmounted_volumes());
    }
    cache.1.clone()
}

pub(super) fn refresh_sidebar(window: &MainWindow, state: &AppState) {
    use favnyr_core::places::{self, PlaceKind};
    let lang = state.snapshot_config().language;
    // Icon code (see SidebarItem): 0 folder · 1 home · 2 drive · 3
    // trash · 4 network · 5 phone · 6 unmounted volume · 7 locked volume.
    fn kind_code(k: PlaceKind) -> i32 {
        match k {
            PlaceKind::Folder => 0,
            PlaceKind::Home => 1,
            PlaceKind::Drive => 2,
            PlaceKind::Trash => 3,
            PlaceKind::Network => 4,
            PlaceKind::Volume => 6,
            PlaceKind::LockedVolume => 7,
        }
    }
    let place_item = |p: places::Place| -> SidebarPlace {
        let space = drive_space_ui(&p, lang);
        SidebarPlace {
            label: p.name.into(),
            // A volume that is not mounted has no path yet. Like a portable
            // device, it travels as the handle its own backend understands —
            // here the block device that would be mounted — and `kind` tells
            // the click handler how to read it.
            path: if matches!(p.kind, PlaceKind::Volume | PlaceKind::LockedVolume) {
                p.device.clone().into()
            } else {
                p.path.display().to_string().into()
            },
            kind: kind_code(p.kind),
            removable: p.removable,
            hotplug: p.hotplug,
            device: p.device.into(),
            space_level: space.level,
            space_used_ratio: space.used_ratio,
            space_text: space.text.into(),
            space_text_short: space.text_short.into(),
            space_hint: space.hint.into(),
        }
    };
    let redirects = state.snapshot_config().sidebar_shortcut_redirects;
    let shortcuts = places::user_place_candidates()
        .into_iter()
        .filter_map(|mut place| {
            let mut path = place.path.clone();
            // Each rename normally updates the original redirect in place.
            // The bounded loop also repairs earlier chained entries safely.
            for _ in 0..=redirects.len() {
                let Some(next) = redirects.get(&path.to_string_lossy().into_owned()) else {
                    break;
                };
                let next = PathBuf::from(next);
                if ops::paths_equal(&next, &path) {
                    break;
                }
                path = next;
            }
            if !path.is_dir() {
                return None;
            }
            if path != place.path {
                place.path = path;
                place.name = place
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| place.path.display().to_string());
            }
            Some(place)
        })
        .map(place_item)
        .collect::<Vec<_>>();
    // Keep sections strictly grouped: local, Windows portable
    // devices, then network. An MTP isn't a core `Place`/`PathBuf`.
    let filesystem = places::drives();
    let mut filesystem_drives = filesystem
        .iter()
        .filter(|p| p.kind != PlaceKind::Network)
        .cloned()
        .map(place_item)
        .collect::<Vec<_>>();
    // Volumes present but not mounted come after the mounted ones: what is
    // reachable now reads first.
    filesystem_drives.extend(cached_unmounted_volumes(state).into_iter().map(place_item));
    #[cfg(any(windows, target_os = "linux"))]
    let drives = {
        let mut drives = filesystem_drives;
        for (label, handle) in portable_devices() {
            drives.push(SidebarPlace {
                label: label.into(),
                path: handle.into(),
                kind: 5,
                removable: false,
                // A phone is unplugged by hand, but it offers no release entry
                // — Favnyr never mounted it.
                hotplug: true,
                device: SharedString::new(),
                // A phone is addressed through an opaque handle, not a path a
                // filesystem call can measure; asking its capacity would be a
                // device query on the UI thread.
                space_level: -1,
                space_used_ratio: 0.0,
                space_text: SharedString::new(),
                space_text_short: SharedString::new(),
                space_hint: SharedString::new(),
            });
        }
        drives
    };
    #[cfg(not(any(windows, target_os = "linux")))]
    let drives = filesystem_drives;
    let network = filesystem
        .into_iter()
        .filter(|p| p.kind == PlaceKind::Network)
        .map(place_item)
        .collect::<Vec<_>>();
    let trash = vec![place_item(places::trash_place())];

    // The four models remain semantic and stable; only their Slint rank
    // varies with the global preference. Trash stays outside this ordering.
    window.set_sidebar_places_drives(ModelRc::new(VecModel::from(drives)));
    window.set_sidebar_places_shortcuts(ModelRc::new(VecModel::from(shortcuts)));
    window.set_sidebar_places_network(ModelRc::new(VecModel::from(network)));
    window.set_sidebar_places_trash(ModelRc::new(VecModel::from(trash)));
}

/// Pushes the workspace-specific collapse state and the global config order to Slint.
/// Called at startup, when loading a workspace, and on reset; no
/// parallel visual logic decides default values.
/// Builds the address tree of the active view.
///
/// Four kinds of rows, in this order: the ancestry (drive root down to the
/// parent), the folder itself, its sub-folders, then its siblings. Only folders
/// ever appear — the tree is for navigating, and a file cannot be navigated
/// into.
///
/// Sub-folders and siblings are read from disk (one listing each). Everything
/// else comes from parsing the path, so a deep path costs two directory reads
/// no matter how many levels it has.
pub(super) fn build_path_tree(current: &Path) -> Vec<crate::PathNode> {
    let mut nodes: Vec<crate::PathNode> = Vec::new();

    // 1. Ancestry, from the root down to and including the folder itself.
    // `parent()` walks up to the root on its own, which also covers UNC paths
    // (`\\server\share\a` → `\\server\share` → `\\server`).
    let mut chain: Vec<&Path> = Vec::new();
    let mut cursor = Some(current);
    while let Some(path) = cursor {
        chain.push(path);
        cursor = path.parent();
    }
    let total = chain.len();
    for (index, path) in chain.iter().rev().enumerate() {
        let is_current = index + 1 == total;
        // The ancestry sits ON the thread (roles 0 and 1); the current folder
        // ends it with a dot.
        nodes.push(path_tree_node(path, 0, if is_current { 1 } else { 0 }, true));
    }

    // A network folder is never listed from here. `list_dir` on SMB or a mapped
    // drive blocks for seconds, and this runs on the UI thread as part of
    // refreshing the sidebar: the ancestry above costs nothing (it is parsed
    // from the path), so a share still gets a usable tree — just without its
    // sub-folders and siblings.
    if favnyr_core::places::is_network_path(current) || rfs::is_unc_path(current) {
        return nodes;
    }

    // 2. Sub-folders of the current folder.
    for path in child_folders(current) {
        nodes.push(path_tree_node(&path, 1, 2, true));
    }

    // 3. Siblings: the other folders of the parent.
    if let Some(parent) = current.parent() {
        for path in child_folders(parent) {
            if path == current {
                continue;
            }
            nodes.push(path_tree_node(&path, 0, 3, true));
        }
    }

    nodes
}

/// Visible sub-folders of `dir`, sorted by name.
///
/// Hidden folders are always skipped, whatever the view's own setting says: the
/// tree is a map of where a session navigates, and dot-files would bury it.
/// A folder that cannot be listed simply contributes nothing.
pub(super) fn child_folders(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = rfs::list_dir(dir, false) else {
        return Vec::new();
    };
    let mut folders: Vec<PathBuf> = entries
        .into_iter()
        .filter(|entry| entry.is_dir)
        .map(|entry| entry.path)
        .collect();
    folders.sort_by_key(|path| path_label(path).to_lowercase());
    folders
}

/// Display name of one tree row: the folder's own name, or its full path when
/// it has none (a drive root such as `C:\`).
pub(super) fn path_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

pub(super) fn path_tree_node(path: &Path, depth: i32, role: i32, available: bool) -> crate::PathNode {
    crate::PathNode {
        label: path_label(path).into(),
        path: path.display().to_string().into(),
        depth,
        role,
        available,
    }
}

/// Pushes the address tree for `path` to the sidebar.
///
/// The path is passed in rather than read from the active tab on purpose:
/// `tab.current_path` is only updated once the listing ARRIVES, so a tree built
/// from it right after a navigation would still describe the folder being left.
/// Every caller of `refresh_listing` already knows the folder it is about to
/// show — that is the one the tree must describe.
pub(super) fn push_path_tree(window: &MainWindow, path: &Path) {
    window.set_sidebar_path_nodes(ModelRc::new(VecModel::from(build_path_tree(path))));
}

pub(super) fn push_sidebar_sections_ui(window: &MainWindow, state: &AppState) {
    let sections = state.sidebar_sections.get();
    window.set_sidebar_shortcuts_collapsed(sections.shortcuts_collapsed);
    window.set_fav_section_collapsed(sections.favorites_collapsed);
    window.set_sidebar_drives_collapsed(sections.drives_collapsed);
    window.set_sidebar_network_collapsed(sections.network_collapsed);
    window.set_sidebar_path_collapsed(sections.path_collapsed);
    window.set_sidebar_section_order(ModelRc::new(VecModel::from(
        state
            .snapshot_config()
            .sidebar_section_order
            .into_iter()
            .map(|section| section.index() as i32)
            .collect::<Vec<_>>(),
    )));
}

/// Portable devices (phones, cameras) listed next to the drives. They carry no
/// filesystem path, so each platform resolves them through its own backend and
/// returns `(label, handle)` — the handle is opaque, and only the backend that
/// produced it knows how to open it.
#[cfg(any(windows, target_os = "linux"))]
pub(super) fn portable_devices() -> Vec<(String, String)> {
    #[cfg(windows)]
    {
        crate::winportable::devices()
            .into_iter()
            .map(|device| (device.name, device.shell_path))
            .collect()
    }
    #[cfg(target_os = "linux")]
    {
        crate::linportable::devices()
            .into_iter()
            .map(|device| (device.name, device.uri))
            .collect()
    }
}

/// Opens a portable device through the backend that produced its handle.
#[cfg(any(windows, target_os = "linux"))]
pub(super) fn open_portable_device(handle: &str) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        crate::winportable::open(handle)
    }
    #[cfg(target_os = "linux")]
    {
        crate::linportable::open(handle)
    }
}

/// Combined sidebar signature: core drive letters/mounts + the portable-device
/// list. The read is instant and never touches COM.
pub(super) fn sidebar_drives_signature() -> u64 {
    let filesystem = favnyr_core::places::drives_signature();
    #[cfg(windows)]
    {
        filesystem ^ crate::winportable::signature().rotate_left(32)
    }
    #[cfg(target_os = "linux")]
    {
        // Mounts, portable devices and block topology are three independent
        // sources: a disk plugged in but never mounted moves only the last.
        filesystem
            ^ crate::linportable::signature().rotate_left(32)
            ^ favnyr_core::places::block_signature().rotate_left(16)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        filesystem
    }
}

/// Virtual containers and legacy file bookmarks do not carry folder colours.
pub(super) fn favorite_folder_slot(
    favorite: &FavNode,
    annotations: &favnyr_core::annotations::AnnotationStore,
) -> i32 {
    if !favorite.is_container && favorite.is_dir {
        i32::from(annotations.color_of(Path::new(favorite.path.as_str())))
    } else {
        0
    }
}

/// Re-reads the favorites tree when the file changed under us. Same shape, and
/// the same reason, as `sync_annotations`: one file is shared by every running
/// instance, so a tree saved from this window must start from what the others
/// have written rather than from the view loaded at launch — which would drop
/// their additions without a trace.
pub(super) fn sync_favorites(state: &AppState) {
    let stamp = favorites_stamp();
    if *state.favorites_stamp.borrow() == stamp {
        return;
    }
    *state.favorites.borrow_mut() = favorites::FavStore::load(&paths::favorites_path());
    *state.favorites_stamp.borrow_mut() = stamp;
}

/// The tree, for reading.
pub(super) fn favorites_now(state: &AppState) -> std::cell::Ref<'_, favorites::FavStore> {
    sync_favorites(state);
    state.favorites.borrow()
}

/// The tree, for changing it. The re-read happens FIRST, so a change made
/// meanwhile by another instance is kept rather than overwritten.
pub(super) fn favorites_for_update(state: &AppState) -> std::cell::RefMut<'_, favorites::FavStore> {
    sync_favorites(state);
    state.favorites.borrow_mut()
}

/// Writes the tree and records the fingerprint it produced, so the next check
/// does not re-read what this instance just wrote.
pub(super) fn save_favorites(state: &AppState) {
    if let Err(err) = state.favorites.borrow().save(&paths::favorites_path()) {
        error!(error = %err, "save favorites failed");
        return;
    }
    *state.favorites_stamp.borrow_mut() = favorites_stamp();
}

/// Converts a flattened core node into a Slint struct (existence checked for
/// favorites → grayed out if the path is missing).
pub(super) fn flat_to_favnode(
    f: &FlatFav,
    annotations: &favnyr_core::annotations::AnnotationStore,
) -> FavNode {
    // One metadata read answers both questions and follows directory symlinks,
    // matching navigation. Containers carry no filesystem path.
    let metadata = (!f.is_container && !f.path.is_empty())
        .then(|| std::fs::metadata(&f.path).ok())
        .flatten();
    let available = f.is_container || f.path.is_empty() || metadata.is_some();
    let is_dir = metadata.is_some_and(|entry| entry.is_dir());
    // A file favorite is drawn with its own icon, resolved the way the views
    // resolve one and cached by path, so a tree of them costs one extraction
    // each per session. An empty image leaves the row on its document glyph.
    let icon = if !f.is_container && available && !is_dir {
        let path = Path::new(f.path.as_str());
        self_icon(path, &ext_of(path), false)
    } else {
        Image::default()
    };
    let mut node = FavNode {
        id: f.id.clone().into(),
        label: f.name.clone().into(),
        path: f.path.clone().into(),
        is_container: f.is_container,
        depth: f.depth,
        expanded: f.expanded,
        has_children: f.has_children,
        available,
        is_dir,
        folder_slot: 0,
        icon,
    };
    node.folder_slot = favorite_folder_slot(&node, annotations);
    node
}

/// Pushes the flattened tree + the container dropdown to the GUI.
pub(super) fn push_favorites_ui(window: &MainWindow, state: &AppState) {
    let fav = favorites_now(state);
    let annotations = annotations_now(state);
    let rows: Vec<FavNode> = fav
        .flatten()
        .iter()
        .map(|row| flat_to_favnode(row, &annotations))
        .collect();
    window.set_fav_nodes(ModelRc::new(VecModel::from(rows)));
    // Toggles "Collapse all" (if ≥1 container is expanded) / "Expand all".
    window.set_fav_can_collapse(fav.any_expanded());

    // Popup dropdown: "Root" + all indented containers.
    let mut labels: Vec<SharedString> =
        vec![i18n::tr(state.config.borrow().language, "fav_popup_root").into()];
    let mut ids: Vec<String> = vec![String::new()];
    for (id, name, depth) in fav.containers() {
        let indent = "   ".repeat(depth.max(0) as usize);
        labels.push(format!("{indent}{name}").into());
        ids.push(id);
    }
    window.set_fav_container_labels(ModelRc::new(VecModel::from(labels)));
    *state.fav_container_ids.borrow_mut() = ids;
}

/// Confirms that a favorite directory is still there, with the "not found"
/// notice when it is not. Only directories reach this: a favorite pointing at a
/// file is launched instead of being navigated, and never stands in for the
/// folder around it.
pub(super) fn resolve_fav_dir(window: &MainWindow, p: &Path, lang: Lang) -> Option<PathBuf> {
    if p.is_dir() {
        return Some(p.to_path_buf());
    }
    notice(
        window,
        i18n::tr(lang, "fav_toast_missing"),
        NoticeKind::FavMissing,
    );
    None
}

/// Returns a sidebar place only when Favnyr can navigate it without a
/// potentially blocking local probe. Network paths are validated by listing.
pub(super) fn resolve_sidebar_place_dir(path: &str) -> Option<PathBuf> {
    let path = PathBuf::from(path);
    if path.as_os_str().is_empty() {
        return None;
    }
    (favnyr_core::places::is_network_path(&path) || path.is_dir()).then_some(path)
}

/// Opens a favorite path in a NEW tab of the active view: folder →
/// tab on the folder; file → tab on its parent (we locate it, we
/// don't execute it). Path not found → "notice" toast.
pub(super) fn fav_open_path_new_tab(window: &MainWindow, state: &AppState, p: PathBuf) {
    let lang = state.config.borrow().language;
    let Some(target) = resolve_fav_dir(window, &p, lang) else {
        return;
    };
    let default_display = TabDisplayDefaults::from(&*state.config.borrow());
    let opened = state.with_tabs_mut(|book| {
        let a = book.open_after_active(target, default_display);
        book.tabs[a].current_path.clone()
    });
    load_directory(window, state, &opened, false);
}

/// What acting on a favorite leaf means, decided by what its target is.
pub(super) enum FavAction {
    /// A directory, which the sidebar navigates to.
    Enter(PathBuf),
    /// A file, which is launched the way the views launch one.
    Launch(PathBuf),
}

/// Classifies a favorite target by what it is. `None` once it is gone, which
/// includes a path that exists as neither, such as a dangling symlink.
pub(super) fn fav_action_for(path: PathBuf) -> Option<FavAction> {
    if path.is_dir() {
        return Some(FavAction::Enter(path));
    }
    if path.is_file() {
        return Some(FavAction::Launch(path));
    }
    None
}

/// Resolves a favorite leaf into the action its target calls for. `None` when
/// the favorite is unknown, or its target gone, the notice already shown.
pub(super) fn fav_action(window: &MainWindow, state: &AppState, id: &str) -> Option<FavAction> {
    let path = PathBuf::from(favorites_now(state).path_of(id)?);
    let action = fav_action_for(path);
    if action.is_none() {
        notice(
            window,
            i18n::tr(state.config.borrow().language, "fav_toast_missing"),
            NoticeKind::FavMissing,
        );
    }
    action
}

/// Launches a favorite file: the Favnyr opener set as the default for its
/// extension when there is one, otherwise the system's. The same single entry
/// point the views use, so a file behaves identically wherever it is opened
/// from.
pub(super) fn fav_launch(window: &MainWindow, state: &AppState, path: &PathBuf) {
    open_one_file_default(window, state, path);
}

/// Acts on a favorite IN the active tab: a directory replaces what the view
/// shows, where a file is launched.
///
/// Both buttons launch a file. A directory has two destinations worth telling
/// apart, this view or a new tab, but a file has only itself: offering it twice
/// would be offering the same thing twice.
pub(super) fn fav_open_here(window: &MainWindow, state: &AppState, id: &str) {
    match fav_action(window, state, id) {
        Some(FavAction::Enter(dir)) => load_directory(window, state, &dir, true),
        Some(FavAction::Launch(file)) => fav_launch(window, state, &file),
        None => {}
    }
}

/// Acts on a favorite in a NEW tab, or launches it when it is a file (no-op if
/// container / unknown).
pub(super) fn fav_open(window: &MainWindow, state: &AppState, id: &str) {
    match fav_action(window, state, id) {
        Some(FavAction::Enter(dir)) => fav_open_path_new_tab(window, state, dir),
        Some(FavAction::Launch(file)) => fav_launch(window, state, &file),
        None => {}
    }
}

/// Default alias of a path = its last component (otherwise the path).
pub(super) fn default_alias(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// Folder colours and notes apply to directories only, so the shared flyout
/// filters its favorite target through this. Unrelated to what may be added to
/// Favorites, which accepts files as well.
pub(super) fn favorite_path_is_directory(path: &Path) -> bool {
    path.is_dir()
}

/// Only an existing target may be newly added to Favorites, a file as readily
/// as a directory. The two behave differently once there, a directory being
/// navigated where a file is launched, but both are worth bookmarking.
/// Loading and manipulating older entries stays independent of this gate.
pub(super) fn favorite_target_exists(path: &Path) -> bool {
    path.exists()
}

/// Saves `paths` into the `container` favorites folder: deduplication
/// (already-present path ignored) + default alias, then saves + refreshes
/// the UI + appropriate toast (added / already present). SHARED core for drag
/// drops onto a favorites folder — intra/inter-instance tab and
/// a view's file selection — and aligned with `on_fav_save_commit`.
pub(super) fn add_paths_to_favorite(
    window: &MainWindow,
    state: &AppState,
    paths: &[PathBuf],
    container: &str,
) -> bool {
    // EMPTY `container` = root (""): drop on the "no favorites" zone / favorites
    // dead zone. The model handles "" as the root container; the boundary here
    // only keeps out what no longer exists.
    let paths: Vec<&PathBuf> = paths
        .iter()
        .filter(|path| favorite_target_exists(path))
        .collect();
    if paths.is_empty() {
        return false;
    }
    let mut added = 0usize;
    {
        let mut fav = favorites_for_update(state);
        for p in paths {
            let path_str = p.display().to_string();
            if fav.container_has_path(container, &path_str) {
                continue; // already in this favorites folder → ignored
            }
            if fav
                .add_favorite(container, &default_alias(p), &path_str)
                .is_some()
            {
                added += 1;
            }
        }
    }
    let lang = state.config.borrow().language;
    if added > 0 {
        save_favorites(state);
        push_favorites_ui(window, state);
        notice(
            window,
            i18n::tr(lang, "fav_toast_added"),
            NoticeKind::FavAdded,
        );
    } else {
        // Nothing added = everything already existed (the container is guaranteed valid on
        // hover) → neutral info, not an error.
        notice(
            window,
            i18n::tr(lang, "fav_toast_exists"),
            NoticeKind::FavExists,
        );
    }
    true
}

/// Opens the "Save as favorite" popup for a set of paths.
pub(super) fn open_fav_save_popup(window: &MainWindow, state: &AppState, mut paths: Vec<PathBuf>) {
    paths.retain(|path| favorite_target_exists(path));
    if paths.is_empty() {
        return;
    }
    let multi = paths.len() > 1;
    let target = if multi {
        i18n::footer_items_text(state.snapshot_config().language, paths.len())
    } else {
        paths[0].display().to_string()
    };
    let alias = if multi {
        String::new()
    } else {
        default_alias(&paths[0])
    };
    *state.fav_save_pending.borrow_mut() = paths;
    push_favorites_ui(window, state); // refreshes the container dropdown
    window.set_fav_save_multi(multi);
    window.set_fav_save_target(target.into());
    window.set_fav_save_alias(alias.into());
    window.set_fav_container_index(0);
    window.set_fav_save_open(true);
    window.set_fav_save_focus_armed(true);
}

/// Height of a favorites tree row (30px) + spacing (1px) = the vertical
/// pitch, MUST match the rendering (FavPanel: height 30 + spacing 1).
pub(super) const FAV_ROW_PITCH: f32 = 31.0;

/// From the drag geometry (cursor y, source row top y, source
/// index), computes `(target index, zone)` — zone 0 = before · 1 = inside · 2 = after.
pub(super) fn fav_drag_target(cur_y: f32, row_top: f32, row_idx: i32, count: i32) -> (i32, i32) {
    if count <= 0 {
        return (0, 1);
    }
    let list_origin = row_top - row_idx as f32 * FAV_ROW_PITCH;
    let hovered = (cur_y - list_origin) / FAV_ROW_PITCH;
    let ti = (hovered.floor() as i32).clamp(0, count - 1);
    let frac = hovered - hovered.floor();
    let zone = if frac < 0.33 {
        0
    } else if frac > 0.66 {
        2
    } else {
        1
    };
    (ti, zone)
}
/// A release outside the visible favorites tree has no reorder target. The
/// geometric helper clamps by design for edge scrolling, so this semantic gate
/// must run before its result is allowed to mutate the tree.
pub(super) fn fav_drop_target(
    commit_reorder: bool,
    cur_y: f32,
    row_top: f32,
    row_idx: i32,
    count: i32,
) -> Option<(i32, i32)> {
    commit_reorder.then(|| fav_drag_target(cur_y, row_top, row_idx, count))
}

/// Applies a node move based on the computed target/zone. Returns
/// `true` if the tree changed.
pub(super) fn fav_perform_move(state: &AppState, src_id: &str, target_index: i32, zone: i32) -> bool {
    let mut fav = favorites_for_update(state);
    let (target_id, target_is_container) = {
        let flat = fav.flatten();
        match flat.get(target_index.max(0) as usize) {
            Some(t) => (t.id.clone(), t.is_container),
            None => return false,
        }
    };
    if target_id == src_id {
        return false;
    }
    let target_parent = fav
        .nodes
        .iter()
        .find(|n| n.id == target_id)
        .map(|n| n.parent.clone())
        .unwrap_or_default();
    if zone == 1 && target_is_container {
        // Drop INSIDE the container (at the end) + expand it to see the result.
        let ok = fav.move_node(src_id, &target_id, None);
        if ok {
            fav.set_expanded(&target_id, true);
        }
        ok
    } else if zone == 0 {
        // Insert BEFORE the target (same parent).
        fav.move_node(src_id, &target_parent, Some(&target_id))
    } else {
        // Insert AFTER the target = before its next sibling (or at the end).
        let siblings: Vec<String> = fav
            .nodes
            .iter()
            .filter(|n| n.parent == target_parent)
            .map(|n| n.id.clone())
            .collect();
        let before = siblings
            .iter()
            .position(|id| *id == target_id)
            .and_then(|p| siblings.get(p + 1))
            .cloned();
        fav.move_node(src_id, &target_parent, before.as_deref())
    }
}
