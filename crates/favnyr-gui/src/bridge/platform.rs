//! Workarounds for defects in the platform and the rendering backend.
//!
//! Everything in this module exists because something below this layer does
//! not behave, and every item records three things: the symptom, what was tried
//! and did NOT work, and what would let it be deleted. That is deliberate —
//! these are the pieces most likely to be mistaken for ordinary logic during a
//! future refactor and removed, which brings the symptom straight back.
//!
//! Nothing here is business logic. If an item stops being needed, delete it.

use super::{AppState, apply_ui_scale};
use slint::ComponentHandle;

/// Rebuilds the window's rendering resources after it comes back to the
/// foreground (taskbar click, `Alt+Tab`, restore from minimised).
///
/// SYMPTOM: the window keeps its size, but a strip along an edge or a patch in
/// a corner is never painted, so whatever sits underneath shows through.
///
/// CAUSE: Slint's software rasteriser tracks damage itself; on re-exposure it
/// decides that region did not change and paints nothing. No application-level
/// handle exists for clearing that set.
///
/// TRIED, ALL INEFFECTIVE:
/// * `request_redraw` — a suggestion; the backend's damage set still decides.
/// * `InvalidateRect` + `UpdateWindow` — makes WINDOWS believe the window needs
///   painting, but Slint does the painting.
/// * a same-sized `Resized` — nothing changed, so nothing is recomputed.
/// * a real one-pixel resize, synchronously in one message — the net change is
///   zero, so the damage region comes out empty.
/// * the same, across two frames — still did not clear the gaps, and it left
///   the window in a state that required a revert.
///
/// WHAT WORKS: nudging the interface zoom down to 95% and restoring it a frame
/// later. `apply_ui_scale` dispatches `ScaleFactorChanged` + `Resized`, which
/// makes Slint rebuild every rendering resource — fonts, icons, layout, clip —
/// and that path does clear the gaps. The transient is short enough to be hard
/// to notice.
///
/// DELETE WHEN: upstream Slint stops leaving unpainted regions on re-exposure.
pub(crate) fn rebuild_after_foreground(
    window: &super::MainWindow,
    state: &AppState,
) {
    use std::sync::atomic::{AtomicBool, Ordering};

    // One nudge at a time: several activation messages can arrive in a burst.
    static NUDGING: AtomicBool = AtomicBool::new(false);
    if NUDGING.swap(true, Ordering::Relaxed) {
        return;
    }
    let current = state.snapshot_config().ui_scale.max(0.1);
    apply_ui_scale(window, state, current * 0.95);

    let weak = window.as_weak();
    let state = state.clone();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::SingleShot,
        std::time::Duration::from_millis(40),
        move || {
            NUDGING.store(false, Ordering::Relaxed);
            if let Some(window) = weak.upgrade() {
                apply_ui_scale(&window, &state, current);
            }
        },
    );
    // The timer must outlive this call.
    std::mem::forget(timer);
}
