//! Undoing the last file operation (Ctrl+Z).
//!
//! Recording and reversing are deliberately separate:
//!
//! * Recording asks the DISK what an operation actually produced, instead of
//!   trusting what it was asked to do. A cancelled or half-failed copy then
//!   leaves a history entry that matches reality, and an operation that changed
//!   nothing leaves none at all.
//! * Reversing runs on a worker thread like every other long operation, then
//!   reports what it managed to do. The UI thread applies the annotation and
//!   manual-order bookkeeping through the SAME paths a real operation uses —
//!   see `UndoEffect`'s mapping in `process_op_events` — so colours, notes and
//!   manual order follow an undo exactly as they follow the original.

use std::path::PathBuf;

use favnyr_core::undo::{Change, CreatedItem, Relocation};

use super::*;

/// What an operation was ASKED to do, kept by `op_id` until it finishes.
///
/// Stored beside the operation registry rather than inside `OpHandle` so the
/// handle stays about the running operation; this is only needed once, at the
/// very end.
pub(super) enum UndoPlan {
    Copy(Vec<PathBuf>),
    Move(Vec<(PathBuf, PathBuf)>),
    Trash(Vec<PathBuf>),
}

/// What an undo actually managed to reverse, one item at a time.
pub(super) enum UndoEffect {
    /// The item is back at `from` (it had been moved to `to`).
    MovedBack { from: PathBuf, to: PathBuf },
    /// A copy or a new entry is gone again.
    RemovedCreated(PathBuf),
    /// The item is back out of the recycle bin.
    Restored(PathBuf),
}

/// Size and modification time of `path`, for [`CreatedItem`].
fn stamp(path: &Path) -> CreatedItem {
    let metadata = std::fs::metadata(path).ok();
    CreatedItem {
        path: path.to_path_buf(),
        size: metadata.as_ref().map(|m| m.len()).unwrap_or(0),
        modified: metadata.and_then(|m| m.modified().ok()),
    }
}

/// Keeps the plan of an operation that has just started.
pub(super) fn remember_plan(state: &AppState, op_id: i32, plan: Option<UndoPlan>) {
    if let Some(plan) = plan {
        state.undo_plans.borrow_mut().insert(op_id, plan);
    }
}

/// Turns a finished operation into a history entry, from what is on disk now.
pub(super) fn record_finished(state: &AppState, op_id: i32) {
    let Some(plan) = state.undo_plans.borrow_mut().remove(&op_id) else {
        return;
    };
    let change = match plan {
        // Only the destinations that really appeared: a cancelled copy creates
        // a prefix of the list, one entry per item.
        UndoPlan::Copy(destinations) => Change::Created(
            destinations
                .into_iter()
                .filter(|path| path.exists())
                .map(|path| stamp(&path))
                .collect(),
        ),
        // A move counts only when it both landed and left: a partially
        // cancelled run must not claim to have moved what is still in place.
        UndoPlan::Move(pairs) => Change::Relocated(
            pairs
                .into_iter()
                .filter(|(from, to)| to.exists() && !from.exists())
                .map(|(from, to)| Relocation { from, to })
                .collect(),
        ),
        UndoPlan::Trash(paths) => {
            Change::Trashed(paths.into_iter().filter(|path| !path.exists()).collect())
        }
    };
    state.undo_history.borrow_mut().record(change);
}

/// Records a completed rename (the only file operation that does not go
/// through the long-operation registry).
pub(super) fn remember_rename(state: &AppState, from: &Path, to: &Path) {
    state
        .undo_history
        .borrow_mut()
        .record(Change::Relocated(vec![Relocation {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
        }]));
}

/// Records a freshly created file or folder.
pub(super) fn remember_created(state: &AppState, path: &Path) {
    state
        .undo_history
        .borrow_mut()
        .record(Change::Created(vec![stamp(path)]));
}

/// Reverses one item, returning what happened to it.
fn reverse(change: Change) -> (Vec<UndoEffect>, usize) {
    let mut done = Vec::new();
    let mut failed = 0usize;
    match change {
        // Reversed in reverse order, so two entries touching the same name
        // cannot fight each other on the way back.
        Change::Relocated(pairs) => {
            for Relocation { from, to } in pairs.into_iter().rev() {
                // Something else now sits where the item came from. Moving
                // over it would destroy data the undo was never asked about.
                if from.exists() || ops::move_to(&to, &from).is_err() {
                    failed += 1;
                } else {
                    done.push(UndoEffect::MovedBack { from, to });
                }
            }
        }
        Change::Created(items) => {
            for item in items {
                if !item.path.exists() {
                    continue; // already gone: nothing left to take back
                }
                // A copy the user has since edited is their work now, not ours
                // to delete. Directories are not compared: their stamp would
                // only describe the folder itself, not what went into it.
                let edited = item.path.is_file()
                    && std::fs::metadata(&item.path).is_ok_and(|m| {
                        m.len() != item.size || m.modified().ok() != item.modified
                    });
                if edited || ops::trash(&item.path).is_err() {
                    failed += 1;
                } else {
                    done.push(UndoEffect::RemovedCreated(item.path));
                }
            }
        }
        Change::Trashed(paths) => {
            for path in paths.into_iter().rev() {
                if ops::restore_from_trash(&path).is_err() {
                    failed += 1;
                } else {
                    done.push(UndoEffect::Restored(path));
                }
            }
        }
    }
    (done, failed)
}

/// Ctrl+Z: take back the most recent recorded operation.
pub(super) fn undo_last(window: &MainWindow, state: &AppState) {
    if shutdown::stopping(state) {
        return;
    }
    let lang = state.snapshot_config().language;
    // The editor view owns its text: taking a file operation back from under a
    // half-written document would be the last thing the user expects, and
    // Slint's TextEdit has no undo of its own to defer to.
    let active = *state.active_panel.borrow();
    if state
        .panels
        .borrow()
        .get(active)
        .is_some_and(|panel| panel.editor_file().is_some())
    {
        return;
    }
    let Some(change) = state.undo_history.borrow_mut().take_last() else {
        notice(window, i18n::tr(lang, "undo_nothing"), NoticeKind::Success);
        return;
    };
    let op_id = state.ops.register(OpHandle {
        cancel: None,
        pending_focus: None,
        targets: Vec::new(),
        thumbnail_invalidations: Vec::new(),
        transient_cleanup: None,
        manual_insertion: None,
        completed_targets: Arc::new(Mutex::new(Vec::new())),
    });
    sync_op_busy(window, state);
    let deliveries = state.op_deliveries.clone();
    let weak = window.as_weak();
    std::thread::spawn(move || {
        let (done, failed) = reverse(change);
        deliver_op_event(
            &weak,
            &deliveries,
            OpDelivery::UndoFinished {
                op_id,
                done,
                failed,
            },
        );
    });
}

/// Message for a finished undo, or `None` when there is nothing worth saying.
pub(super) fn undo_message(lang: Lang, done: usize, failed: usize) -> Option<String> {
    if done == 0 && failed == 0 {
        return None;
    }
    if failed == 0 {
        return Some(i18n::tr(lang, "undo_done").replace("{count}", &done.to_string()));
    }
    Some(
        i18n::tr(lang, "undo_partial")
            .replace("{done}", &done.to_string())
            .replace("{failed}", &failed.to_string()),
    )
}
