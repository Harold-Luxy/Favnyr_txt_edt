//! Bounded history of the file operations the user can take back.
//!
//! Only the *facts needed to reverse* an operation are kept: which path moved
//! where, which paths the operation created, and which paths went to the
//! recycle bin. Nothing here touches the filesystem — the GUI crate performs
//! the reversal and decides what to report, which keeps this module testable
//! without a disk.
//!
//! Deliberately NOT recorded: anything irreversible. A permanent delete (a
//! network share, or Shift+Delete) leaves nothing behind to restore, so it
//! never enters the history — and it must not push the older entries out of a
//! bounded stack either, since that would silently cost the user the undo they
//! could still have used.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::SystemTime;

/// How many operations the history remembers.
///
/// Deep enough to cover a burst of tidying up, shallow enough that the oldest
/// entry is still something the user recognises.
pub const DEPTH: usize = 20;

/// An item that was moved or renamed from `from` to `to`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relocation {
    pub from: PathBuf,
    pub to: PathBuf,
}

/// An item an operation produced, which did not exist before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedItem {
    pub path: PathBuf,
    /// Size and modification time observed immediately after creation.
    ///
    /// Undo deletes what the operation created, but the user may have edited
    /// that copy since. Comparing these two tells "the copy we made" from
    /// "something the user has put work into", and the second is left alone.
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// One reversible operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Moved or renamed: undo sends `to` back to `from`.
    Relocated(Vec<Relocation>),
    /// Created by a copy or a "new file/folder": undo removes them.
    Created(Vec<CreatedItem>),
    /// Sent to the recycle bin: undo restores them.
    Trashed(Vec<PathBuf>),
}

impl Change {
    /// `true` when the operation touched nothing, so recording it would only
    /// cost the user a Ctrl+Z that does nothing.
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Relocated(items) => items.is_empty(),
            Self::Created(items) => items.is_empty(),
            Self::Trashed(paths) => paths.is_empty(),
        }
    }
}

/// Last-in-first-out history, capped at [`DEPTH`].
#[derive(Debug, Default)]
pub struct History {
    entries: VecDeque<Change>,
}

impl History {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an operation as the most recent one.
    ///
    /// An empty change is dropped rather than stored: a cancelled or fully
    /// skipped operation would otherwise leave an entry whose undo is a no-op,
    /// making Ctrl+Z look broken.
    pub fn record(&mut self, change: Change) {
        if change.is_empty() {
            return;
        }
        if self.entries.len() == DEPTH {
            self.entries.pop_front();
        }
        self.entries.push_back(change);
    }

    /// Removes and returns the most recent operation, for the caller to
    /// reverse. An entry is consumed BEFORE it is reversed: if the reversal
    /// fails, replaying it would be wrong anyway (the disk has moved on), and
    /// keeping it would make Ctrl+Z repeat the same failure forever.
    pub fn take_last(&mut self) -> Option<Change> {
        self.entries.pop_back()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relocation(name: &str) -> Change {
        Change::Relocated(vec![Relocation {
            from: PathBuf::from(format!("/a/{name}")),
            to: PathBuf::from(format!("/b/{name}")),
        }])
    }

    #[test]
    fn the_last_operation_comes_back_first() {
        let mut history = History::new();
        history.record(relocation("first"));
        history.record(relocation("second"));

        assert_eq!(history.take_last(), Some(relocation("second")));
        assert_eq!(history.take_last(), Some(relocation("first")));
        assert_eq!(history.take_last(), None);
    }

    #[test]
    fn the_stack_stops_at_its_depth_and_drops_the_oldest() {
        let mut history = History::new();
        for index in 0..DEPTH + 5 {
            history.record(relocation(&format!("item{index}")));
        }
        assert_eq!(history.len(), DEPTH);

        // The oldest kept entry is the first one inside the window.
        let mut oldest = None;
        while let Some(change) = history.take_last() {
            oldest = Some(change);
        }
        assert_eq!(oldest, Some(relocation("item5")));
    }

    #[test]
    fn an_operation_that_touched_nothing_is_not_recorded() {
        let mut history = History::new();
        history.record(Change::Relocated(Vec::new()));
        history.record(Change::Created(Vec::new()));
        history.record(Change::Trashed(Vec::new()));

        assert!(
            history.is_empty(),
            "a cancelled operation must not cost a Ctrl+Z"
        );
    }

    #[test]
    fn taking_consumes_the_entry_even_when_the_caller_fails_to_reverse_it() {
        let mut history = History::new();
        history.record(relocation("only"));

        assert!(history.take_last().is_some());
        assert!(
            history.take_last().is_none(),
            "a failed reversal must not be replayed forever"
        );
    }
}
