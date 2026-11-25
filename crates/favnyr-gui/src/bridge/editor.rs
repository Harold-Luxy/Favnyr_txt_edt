//! The editor buffer map: which panel holds which document.
//!
//! [`AppState::editor_buffers`] is keyed by PANEL INDEX, which is fragile in a
//! way that does not announce itself: removing a panel renumbers every later
//! one, and a map that is not shifted with it keeps answering for the wrong
//! view. `update_panels_ui` renders a missing entry as an empty document, so
//! the symptom is a note that looks wiped while its file is untouched — and an
//! unsaved edit written into the new empty buffer is gone for good.
//!
//! Every path that removes a panel goes through `drop_editor_buffer`.

use super::*;

/// Drops the editor buffer of the view at `removed` and moves every buffer
/// that sat after it down by one, because [`AppState::editor_buffers`] is keyed
/// by panel index and removing a panel renumbers the rest.
///
/// Every place that removes a panel owes this call. It is the ONLY bookkeeping
/// an editor view needs when the panel list shrinks, and skipping it fails
/// quietly rather than loudly: the view keeps its file, looks up a buffer
/// nobody wrote, and `update_panels_ui` renders that miss as an empty
/// document. The note then looks wiped while its file is untouched — and an
/// unsaved edit would be gone for good.
pub(super) fn drop_editor_buffer(state: &AppState, removed: usize) {
    shift_editor_buffers(&mut state.editor_buffers.borrow_mut(), removed);
}

pub(super) fn shift_editor_buffers(
    buffers: &mut std::collections::HashMap<usize, EditorBuffer>,
    removed: usize,
) {
    buffers.remove(&removed);
    if buffers.is_empty() {
        return;
    }
    let shifted: Vec<(usize, EditorBuffer)> = buffers
        .drain()
        .map(|(index, buffer)| {
            (
                if index > removed { index - 1 } else { index },
                buffer,
            )
        })
        .collect();
    buffers.extend(shifted);
}

/// The renumbering rule above, on its own.
///
/// It is exactly the kind of off-by-one that does not crash, so it gets its own
/// tests rather than relying on a manual click-through: a stale key renders as
/// an empty editor and looks like data loss.
#[cfg(test)]
mod editor_buffer_shift_tests {
    use super::{EditorBuffer, shift_editor_buffers};
    use std::collections::HashMap;

    fn buffer(text: &str) -> EditorBuffer {
        EditorBuffer {
            text: text.to_string(),
            saved: text.to_string(),
        }
    }

    fn map(entries: &[(usize, &str)]) -> HashMap<usize, EditorBuffer> {
        entries
            .iter()
            .map(|(index, text)| (*index, buffer(text)))
            .collect()
    }

    #[test]
    fn removing_a_view_moves_the_later_buffers_down_with_it() {
        // The reported bug: an editor view opened beside the folder view it came
        // from, then that folder view closed. Without the shift the editor keeps
        // key 1 while its panel becomes 0, and its text disappears.
        let mut buffers = map(&[(1, "note")]);
        shift_editor_buffers(&mut buffers, 0);
        assert_eq!(buffers.get(&0).map(|b| b.text.as_str()), Some("note"));
        assert!(!buffers.contains_key(&1), "the stale key must not survive");
    }

    #[test]
    fn removing_the_view_that_came_last_leaves_the_others_alone() {
        let mut buffers = map(&[(0, "first"), (1, "second")]);
        shift_editor_buffers(&mut buffers, 1);
        assert_eq!(buffers.get(&0).map(|b| b.text.as_str()), Some("first"));
        assert_eq!(buffers.len(), 1);
    }

    #[test]
    fn removing_a_view_that_has_no_buffer_still_renumbers_the_rest() {
        // Closing a plain folder view must not disturb an editor that sits
        // after it.
        let mut buffers = map(&[(2, "note")]);
        shift_editor_buffers(&mut buffers, 1);
        assert_eq!(buffers.get(&1).map(|b| b.text.as_str()), Some("note"));
    }

    /// Two editors on either side of the removed view must not collide on the
    /// same key once the hole closes.
    #[test]
    fn two_editors_keep_their_own_text_when_a_view_between_them_closes() {
        let mut buffers = map(&[(0, "before"), (2, "after")]);
        shift_editor_buffers(&mut buffers, 1);
        assert_eq!(buffers.get(&0).map(|b| b.text.as_str()), Some("before"));
        assert_eq!(buffers.get(&1).map(|b| b.text.as_str()), Some("after"));
        assert_eq!(buffers.len(), 2);
    }
}

// ---- Loading, saving and flushing ----

/// Replaces the buffer of panel `idx` with the contents of `file`.
///
/// Decoding is LOSSY on purpose: a text file saved in a legacy code page must
/// still open (readable in ASCII, mangled elsewhere) rather than fail outright,
/// because the alternative is a view the user cannot reach.
pub(crate) fn load_editor_buffer(state: &AppState, idx: usize, file: &Path) -> anyhow::Result<()> {
    let len = std::fs::metadata(file)?.len();
    if len > MAX_EDITOR_BYTES {
        return Err(anyhow::anyhow!(
            "file is {len} bytes, above the {MAX_EDITOR_BYTES} limit"
        ));
    }
    let bytes = std::fs::read(file)?;
    // A UTF-8 BOM would otherwise show up as a stray character before the first
    // word, and be written back on save, doubling it every round trip.
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    let text = String::from_utf8_lossy(bytes).into_owned();

    let mut buffers = state.editor_buffers.borrow_mut();
    buffers.insert(
        idx,
        EditorBuffer {
            saved: text.clone(),
            text,
        },
    );
    Ok(())
}
/// Whether the editor at `idx` has unsaved changes.
pub(crate) fn editor_is_dirty(state: &AppState, idx: usize) -> bool {
    state
        .editor_buffers
        .borrow()
        .get(&idx)
        .is_some_and(EditorBuffer::is_dirty)
}
/// Writes the editor at `idx` back to its file, if it differs from disk.
///
/// Returns `true` when something was written. A failure is reported to the user
/// and the buffer is left dirty, so the next save (or the shutdown flush) tries
/// again instead of silently dropping the edits.
pub(crate) fn save_editor(state: &AppState, idx: usize) -> bool {
    let (path, text, dirty) = {
        let buffers = state.editor_buffers.borrow();
        let Some(buffer) = buffers.get(&idx) else {
            return false;
        };
        let Some(path) = state
            .panels
            .borrow()
            .get(idx)
            .and_then(|p| p.editor_file().map(Path::to_path_buf))
        else {
            return false;
        };
        (path, buffer.text.clone(), buffer.is_dirty())
    };
    if !dirty {
        return false;
    }

    // Written through a sibling temporary file then renamed: a crash or a full
    // disk mid-write would otherwise leave a truncated note where the original
    // used to be, which is the one outcome a note-taking view must not produce.
    let tmp = path.with_extension("favnyr-tmp");
    let written = std::fs::write(&tmp, text.as_bytes())
        .and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(err) = written {
        let _ = std::fs::remove_file(&tmp);
        warn!(error = %err, file = %path.display(), "saving the editor buffer failed");
        return false;
    }

    if let Some(buffer) = state.editor_buffers.borrow_mut().get_mut(&idx) {
        buffer.saved = text;
    }
    true
}

/// Saves every dirty editor view. Called on application shutdown, so edits made
/// in the last seconds before closing are not lost.
pub(crate) fn flush_editor_views(state: &AppState) {
    let dirty: Vec<usize> = {
        let buffers = state.editor_buffers.borrow();
        buffers
            .iter()
            .filter(|(_, b)| b.is_dirty())
            .map(|(idx, _)| *idx)
            .collect()
    };
    for idx in dirty {
        if save_editor(state, idx) {
            info!(panel = idx, "flushed editor buffer on shutdown");
        }
    }
}
