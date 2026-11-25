//! File operations: what a paste becomes, and the worker runtime that
//! carries it out.
//!
//! A gesture is arbitrated here before anything touches the disk. A paste
//! first becomes a [PasteJob]: every source is given a destination -- the free
//! name, or the one the user picked in the conflict popup -- and only once
//! `pending` is empty does it execute. That ordering is what makes one gesture
//! one undo entry, and what keeps two items of the same paste off one
//! destination while neither exists on disk yet.
//!
//! Execution is a single worker path, [run_heavy]: it publishes progress rows,
//! delivers per-item results back to the UI thread, honours cancellation, and
//! records what actually happened. The bookkeeping reads the filesystem before
//! and after rather than the intent, so undo never claims more than the disk
//! shows.
//!
//! The module is called `file_ops` rather than `ops` because bridge.rs already
//! imports `favnyr_core::fs::ops` under that name.

use super::*;

/// Paste operation currently resolving name conflicts.
/// Items with no conflict are directly "resolved" (original name); those
/// whose name is already taken wait for a user decision via the popup.
/// Once all conflicts are settled, `resolved` is executed.
pub(super) struct PasteJob {
    pub(super) op: ClipOp,
    pub(super) dst_dir: PathBuf,
    /// Original selection order, kept independently because conflict items are
    /// resolved later than immediately available destinations.
    pub(super) source_order: Vec<PathBuf>,
    /// Sources still waiting to be arbitrated (name conflict).
    pub(super) pending: std::collections::VecDeque<PathBuf>,
    /// Triples (source, final target, overwrite) ready to execute. `overwrite =
    /// true` ("Replace" resolution) → the worker deletes the existing target
    /// before the copy/move; `false` (rename / no conflict) → the
    /// target is guaranteed to be free.
    pub(super) resolved: Vec<(PathBuf, PathBuf, bool)>,
    /// Source whose conflict is currently shown in the popup.
    pub(super) current: Option<PathBuf>,
    /// Keeps external staging data alive until this paste is resolved and its
    /// background copy/move has completed.
    pub(super) transient_cleanup: Option<TransientDropGuard>,
    /// Requested position when an internal drag targets a manual-order view.
    /// Clipboard and external OLE pastes leave it empty.
    pub(super) manual_insertion: Option<ManualOrderInsertion>,
}

impl PasteJob {
    /// `true` if this paste already sends one of its sources to `target`.
    /// Nothing exists on disk while the job is being arbitrated, so this is the
    /// only thing keeping two of its items off the same destination.
    ///
    /// The comparison follows the platform's case rules. The filesystem check
    /// beside it does so for free — `exists` answers for a name spelled
    /// differently — but that check cannot see a destination no item has
    /// written yet. Two names typed in the popup differing only in case would
    /// otherwise both be accepted, and the second copy would land on the first.
    pub(super) fn claims(&self, target: &Path) -> bool {
        self.resolved
            .iter()
            .any(|(_, dst, _)| ops::paths_equal(dst, target))
    }
}

/// Send events produced by the trash workers then consumed on the
/// Slint thread. `PathBuf`s remain native: no non-UTF-8 name is lost
/// during a delete/restore on Linux.
/// What a worker thread reports back once it has finished with an item, drained
/// on the UI thread where the application state is reachable. Named for the
/// operation rather than the trash: a move reports here too.
pub(super) enum OpDelivery {
    Trashed(PathBuf),
    /// An item the worker ACTUALLY moved, so its colour and its note can follow
    /// it. Reported per item rather than per operation: a move that fails
    /// halfway must carry only what really moved.
    Moved {
        from: PathBuf,
        to: PathBuf,
    },
    /// An item removed beyond recovery. The trash is NOT reported here — it can
    /// be restored, so its annotation is kept.
    PermanentlyDeleted(PathBuf),
    /// A path whose content was replaced by the operation. Its colour and note
    /// described what used to be there, and something else is there now, so
    /// they must not stay: a copy landing on the name would otherwise wear the
    /// annotation of the file it displaced. A move immediately writes the
    /// source's own annotation over it, which is the same rule seen from the
    /// other side.
    Replaced(PathBuf),
    RestoreFinished {
        /// Registry entry to release once the restore is applied.
        op_id: i32,
        original_path: PathBuf,
        error: Option<favnyr_core::fs::ops::TrashError>,
    },
    /// Ctrl+Z finished: what it managed to take back, item by item.
    UndoFinished {
        op_id: i32,
        done: Vec<undo::UndoEffect>,
        failed: usize,
    },
}

/// A long operation in flight, tracked in `OpRegistry` under a unique id.
/// Keying by id (rather than a single "one is running" flag) lets the toast,
/// the cancel button and the completion handler each target one exact operation.
pub(super) struct OpHandle {
    /// Cooperative cancellation flag shared with the worker thread. `None` for
    /// operations that cannot be interrupted (restoring from the trash).
    pub(super) cancel: Option<Arc<AtomicBool>>,
    /// Item to highlight (selection + scroll) once THIS operation finishes: the
    /// final target of a paste/move. A full path, since the active view may have
    /// changed folder during the copy — in that case nothing is highlighted.
    pub(super) pending_focus: Option<PathBuf>,
    /// Paths this operation WRITES to. Held until it completes so that another
    /// operation started meanwhile cannot resolve to the same destination.
    /// Deletions write nothing and reserve nothing.
    pub(super) targets: Vec<PathBuf>,
    /// Source and destination names whose content-thumbnail identity may have
    /// changed when this operation finishes.
    pub(super) thumbnail_invalidations: Vec<PathBuf>,
    /// Held until the worker reports completion, then dropped before refresh.
    pub(super) transient_cleanup: Option<TransientDropGuard>,
    /// Optional placement requested by an internal drop into a manual-order
    /// view. Applied only to targets the worker records as created.
    pub(super) manual_insertion: Option<ManualOrderInsertion>,
    /// Top-level destinations actually produced by the worker. Sharing this
    /// small vector avoids changing the progress and OLE delivery protocols.
    pub(super) completed_targets: Arc<Mutex<Vec<PathBuf>>>,
}

/// Key a destination is reserved under.
///
/// Reservations are matched by hash, so the platform's case rules have to be
/// folded into the key rather than into the comparison. Without it, a name
/// typed in the conflict popup differing only in case from one a running
/// operation is about to write would read as free — `Path::exists` cannot help
/// there, since neither file is on disk yet — and both would land on the same
/// one.
pub(super) fn reservation_key(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(path.to_string_lossy().to_lowercase())
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

/// The long operations currently running, each under a session-unique id.
#[derive(Default)]
pub(super) struct OpRegistry {
    pub(super) active: RefCell<HashMap<i32, OpHandle>>,
    /// Union of every in-flight operation's `targets`, for O(1) lookup.
    /// A destination is claimed as soon as its operation starts, well before the
    /// file physically exists — that is precisely the window in which a second
    /// operation would otherwise pick the very same name.
    pub(super) reserved: RefCell<HashSet<PathBuf>>,
    /// Last id handed out; ids start at 1, so 0 always means "no operation".
    pub(super) serial: Cell<i32>,
}

impl OpRegistry {
    /// `true` while at least one operation is in flight.
    pub(super) fn in_flight(&self) -> bool {
        !self.active.borrow().is_empty()
    }

    /// Registers an operation and returns the id identifying it until it
    /// completes. Ids are never reused within a session, so a late completion
    /// can't release an operation started afterwards.
    pub(super) fn register(&self, handle: OpHandle) -> i32 {
        let id = self.serial.get().wrapping_add(1).max(1);
        self.serial.set(id);
        self.reserved
            .borrow_mut()
            .extend(handle.targets.iter().map(|target| reservation_key(target)));
        self.active.borrow_mut().insert(id, handle);
        id
    }

    /// Removes a finished operation, releases the destinations it held and
    /// hands back its handle, whose `pending_focus` the caller consumes. `None`
    /// for an unknown id (already released — a completion delivered twice).
    pub(super) fn finish(&self, id: i32) -> Option<OpHandle> {
        let handle = self.active.borrow_mut().remove(&id)?;
        let mut reserved = self.reserved.borrow_mut();
        for target in &handle.targets {
            reserved.remove(&reservation_key(target));
        }
        Some(handle)
    }

    /// `true` if a running operation has already claimed this destination.
    /// Used alongside `Path::exists` wherever a free name is picked, so the
    /// answer covers files that are about to exist.
    pub(super) fn is_reserved(&self, path: &Path) -> bool {
        self.reserved.borrow().contains(&reservation_key(path))
    }

    /// Raises the cancellation flag of one operation, if it is still running
    /// and interruptible. Unknown or uninterruptible ids are a no-op.
    pub(super) fn cancel(&self, id: i32) {
        if let Some(handle) = self.active.borrow().get(&id)
            && let Some(flag) = handle.cancel.as_ref()
        {
            flag.store(true, Ordering::Relaxed);
        }
    }

    pub(super) fn cancel_all(&self) {
        for handle in self.active.borrow().values() {
            if let Some(flag) = &handle.cancel {
                flag.store(true, Ordering::Relaxed);
            }
        }
    }
}

/// True if `name` cannot be used as a copy name: empty,
/// a path separator, `.`/`..`, or already present in the target folder.
pub(super) fn paste_name_invalid(state: &AppState, name: &str) -> bool {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return true;
    }
    match state.paste_job.borrow().as_ref() {
        Some(job) => {
            let target = job.dst_dir.join(name);
            target_taken(state, &target) || job.claims(&target)
        }
        None => true,
    }
}

/// `true` if a destination is unavailable: already on disk, or claimed by an
/// operation still running. Both are needed — checking only the filesystem
/// would let two concurrent pastes resolve to the very same path.
pub(super) fn target_taken(state: &AppState, path: &Path) -> bool {
    path.exists() || state.ops.is_reserved(path)
}

/// Distinct destinations for a whole batch, in order.
///
/// Each name is picked against `taken` AND against what earlier items of the
/// same batch already took. That last part matters: nothing is on disk yet
/// while the batch is being planned, so duplicating `x.txt` together with
/// `x - Copy01.txt` would otherwise hand both of them `x - Copy02.txt` and one
/// result would overwrite the other.
pub(super) fn plan_unique_targets(sources: &[PathBuf], taken: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut planned: Vec<PathBuf> = Vec::with_capacity(sources.len());
    // Set rather than a scan of `planned`: picking a name already probes the
    // filesystem candidate by candidate, and a batch of same-named items would
    // make a linear lookup grow on top of that.
    let mut claimed: HashSet<PathBuf> = HashSet::with_capacity(sources.len());
    for src in sources {
        let dst = ops::unique_sibling_where(src, |p| taken(p) || claimed.contains(p));
        claimed.insert(dst.clone());
        planned.push(dst);
    }
    planned
}

/// State of a name proposed in the Rename dialog. The distinction between
/// `Conflict` and `ReplaceableFile` avoids displaying "Force replace"
/// for a syntactically invalid name or for a folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RenameNameStatus {
    Valid,
    Invalid,
    Conflict,
    ReplaceableFile,
}

/// Availability of a name in a folder, shared by the Create and
/// Rename dialogs. `ExistingNonDirectory` includes files and links: a
/// directory entry, even a broken link, always occupies its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EntryNameAvailability {
    Available,
    Invalid,
    ExistingNonDirectory,
    ExistingDirectory,
    /// Nothing on disk yet, but a running operation will write there. Occupied
    /// for every purpose, and never replaceable: there is no entry to replace.
    Reserved,
}

/// `entry_name_availability` widened to the destinations of operations in
/// flight. A name a running copy is about to write is not free either, even
/// though nothing occupies it on disk yet — creating or renaming onto it would
/// have that entry overwritten as soon as the operation reaches it.
pub(super) fn entry_name_availability_now(
    state: &AppState,
    parent: &Path,
    name: &str,
) -> EntryNameAvailability {
    let on_disk = entry_name_availability(parent, name);
    with_reservation(on_disk, state.ops.is_reserved(&parent.join(name)))
}

/// Folds a reservation into a filesystem-only verdict. Only a name that was
/// otherwise free becomes `Reserved`: a real entry keeps its own kind, which is
/// what the dialogs report on.
pub(super) fn with_reservation(on_disk: EntryNameAvailability, reserved: bool) -> EntryNameAvailability {
    match on_disk {
        EntryNameAvailability::Available if reserved => EntryNameAvailability::Reserved,
        other => other,
    }
}

pub(super) fn entry_name_availability(parent: &Path, name: &str) -> EntryNameAvailability {
    if ops::check_file_name(name).is_err() {
        return EntryNameAvailability::Invalid;
    }
    match std::fs::symlink_metadata(parent.join(name)) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => EntryNameAvailability::Available,
        // A name the OS refuses outright never reached the disk, so there is no
        // entry occupying it: reporting a conflict would send the user looking
        // for a duplicate that does not exist. `check_file_name` catches these
        // beforehand; this keeps the verdict honest for whatever it cannot know
        // in advance, such as a name a specific filesystem rejects on its own.
        Err(err) if err.kind() == std::io::ErrorKind::InvalidFilename => {
            EntryNameAvailability::Invalid
        }
        Err(_) => EntryNameAvailability::ExistingNonDirectory,
        Ok(meta) if meta.is_dir() => EntryNameAvailability::ExistingDirectory,
        Ok(_) => EntryNameAvailability::ExistingNonDirectory,
    }
}

/// Publishes a verdict on the typed name to the Create popup: what the confirm
/// button may do, and what the user is told. Single point of truth for the
/// three places that judge the name — the live check on each keystroke and the
/// two safeguards on confirmation, which must not word the same verdict
/// differently.
pub(super) fn push_create_name_status(
    window: &MainWindow,
    lang: Lang,
    name: &str,
    availability: EntryNameAvailability,
) {
    window.set_create_name_valid(availability != EntryNameAvailability::Invalid);
    window.set_create_conflict(matches!(
        availability,
        EntryNameAvailability::ExistingNonDirectory
            | EntryNameAvailability::ExistingDirectory
            | EntryNameAvailability::Reserved
    ));
    window.set_create_name_error(name_error_text(lang, name, availability).into());
}

/// Translated explanation of why a name cannot be used, or `""` when it can.
/// The interface shows this verbatim, so the reason reaches the user instead of
/// the catch-all "already taken".
pub(super) fn name_error_text(lang: Lang, name: &str, availability: EntryNameAvailability) -> String {
    match availability {
        EntryNameAvailability::Available => String::new(),
        EntryNameAvailability::ExistingNonDirectory
        | EntryNameAvailability::ExistingDirectory
        | EntryNameAvailability::Reserved => i18n::tr(lang, "paste_conflict_taken"),
        EntryNameAvailability::Invalid => match ops::check_file_name(name) {
            // A field the user has not filled in yet: the disabled button says
            // enough, an error message would only scold them for typing nothing.
            Ok(()) | Err(ops::NameRejection::Empty) => String::new(),
            Err(ops::NameRejection::DotEntry) => i18n::tr(lang, "name_error_dot_entry"),
            Err(ops::NameRejection::ReservedDevice) => i18n::tr(lang, "name_error_reserved"),
            Err(ops::NameRejection::TrailingDotOrSpace) => i18n::tr(lang, "name_error_trailing"),
            Err(ops::NameRejection::ForbiddenChar) => {
                i18n::tr(lang, "name_error_chars").replace("{chars}", ops::FORBIDDEN_NAME_CHARS)
            }
        },
    }
}

/// `rename_name_status` accounting for the operations in flight. Kept apart
/// from the pure form so the naming rules stay testable without app state.
pub(super) fn rename_name_status_now(state: &AppState, source: &Path, name: &str) -> RenameNameStatus {
    let on_disk = rename_name_status(source, name);
    let reserved = source
        .parent()
        .is_some_and(|parent| state.ops.is_reserved(&parent.join(name)));
    rename_with_reservation(on_disk, reserved)
}

/// Folds a reservation into a filesystem-only rename verdict. A destination a
/// running operation will write offers no entry to replace, so it downgrades to
/// a plain conflict and "Force replace" is never proposed for it.
pub(super) fn rename_with_reservation(on_disk: RenameNameStatus, reserved: bool) -> RenameNameStatus {
    match on_disk {
        RenameNameStatus::Valid if reserved => RenameNameStatus::Conflict,
        other => other,
    }
}

pub(super) fn rename_name_status(source: &Path, name: &str) -> RenameNameStatus {
    let Some(parent) = source.parent() else {
        return RenameNameStatus::Invalid;
    };
    let old = source.file_name().map(|n| n.to_string_lossy());
    if old.as_deref() == Some(name) {
        return RenameNameStatus::Valid;
    }
    // Case change on Windows: the "target" is the entry itself,
    // this isn't a destructive replacement. Purely lexical comparison,
    // without `canonicalize`: this check runs on every keystroke and shouldn't
    // add a network round-trip. (The `target` computation is ONLY used here → it
    // stays inside the Windows block so it isn't "unused" on Linux.)
    #[cfg(windows)]
    {
        let target = parent.join(name);
        if source
            .to_string_lossy()
            .eq_ignore_ascii_case(&target.to_string_lossy())
        {
            return RenameNameStatus::Valid;
        }
    }

    match entry_name_availability(parent, name) {
        EntryNameAvailability::Available => RenameNameStatus::Valid,
        EntryNameAvailability::Invalid => RenameNameStatus::Invalid,
        EntryNameAvailability::ExistingDirectory => RenameNameStatus::Conflict,
        EntryNameAvailability::Reserved => RenameNameStatus::Conflict,
        EntryNameAvailability::ExistingNonDirectory => {
            let Ok(source_meta) = std::fs::symlink_metadata(source) else {
                return RenameNameStatus::Conflict;
            };
            if source_meta.is_dir() {
                RenameNameStatus::Conflict
            } else {
                RenameNameStatus::ReplaceableFile
            }
        }
    }
}

/// UTF-8 offset expected by `TextInput::set_selection_offsets`.
///
/// For a file, the caret is placed before the extension recognized by the same
/// rule as display and duplication (`fs::ops::split_name`). Dotfiles
/// and purely numeric suffixes therefore remain whole names. A
/// folder is never split, even if its name contains a period.
pub(super) fn filename_caret_offset(name: &str, is_dir: bool) -> i32 {
    if is_dir {
        return byte_offset(name.len());
    }
    let (stem, extension) = ops::split_name(name);
    byte_offset(if extension.is_empty() {
        name.len()
    } else {
        stem.len()
    })
}

/// A byte length as a caret offset.
///
/// Slint counts selection offsets in bytes, not in characters, so every
/// offset handed to it is a `str::len` and goes through here. The clamp is
/// the only reason this is a function: a path long enough to overflow an
/// `i32` would otherwise wrap into a negative offset.
pub(super) fn byte_offset(len: usize) -> i32 {
    len.min(i32::MAX as usize) as i32
}

/// Advances the `PasteJob`: opens the popup for the next conflict, or
/// executes the operation if all conflicts are resolved.
/// Resolves a conflict by **overwriting**: the target keeps its original name and will be
/// replaced (the worker removes the existing item before the copy). Safeguard: a
/// "self-overwrite" (copying a file into its own folder) is
/// ignored — never delete the source.
pub(super) fn resolve_replace(job: &mut PasteJob, src: PathBuf) {
    let Some(name) = src.file_name() else { return };
    let target = job.dst_dir.join(name);
    // Case-insensitively on Windows: paths reaching a paste can come from
    // another application's clipboard, which spells them however it likes. An
    // exact comparison would let a differently-spelled self-overwrite through,
    // and this branch is the one that has the source deleted.
    if ops::paths_equal(&target, &src) {
        return; // copy onto itself → we don't overwrite (the item is simply skipped)
    }
    // An earlier item of this same paste already aims there. "Replace" means
    // replacing what was in the folder, never what this operation has just put
    // there: honouring it would make the second copy destroy the first, and the
    // user would end up with one file where they pasted two. The popup no
    // longer offers it (see `advance_paste`); this refuses it outright, since
    // "Replace all" walks the queue without asking again.
    if job.claims(&target) {
        return;
    }
    job.resolved.push((src, target, true));
}

pub(super) fn advance_paste(window: &MainWindow, state: &AppState) {
    enum Step {
        Conflict {
            original: String,
            suggested: String,
            rename_only: bool,
            is_dir: bool,
        },
        Execute(Box<PasteJob>),
    }
    let step = {
        let mut guard = state.paste_job.borrow_mut();
        if guard.is_none() {
            return;
        }
        let has_pending = guard
            .as_ref()
            .map(|j| !j.pending.is_empty())
            .unwrap_or(false);
        if has_pending {
            let job = guard.as_mut().unwrap();
            let src = job.pending.pop_front().unwrap();
            let name = src
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            // The suggestion must also clear the destinations this same paste
            // has already resolved, none of which exist on disk yet.
            let base = job.dst_dir.join(&name);
            let suggested =
                ops::unique_sibling_where(&base, |p| target_taken(state, p) || job.claims(p))
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| name.clone());
            // Copy WITHIN THE SAME FOLDER (src already in dst_dir): overwrite =
            // destroy the source (neutralized), skip = cancel → only
            // renaming makes sense. We therefore hide Replace/Skip.
            // Renaming is the only meaningful answer in two cases: copying
            // within the folder the item already sits in — where replacing
            // would destroy the source and skipping equals cancelling — and a
            // destination an earlier item of this same paste already owns,
            // where replacing would destroy that one. Same reasoning as the
            // rename dialog, which never offers "Force replace" for a
            // destination a running operation has reserved.
            let rename_only = src
                .parent()
                .is_some_and(|parent| ops::paths_equal(parent, &job.dst_dir))
                || job.claims(&base);
            // `is_dir()` deliberately follows a symlink if present: in the
            // dialog, a link to a folder is handled like a folder.
            let is_dir = src.is_dir();
            job.current = Some(src);
            Step::Conflict {
                original: name,
                suggested,
                rename_only,
                is_dir,
            }
        } else {
            Step::Execute(Box::new(guard.take().unwrap()))
        }
    };
    match step {
        Step::Conflict {
            original,
            suggested,
            rename_only,
            is_dir,
        } => {
            window.set_paste_conflict_original(original.into());
            window.set_paste_conflict_name(suggested.into());
            window.set_paste_conflict_name_taken(false);
            window.set_paste_conflict_rename_only(rename_only);
            window.set_paste_conflict_is_dir(is_dir);
            window.set_paste_conflict_open(true);
            window.set_paste_conflict_focus_gen(
                window.get_paste_conflict_focus_gen().wrapping_add(1),
            );
        }
        Step::Execute(job) => {
            window.set_paste_conflict_open(false);
            execute_paste(window, state, *job);
        }
    }
}

/// Starts a paste operation (`op`) from `sources` to `dst_dir`:
/// splits into `resolved` (no conflict) / `pending` (conflict → popup), then
/// `advance_paste`. Shared by paste (clipboard) and drag'n'drop.
pub(super) fn begin_paste(
    window: &MainWindow,
    state: &AppState,
    op: ClipOp,
    dst_dir: PathBuf,
    sources: Vec<PathBuf>,
    manual_insertion: Option<ManualOrderInsertion>,
) {
    begin_paste_with_cleanup(window, state, op, dst_dir, sources, None, manual_insertion);
}

pub(super) fn begin_paste_with_cleanup(
    window: &MainWindow,
    state: &AppState,
    op: ClipOp,
    dst_dir: PathBuf,
    sources: Vec<PathBuf>,
    transient_cleanup: Option<TransientDropGuard>,
    manual_insertion: Option<ManualOrderInsertion>,
) {
    if shutdown::stopping(state) {
        return;
    }
    if dst_dir.as_os_str().is_empty() || sources.is_empty() {
        return;
    }
    // Refuses a second paste while one is still being resolved, and says so.
    // An OLE drop from another application reaches this point without the user
    // ever touching Favnyr's window, so a silent refusal looks exactly like a
    // drag that did nothing — and the staging tree holding the dropped bytes is
    // released on the way out, leaving no trace at all. The waiting dialog can
    // easily sit behind the application the drag came from.
    if state.paste_job.borrow().is_some() {
        warn!(
            destination = %dst_dir.display(),
            count = sources.len(),
            "a paste is still waiting on its name conflict: the new one is refused"
        );
        let lang = state.snapshot_config().language;
        notice(
            window,
            i18n::tr(lang, "paste_already_pending"),
            NoticeKind::Error,
        );
        return;
    }
    let source_order = sources.clone();
    let mut job = PasteJob {
        op,
        dst_dir: dst_dir.clone(),
        source_order,
        pending: std::collections::VecDeque::new(),
        resolved: Vec::new(),
        current: None,
        transient_cleanup,
        manual_insertion,
    };
    for src in sources {
        // Never write an item into itself or into one of its own descendants.
        // The walk would keep meeting the copy it has just created and recurse
        // until the path length or the disk gives out. The drag route refused
        // this on its own; the clipboard reached here unguarded, so the rule
        // now sits on the single funnel every route goes through.
        if ops::is_within(&dst_dir, &src) {
            continue;
        }
        // Ignores drops of an item onto itself / into its own folder.
        if matches!(op, ClipOp::Cut)
            && src
                .parent()
                .is_some_and(|parent| ops::paths_equal(parent, &dst_dir))
        {
            continue;
        }
        let Some(name) = src.file_name() else {
            continue;
        };
        let target = dst_dir.join(name);
        // Occupied means: on disk, claimed by a running operation, or already
        // taken by an earlier item of this very paste. The last case happens
        // with same-named sources from different folders (OS clipboard, drop
        // from another app) — without it both would land on the same path.
        if target_taken(state, &target) || job.claims(&target) {
            job.pending.push_back(src);
        } else {
            job.resolved.push((src, target, false));
        }
    }
    *state.paste_job.borrow_mut() = Some(job);
    advance_paste(window, state);
}

/// Executes the resolved copies/moves (targets guaranteed free) on a
/// background thread with a progress bar.
pub(super) fn execute_paste(window: &MainWindow, state: &AppState, mut job: PasteJob) {
    if job.resolved.is_empty() {
        // Everything was skipped/cancelled: nothing to execute, just refresh.
        refresh_listing(window, state, &job.dst_dir);
        return;
    }
    sort_resolved_by_source_order(&mut job.resolved, &job.source_order);
    let lang = state.snapshot_config().language;
    // Item to highlight once the copy/move finishes: the
    // 1st target (the order of `resolved` follows the original selection's order).
    // Sorting often places the newcomer off-screen → `op-finished` will bring it back into view.
    let pending_focus = job.resolved.first().map(|(_, dst, _)| dst.clone());
    let cut = matches!(job.op, ClipOp::Cut);
    let work = match job.op {
        ClipOp::Copy => Heavy::Copy(job.resolved),
        ClipOp::Cut => Heavy::Move(job.resolved),
    };
    start_heavy_op(
        window,
        state,
        work,
        lang,
        pending_focus,
        job.transient_cleanup,
        job.manual_insertion,
    );
    // The cut is consumed once the operation is under way, never before: its
    // paths are what the operation moves, and dropping them early would lose
    // the pending selection if the paste did not start.
    if cut {
        let mut clip = state.clipboard.borrow_mut();
        clip.paths.clear();
        clip.op = None;
    }
}

pub(super) fn sort_resolved_by_source_order(
    resolved: &mut [(PathBuf, PathBuf, bool)],
    source_order: &[PathBuf],
) {
    let ranks: HashMap<PathBuf, usize> = source_order
        .iter()
        .enumerate()
        .map(|(index, source)| (reservation_key(source), index))
        .collect();
    resolved.sort_by_key(|(source, _, _)| {
        ranks
            .get(&reservation_key(source))
            .copied()
            .unwrap_or(usize::MAX)
    });
}

// ---------- Long background operations (progress bar) ----------

pub(super) const OP_RUNNING: i32 = 1;
pub(super) const OP_SCAN: i32 = 2;
pub(super) const OP_SUCCESS: i32 = 3;
pub(super) const OP_ERROR: i32 = 4;
pub(super) const OP_CANCELLED: i32 = 5;

/// Work handed to the background thread. For Copy/Move, each item is
/// `(source, target, overwrite)`: `overwrite = true` → the existing target is
/// removed before the operation ("Replace" resolution).
pub(super) enum Heavy {
    Copy(Vec<(PathBuf, PathBuf, bool)>),
    Move(Vec<(PathBuf, PathBuf, bool)>),
    Trash(Vec<PathBuf>),
    PermanentDelete(Vec<PathBuf>),
    #[cfg(windows)]
    NativeMoveCleanup(Vec<crate::winddrag::NativeMoveSource>),
}

/// i18n labels captured (owned → Send) for the thread.
#[derive(Clone)]
pub(super) struct OpLabels {
    pub(super) running: String,
    pub(super) scanning: String,
    pub(super) done: String,
    pub(super) cancelled: String,
    pub(super) errors: String,
    pub(super) items: String,
}

pub(super) struct HeavyRuntime {
    pub(super) op_id: i32,
    pub(super) weak: slint::Weak<MainWindow>,
    pub(super) cancel: Arc<AtomicBool>,
    pub(super) lang: Lang,
    pub(super) labels: OpLabels,
    pub(super) op_deliveries: Arc<Mutex<VecDeque<OpDelivery>>>,
    pub(super) completed_targets: Arc<Mutex<Vec<PathBuf>>>,
}

/// Toasts drawn in the stack; the rest are summarised by the "+N others" chip.
/// Mirrors the bound used by the `for` loop over `ops` in the .slint.
pub(super) const MAX_VISIBLE_TOASTS: usize = 3;

/// Number of operation toasts the stack cannot display.
pub(super) fn hidden_toast_count(total: usize) -> usize {
    total.saturating_sub(MAX_VISIBLE_TOASTS)
}

/// Pushes a toast update from a worker thread to the UI thread.
///
/// `visible` carries the deferred-appearance threshold: below it the operation
/// gets no row at all, so a quick copy never flashes a toast.
#[allow(clippy::too_many_arguments)]
pub(super) fn push_op(
    weak: &slint::Weak<MainWindow>,
    op_id: i32,
    state_i: i32,
    visible: bool,
    progress: f32,
    title: String,
    detail: String,
    percent: String,
) {
    if !visible {
        return;
    }
    let row = OpProgress {
        id: op_id,
        state: state_i,
        progress,
        indeterminate: state_i == OP_SCAN,
        title: title.into(),
        detail: detail.into(),
        percent: percent.into(),
    };
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            w.invoke_op_progress(row);
        }
    });
}

/// Inserts or refreshes one operation's toast row, keyed by its id.
pub(super) fn upsert_op_row(state: &AppState, row: OpProgress) {
    let model = &state.ops_model;
    let existing =
        (0..model.row_count()).find(|&i| model.row_data(i).map(|r| r.id) == Some(row.id));
    match existing {
        Some(i) => model.set_row_data(i, row),
        None => model.push(row),
    }
}

/// Removes one operation's toast row, if it still has one.
pub(super) fn remove_op_row(state: &AppState, op_id: i32) {
    let model = &state.ops_model;
    if let Some(i) =
        (0..model.row_count()).find(|&i| model.row_data(i).map(|r| r.id) == Some(op_id))
    {
        model.remove(i);
    }
}

/// Refreshes what depends on the toast set as a whole: the "+N others" chip.
pub(super) fn refresh_ops_ui(window: &MainWindow, state: &AppState) {
    let hidden = hidden_toast_count(state.ops_model.row_count());
    let text = if hidden == 0 {
        String::new()
    } else {
        i18n::op_more_text(state.snapshot_config().language, hidden)
    };
    window.set_ops_more_text(text.into());
}

/// Publishes whether anything heavy is in flight. Drives the deferred
/// refreshes, which must not re-list while an operation is writing.
pub(super) fn sync_op_busy(window: &MainWindow, state: &AppState) {
    window.set_op_busy(state.ops.in_flight());
}

/// Settling delay before re-listing once operations complete. Short enough to
/// read as instant, long enough to absorb a burst of completions.
pub(super) const OP_REFRESH_DEBOUNCE_MS: u64 = 150;

thread_local! {
    static OP_REFRESH_DEBOUNCE: slint::Timer = slint::Timer::default();
}

/// Requests the re-listing that follows a completed operation.
///
/// `refresh_all_panels` re-reads every local panel synchronously, so running it
/// once per completion would be wasteful now that operations finish
/// independently. Restarting a single-shot timer collapses a burst into one
/// pass. A stream of completions closer together than the delay keeps pushing
/// it back, which is the intended trade: the views stay untouched while things
/// are still moving, then catch up in a single re-listing.
pub(super) fn request_op_refresh(window: &MainWindow) {
    let weak = window.as_weak();
    OP_REFRESH_DEBOUNCE.with(|timer| {
        timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(OP_REFRESH_DEBOUNCE_MS),
            move || {
                if let Some(w) = weak.upgrade() {
                    w.invoke_refresh_all();
                }
            },
        );
    });
}

/// Selects and scrolls to the item left behind by the last operation to finish,
/// once the views have been re-listed. Only acts if the ACTIVE view is really
/// showing its folder: the user may have navigated elsewhere during a long
/// copy, and the selection of an unrelated folder is never moved.
pub(super) fn apply_focus_after_refresh(window: &MainWindow, state: &AppState) {
    let Some(target) = state.focus_after_refresh.borrow_mut().take() else {
        return;
    };
    let (Some(dir), Some(name)) = (target.parent(), target.file_name()) else {
        return;
    };
    if state.current_path() == dir {
        focus_entry_by_name(window, state, &name.to_string_lossy());
    }
}

/// Deposits a trash result into the Send queue then wakes its single
/// Slint consumer. Recovering from a poisoned mutex avoids permanently
/// losing the ability to cancel a deletion.
pub(super) fn deliver_op_event(
    weak: &slint::Weak<MainWindow>,
    deliveries: &Arc<std::sync::Mutex<VecDeque<OpDelivery>>>,
    event: OpDelivery,
) {
    deliveries
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push_back(event);
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            w.invoke_process_op_events();
        }
    });
}

/// Starts a long operation: registers it, prepares the cancellation flag + the
/// toast, then launches the background thread. Operations run concurrently —
/// each gets its own thread, its own toast and its own cancel button — so this
/// never turns work away.
pub(super) fn start_heavy_op(
    window: &MainWindow,
    state: &AppState,
    work: Heavy,
    lang: Lang,
    pending_focus: Option<PathBuf>,
    transient_cleanup: Option<TransientDropGuard>,
    manual_insertion: Option<ManualOrderInsertion>,
) {
    if shutdown::stopping(state) {
        return;
    }
    // Destinations are claimed for the whole run: a paste started meanwhile
    // must see these names as taken even though nothing exists on disk yet.
    // Deletions write nowhere, so they claim nothing.
    let targets = match &work {
        Heavy::Copy(items) | Heavy::Move(items) => {
            items.iter().map(|(_, dst, _)| dst.clone()).collect()
        }
        Heavy::Trash(_) | Heavy::PermanentDelete(_) => Vec::new(),
        #[cfg(windows)]
        Heavy::NativeMoveCleanup(_) => Vec::new(),
    };
    let thumbnail_invalidations = match &work {
        Heavy::Copy(items) => items.iter().map(|(_, dst, _)| dst.clone()).collect(),
        Heavy::Move(items) => items
            .iter()
            .flat_map(|(src, dst, _)| [src.clone(), dst.clone()])
            .collect(),
        Heavy::Trash(paths) | Heavy::PermanentDelete(paths) => paths.clone(),
        #[cfg(windows)]
        Heavy::NativeMoveCleanup(sources) => {
            sources.iter().map(|source| source.path.clone()).collect()
        }
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let completed_targets = Arc::new(Mutex::new(Vec::new()));
    let op_id = state.ops.register(OpHandle {
        cancel: Some(cancel.clone()),
        pending_focus,
        targets,
        thumbnail_invalidations,
        transient_cleanup,
        manual_insertion,
        completed_targets: completed_targets.clone(),
    });

    // Remember what this operation was ASKED to do. `on_op_finished` compares
    // that against the disk to build the undo entry, so a cancelled or
    // half-failed run records only what really happened.
    let plan = match &work {
        Heavy::Copy(items) => Some(undo::UndoPlan::Copy(
            items.iter().map(|(_, dst, _)| dst.clone()).collect(),
        )),
        Heavy::Move(items) => Some(undo::UndoPlan::Move(
            items
                .iter()
                .map(|(src, dst, _)| (src.clone(), dst.clone()))
                .collect(),
        )),
        Heavy::Trash(paths) => Some(undo::UndoPlan::Trash(paths.clone())),
        // Nothing brings a permanent delete back, so it is never offered.
        Heavy::PermanentDelete(_) => None,
        #[cfg(windows)]
        Heavy::NativeMoveCleanup(_) => None,
    };
    undo::remember_plan(state, op_id, plan);
    let running = match work {
        Heavy::Copy(_) => i18n::tr(lang, "op_copying"),
        Heavy::Move(_) => i18n::tr(lang, "op_moving"),
        Heavy::Trash(_) => i18n::tr(lang, "op_deleting"),
        Heavy::PermanentDelete(_) => i18n::tr(lang, "op_deleting_permanently"),
        #[cfg(windows)]
        Heavy::NativeMoveCleanup(_) => i18n::tr(lang, "op_moving"),
    };
    let labels = OpLabels {
        running,
        scanning: i18n::tr(lang, "op_scanning"),
        done: i18n::tr(lang, "op_done"),
        cancelled: i18n::tr(lang, "op_cancelled"),
        errors: i18n::tr(lang, "op_errors"),
        items: i18n::tr(lang, "op_items"),
    };

    // No row until the deferred appearance threshold (400 ms) — `push_op`
    // creates it on the first update that passes it.
    sync_op_busy(window, state);

    let weak = window.as_weak();
    let op_deliveries = state.op_deliveries.clone();
    std::thread::spawn(move || {
        run_heavy(
            work,
            HeavyRuntime {
                op_id,
                weak,
                cancel,
                lang,
                labels,
                op_deliveries,
                completed_targets,
            },
        )
    });
}

pub(super) fn run_heavy(work: Heavy, runtime: HeavyRuntime) {
    let HeavyRuntime {
        op_id,
        weak,
        cancel,
        lang,
        labels,
        op_deliveries,
        completed_targets,
    } = runtime;
    let start = Instant::now();
    let shown = || start.elapsed() >= Duration::from_millis(400);
    let is_cancelled = || cancel.load(Ordering::Relaxed);
    let mut last = Instant::now() - Duration::from_millis(500);
    let mut had_error = false;
    let mut cancelled = false;
    let mut failure_notice: Option<String> = None;
    // First entry the copy had to step over, kept with its failure category. Only
    // the first: a toast cannot list a folder's worth of refusals, and the file
    // that blocked first is the one the user has to act on.
    let mut first_skipped: Option<(PathBuf, Option<std::io::ErrorKind>)> = None;

    #[cfg(windows)]
    let native_sources = match &work {
        Heavy::NativeMoveCleanup(sources) => sources.clone(),
        _ => Vec::new(),
    };
    // Breaks down into (kind, copy/move items, delete paths).
    // kind 2 = trash; kind 3 = explicit deletion; kind 4 = native move cleanup.
    let (kind, items, delete_paths): (u8, Vec<(PathBuf, PathBuf, bool)>, Vec<PathBuf>) = match work
    {
        Heavy::Copy(v) => (0, v, Vec::new()),
        Heavy::Move(v) => (1, v, Vec::new()),
        Heavy::Trash(v) => (2, Vec::new(), v),
        Heavy::PermanentDelete(v) => (3, Vec::new(), v),
        #[cfg(windows)]
        Heavy::NativeMoveCleanup(sources) => (
            4,
            Vec::new(),
            sources.into_iter().map(|source| source.path).collect(),
        ),
    };

    if kind >= 2 {
        // Trash, permanent deletion and native move cleanup progress in items.
        let total = delete_paths.len();
        let mut done = 0usize;
        for p in &delete_paths {
            if is_cancelled() {
                cancelled = true;
                break;
            }
            let result = if kind >= 3 {
                #[cfg(windows)]
                let outcome = if kind == 4 {
                    native_sources[done].finish_move()
                } else {
                    ops::permanent_delete(p)
                };
                #[cfg(not(windows))]
                let outcome = ops::permanent_delete(p);
                if outcome.is_ok() {
                    deliver_op_event(
                        &weak,
                        &op_deliveries,
                        OpDelivery::PermanentlyDeleted(p.clone()),
                    );
                }
                outcome
            } else {
                match ops::trash_with_disposition(p) {
                    Ok(ops::TrashDisposition::Trashed) => {
                        deliver_op_event(&weak, &op_deliveries, OpDelivery::Trashed(p.clone()));
                        Ok(())
                    }
                    Ok(ops::TrashDisposition::PermanentlyDeleted) => {
                        // The trash was unavailable and the item went outright.
                        // Nothing brings it back, so its annotation goes too.
                        deliver_op_event(
                            &weak,
                            &op_deliveries,
                            OpDelivery::PermanentlyDeleted(p.clone()),
                        );
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            };
            if let Err(err) = result {
                had_error = true;
                error!(error = %err, path = %p.display(), permanent = kind >= 3, "delete failed");
                // Diagnostic triggered ONLY after the failure: no probe,
                // enumeration, or Restart Manager session on the normal path.
                // A single toast is enough even for a multi-selection.
                if failure_notice.is_none() {
                    failure_notice = Some(if kind == 4 {
                        i18n::file_operation_failed(
                            lang,
                            "native_move_failed",
                            &path_notice_name(p),
                            &i18n::file_error_reason(lang, file_error_kind(&err)),
                        )
                    } else {
                        delete_failure_notice(p, lang, file_error_kind(&err))
                    });
                }
            }
            done += 1;
            if last.elapsed() >= Duration::from_millis(80) || done == total {
                last = Instant::now();
                let pct = (done * 100).checked_div(total).unwrap_or(100);
                push_op(
                    &weak,
                    op_id,
                    OP_RUNNING,
                    shown(),
                    if total > 0 {
                        done as f32 / total as f32
                    } else {
                        1.0
                    },
                    labels.running.clone(),
                    format!("{done} / {total} {}", labels.items),
                    format!("{pct} %"),
                );
            }
        }
    } else {
        // Copy / move — progress in bytes after a pre-scan.
        push_op(
            &weak,
            op_id,
            OP_SCAN,
            shown(),
            0.0,
            labels.scanning.clone(),
            String::new(),
            String::new(),
        );
        let mut sizes = Vec::with_capacity(items.len());
        let mut total = 0u64;
        for (src, _, _) in &items {
            if is_cancelled() {
                cancelled = true;
                break;
            }
            let sz = ops::path_size(src);
            sizes.push(sz);
            total += sz;
            if last.elapsed() >= Duration::from_millis(80) {
                last = Instant::now();
                push_op(
                    &weak,
                    op_id,
                    OP_SCAN,
                    shown(),
                    0.0,
                    labels.scanning.clone(),
                    String::new(),
                    String::new(),
                );
            }
        }

        let mut done: u64 = 0;
        let transfer_start = Instant::now();
        let mut transfer = TransferMeter::new();
        if !cancelled {
            for (i, (src, target, overwrite)) in items.iter().enumerate() {
                if is_cancelled() {
                    cancelled = true;
                    break;
                }
                // Overwrite ("Replace" resolution) → remove the existing
                // target BEFORE the operation, otherwise `rename`/`create_dir`
                // fail on an occupied target. Trash (recoverable) with a
                // fallback to permanent deletion. Both failing → skip
                // the item (never copy over a target that wasn't removed).
                if *overwrite && target.exists() {
                    if let Err(err) = ops::trash(target).or_else(|_| ops::permanent_delete(target))
                    {
                        had_error = true;
                        error!(error = %err, target = %target.display(), "overwrite: remove existing failed");
                        if failure_notice.is_none() {
                            failure_notice =
                                Some(delete_failure_notice(target, lang, file_error_kind(&err)));
                        }
                        continue;
                    }
                    deliver_op_event(&weak, &op_deliveries, OpDelivery::Replaced(target.clone()));
                }
                let mut on_bytes = |delta: u64| {
                    done += delta;
                    transfer.add_bytes(delta);
                    if last.elapsed() >= Duration::from_millis(80) {
                        last = Instant::now();
                        let pct = (done.min(total) * 100).checked_div(total).unwrap_or(0) as u32;
                        let mut detail = format!(
                            "{} / {}",
                            rfs::format_size(done, i18n::size_units(lang)),
                            rfs::format_size(total, i18n::size_units(lang))
                        );
                        if let Some(estimate) =
                            transfer.estimate(transfer_start.elapsed(), total.saturating_sub(done))
                        {
                            detail.push('\n');
                            detail.push_str(&i18n::transfer_status(
                                lang,
                                estimate.bytes_per_second,
                                estimate.remaining_seconds,
                            ));
                        }
                        push_op(
                            &weak,
                            op_id,
                            OP_RUNNING,
                            shown(),
                            if total > 0 {
                                (done as f32 / total as f32).min(1.0)
                            } else {
                                0.0
                            },
                            labels.running.clone(),
                            detail,
                            format!("{pct} %"),
                        );
                    }
                };
                if kind == 1 {
                    // Move: fast rename, otherwise copy+delete.
                    match std::fs::rename(src, target) {
                        Ok(()) => {
                            done += sizes.get(i).copied().unwrap_or(0);
                            record_completed_target(&completed_targets, target);
                            deliver_op_event(
                                &weak,
                                &op_deliveries,
                                OpDelivery::Moved {
                                    from: src.clone(),
                                    to: target.clone(),
                                },
                            );
                        }
                        Err(_) => {
                            let mut skipped_here = 0_usize;
                            let copied = ops::copy_tree_progress(
                                src,
                                target,
                                &mut on_bytes,
                                &mut |path, err| {
                                    skipped_here += 1;
                                    error!(
                                        error = %err,
                                        path = %path.display(),
                                        "move: entry skipped"
                                    );
                                    if first_skipped.is_none() {
                                        first_skipped =
                                            Some((path.to_path_buf(), file_error_kind(err)));
                                    }
                                },
                                &is_cancelled,
                            );
                            match copied {
                                Ok(ops::OpStatus::Cancelled) => cancelled = true,
                                // Part of the tree stayed behind: removing the
                                // source would destroy exactly what could not be
                                // copied. The move then stops at a copy, which
                                // the final message says.
                                Ok(ops::OpStatus::Done) if skipped_here > 0 => {
                                    record_completed_target(&completed_targets, target);
                                    had_error = true;
                                    // The source is deliberately kept, so the
                                    // item now exists in both places. Saying so
                                    // matters: the generic "skipped" message
                                    // would let the user believe the rest of the
                                    // move went through, when nothing moved at
                                    // all — the target only holds a copy.
                                    if failure_notice.is_none()
                                        && let Some((path, error)) = first_skipped.as_ref()
                                    {
                                        failure_notice = Some(i18n::move_source_kept(
                                            lang,
                                            &path_notice_name(src),
                                            &lock_reason(path, lang, *error),
                                        ));
                                    }
                                }
                                Ok(ops::OpStatus::Done) => {
                                    record_completed_target(&completed_targets, target);
                                    if let Err(err) = ops::permanent_delete(src) {
                                        // The copy landed but the original stayed
                                        // behind: the move degenerated into a copy,
                                        // so the operation did NOT succeed. Reporting
                                        // it as done would leave two identical items
                                        // with no hint which one is authoritative.
                                        had_error = true;
                                        error!(
                                            error = %err,
                                            src = %src.display(),
                                            "move: delete source failed"
                                        );
                                        if failure_notice.is_none() {
                                            failure_notice = Some(move_source_kept_notice(
                                                src,
                                                lang,
                                                file_error_kind(&err),
                                            ));
                                        }
                                    } else {
                                        // Copied, then the original removed: the
                                        // move really happened.
                                        deliver_op_event(
                                            &weak,
                                            &op_deliveries,
                                            OpDelivery::Moved {
                                                from: src.clone(),
                                                to: target.clone(),
                                            },
                                        );
                                    }
                                }
                                Err(err) => {
                                    had_error = true;
                                    error!(error = %err, src = %src.display(), "move failed");
                                    if first_skipped.is_none() {
                                        first_skipped = Some((src.clone(), file_error_kind(&err)));
                                    }
                                }
                            }
                        }
                    }
                } else {
                    // Copy.
                    let mut skipped_here = 0_usize;
                    let copied = ops::copy_tree_progress(
                        src,
                        target,
                        &mut on_bytes,
                        &mut |path, err| {
                            skipped_here += 1;
                            error!(
                                error = %err,
                                path = %path.display(),
                                "copy: entry skipped"
                            );
                            if first_skipped.is_none() {
                                first_skipped = Some((path.to_path_buf(), file_error_kind(err)));
                            }
                        },
                        &is_cancelled,
                    );
                    match copied {
                        Ok(ops::OpStatus::Cancelled) => cancelled = true,
                        Ok(ops::OpStatus::Done) => {
                            record_completed_target(&completed_targets, target);
                        }
                        Err(err) => {
                            had_error = true;
                            error!(error = %err, src = %src.display(), "copy failed");
                            if first_skipped.is_none() {
                                first_skipped = Some((src.clone(), file_error_kind(&err)));
                            }
                        }
                    }
                    // Entries stepped over are failures too: the operation
                    // finishes, but it did not do everything it was asked.
                    if skipped_here > 0 {
                        had_error = true;
                    }
                }
                if cancelled {
                    break;
                }
            }
        }
    }

    // An entry stepped over is worth a message: the operation looks finished,
    // and only this says what did not make it through. A notice already set
    // (a move that kept its source) describes the same failure more precisely,
    // so it keeps the floor.
    if failure_notice.is_none()
        && let Some((path, error)) = first_skipped.as_ref()
    {
        failure_notice = Some(skipped_entry_notice(path, lang, *error));
    }

    // Final state.
    let final_state = if cancelled {
        OP_CANCELLED
    } else if had_error {
        OP_ERROR
    } else {
        OP_SUCCESS
    };
    let final_visible = final_state == OP_ERROR || shown();
    let title = match final_state {
        OP_CANCELLED => labels.cancelled.clone(),
        OP_ERROR => labels.errors.clone(),
        _ => labels.done.clone(),
    };
    let weak2 = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak2.upgrade() {
            if final_visible {
                w.invoke_op_progress(OpProgress {
                    id: op_id,
                    state: final_state,
                    progress: 1.0,
                    indeterminate: false,
                    title: title.into(),
                    detail: SharedString::default(),
                    percent: if final_state == OP_SUCCESS {
                        SharedString::from("100 %")
                    } else {
                        SharedString::default()
                    },
                });
            } else {
                // Finished below the appearance threshold: make sure no row lingers.
                w.invoke_op_dismiss(op_id);
            }
            if let Some(message) = failure_notice {
                show_notice(&w, message);
            }
            // Releases the operation, which then schedules the re-listing of ALL
            // views (an op can involve 2 panels: drag-drop source+target, or the
            // same folder open in 2 views).
            w.invoke_op_finished(op_id);
        }
    });
}

pub(super) fn record_completed_target(completed: &Arc<Mutex<Vec<PathBuf>>>, target: &Path) {
    completed
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(target.to_path_buf());
}
