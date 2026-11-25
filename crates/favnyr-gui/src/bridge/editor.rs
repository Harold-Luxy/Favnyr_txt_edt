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
