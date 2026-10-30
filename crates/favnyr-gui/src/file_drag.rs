//! File-transfer intent shared by internal drags and Windows OLE drops.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum DropAction {
    Choose = -1,
    Move = 0,
    Copy = 1,
}

impl DropAction {
    /// Transfer intent carried by the modifier keys at drag time.
    ///
    /// A plain drag MOVES. Dragging a file onto a folder is the ordinary "put
    /// it in there" gesture, and answering a Move/Copy/Link menu every time
    /// turned it into a two-step action; the menu is the exception now, not
    /// the default. Ctrl copies, as it does everywhere else on this platform.
    ///
    /// Holding BOTH modifiers asks for that explicit menu. It is the one
    /// combination left over: Shift alone has to keep meaning what a plain
    /// drag means, and no single free modifier remains for Link — which is
    /// only reachable through the menu anyway.
    pub fn from_modifiers(control: bool, shift: bool) -> Self {
        if control && shift {
            Self::Choose
        } else if control {
            Self::Copy
        } else {
            Self::Move
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_drag_moves_and_ctrl_copies() {
        assert_eq!(DropAction::from_modifiers(false, false), DropAction::Move);
        assert_eq!(DropAction::from_modifiers(false, true), DropAction::Move);
        assert_eq!(DropAction::from_modifiers(true, false), DropAction::Copy);
    }

    #[test]
    fn only_both_modifiers_ask_for_the_action_menu() {
        // -1 in the UI: the one path that still offers Link.
        assert_eq!(DropAction::from_modifiers(true, true), DropAction::Choose);
    }
}
