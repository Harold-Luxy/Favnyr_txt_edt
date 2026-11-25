//! Cross-instance IPC for Favnyr (Windows) — **tab transfer via
//! drag-and-drop between two Favnyr windows**. No dependency added:
//! raw FFI to `user32` / `comctl32` (system DLLs), like `places.rs`.
//!
//! Principle: each instance MARKS its window (`SetPropW`) and SUBCLASSES it
//! (`SetWindowSubclass`) to intercept `WM_COPYDATA`. On drop, the SOURCE
//! instance finds the Favnyr window under the cursor (`WindowFromPoint`) and
//! sends it the serialized tab (`SendMessageW(WM_COPYDATA)`). The target
//! extracts the payload in its subclassed procedure, then processes it on the
//! Slint UI thread (`invoke_from_event_loop`).
//!
//! `WM_COPYDATA` is SYNCHRONOUS: the subclassed procedure just COPIES the
//! payload and schedules its processing (immediate return → the source
//! doesn't block).

#[cfg(windows)]
mod imp {
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicIsize, Ordering};

    type Hwnd = isize;

    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }
    #[repr(C)]
    struct CopyDataStruct {
        dw_data: usize,
        cb_data: u32,
        lp_data: *const core::ffi::c_void,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn WindowFromPoint(pt: Point) -> Hwnd;
        fn GetAncestor(hwnd: Hwnd, flags: u32) -> Hwnd;
        fn SetPropW(hwnd: Hwnd, s: *const u16, data: isize) -> i32;
        fn GetPropW(hwnd: Hwnd, s: *const u16) -> isize;
        // Sending WITH TIMEOUT: if the target is frozen, we don't wait
        // indefinitely (SendMessageW would block), and we can verify it
        // actually PROCESSED the message (LRESULT) before removing the tab on
        // the source side.
        fn SendMessageTimeoutW(
            hwnd: Hwnd,
            msg: u32,
            wparam: usize,
            lparam: isize,
            flags: u32,
            timeout_ms: u32,
            result: *mut usize,
        ) -> isize;
        fn SetForegroundWindow(hwnd: Hwnd) -> i32;
        fn GetForegroundWindow() -> Hwnd;
        fn SetFocus(hwnd: Hwnd) -> Hwnd;
        fn ClientToScreen(hwnd: Hwnd, point: *mut Point) -> i32;
        fn ScreenToClient(hwnd: Hwnd, point: *mut Point) -> i32;
        fn EnumThreadWindows(
            thread_id: u32,
            cb: unsafe extern "system" fn(Hwnd, isize) -> i32,
            lparam: isize,
        ) -> i32;
        fn IsWindowVisible(hwnd: Hwnd) -> i32;
        fn GetWindow(hwnd: Hwnd, cmd: u32) -> Hwnd;
        // Cross-instance hover: ASYNCHRONOUS (fire-and-forget) → adds no
        // latency to the source's drag (unlike SendMessage).
        fn PostMessageW(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> i32;
        fn RegisterWindowMessageW(s: *const u16) -> u32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
    }
    // Geometry telemetry for the cross-monitor drag: a window that grows (and is
    // animated into its new rect, which reads as a snap) when it crosses screens
    // is either a correct DPI rescale or a scale applied twice, and only the
    // numbers tell them apart.
    #[repr(C)]
    #[derive(Default)]
    struct LogRect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        // Whole-client-area invalidation: the stronger sibling of the redraw
        // request, which the software rasteriser may fold into its own damage
        // tracking and then ignore.
        fn InvalidateRect(hwnd: Hwnd, rect: *const core::ffi::c_void, erase: i32) -> i32;
        fn UpdateWindow(hwnd: Hwnd) -> i32;
        fn SetWindowPos(hwnd: Hwnd,
            insert_after: isize,
            x: i32,
            y: i32,
            cx: i32,
            cy: i32,
            flags: u32,
        ) -> i32;
    }
    /// `SetWindowPos` flags: keep the z-order and activation untouched, and let
    /// the caller's frame stay put.
    const SWP_NOZORDER: u32 = 0x0004;
    const SWP_NOACTIVATE: u32 = 0x0010;
    const SWP_NOOWNERZORDER: u32 = 0x0200;
    // Window subclassing (Common Controls v6, system DLL).
    #[link(name = "comctl32")]
    unsafe extern "system" {
        fn SetWindowSubclass(
            hwnd: Hwnd,
            proc: unsafe extern "system" fn(Hwnd, u32, usize, isize, usize, usize) -> isize,
            id: usize,
            ref_data: usize,
        ) -> i32;
        fn DefSubclassProc(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> isize;
    }

    const WM_COPYDATA: u32 = 0x004A;
    const WM_DEVICECHANGE: u32 = 0x0219;
    const WM_POWERBROADCAST: u32 = 0x0218;
    /// The user grabbed the title bar or a resize border, and let go of it.
    /// Windows runs a modal loop in between, which is the only window in which
    /// the window can follow the pointer.
    const WM_ENTERSIZEMOVE: u32 = 0x0231;
    const WM_EXITSIZEMOVE: u32 = 0x0232;
    /// Sent before a non-client drag begins; `wparam` is the hit-test code, which
    /// says whether the user grabbed the caption (a MOVE) or a border (a RESIZE).
    const WM_NCLBUTTONDOWN: u32 = 0x00A1;
    const HTCAPTION: usize = 2;
    /// Hit codes seen at press time for the eight resize borders. A non-client
    /// press on one of these IS the start of an interactive resize — which is
    /// what has to be detected here, because `WM_ENTERSIZEMOVE` was measured NOT
    /// to arrive on this window (the log shows the size stream but never the
    /// matching enter/exit pair).
    const RESIZE_HITS: [usize; 8] = [10, 11, 12, 13, 14, 15, 16, 17];
    const WM_DPICHANGED: u32 = 0x02E0;
    const WM_SIZE: u32 = 0x0005;
    const SIZE_RESTORED: usize = 0;
    const SIZE_MAXIMIZED: usize = 2;
    /// Activation change: used only as a safety net. Losing/gaining activation
    /// cannot happen in the middle of a drag, so seeing it is proof that a
    /// `WM_EXITSIZEMOVE` was missed and the interaction state must be cleared.
    const WM_ACTIVATE: u32 = 0x0006;
    /// End of any left click, and the window taking keyboard focus. Both are
    /// gesture boundaries, and both are used only to ask for a full repaint.
    const WM_LBUTTONUP: u32 = 0x0202;
    const WM_SETFOCUS: u32 = 0x0007;
    /// Resume subtypes of `WM_POWERBROADCAST`. Both are broadcast to every
    /// top-level window with no prior registration. Two of them because they
    /// describe different ways back: `RESUMESUSPEND` follows a resume the user
    /// asked for, `RESUMEAUTOMATIC` one the machine decided on — and a laptop
    /// waking to its lock screen reports the second.
    const PBT_APMRESUMESUSPEND: usize = 0x0007;
    const PBT_APMRESUMEAUTOMATIC: usize = 0x0012;
    /// `DBT_DEVNODES_CHANGED`: a device was added to or removed from the
    /// machine. Windows broadcasts it to every top-level window WITHOUT any
    /// prior `RegisterDeviceNotification`, and it carries exactly the
    /// granularity needed here — "something was plugged or unplugged", not
    /// which device — which is the signal to re-scan rather than to poll.
    const DBT_DEVNODES_CHANGED: usize = 0x0007;
    const GA_ROOT: u32 = 2;
    const GW_OWNER: u32 = 4;
    /// Subclass identity, unique to this application among the subclasses set
    /// on the same window. The value is the ASCII of the application name,
    /// spelled out in the comment so that renaming the application finds it by
    /// searching for the old name: a value written only in hexadecimal is
    /// invisible to that search, and outlives the rename unnoticed.
    ///
    /// Four bytes, so the value also fits a 32-bit `usize`: the identity does
    /// not depend on the pointer width the application is built for.
    const SUBCLASS_ID: usize = 0x46_56_4E_52; // "FVNR", for FAVNYR
    /// Protocol tag/version (`dwData` field of the COPYDATASTRUCT): identifies
    /// a "Favnyr tab" payload — any other WM_COPYDATA (from another app) is
    /// ignored. Same ASCII spelling as the subclass id, plus the version.
    ///
    /// Changing this value makes instances built before the change and after
    /// it unable to hand tabs to each other: the tag no longer matches, so the
    /// message is ignored. Nothing crashes, and mismatched builds are not
    /// expected to run side by side.
    const PROTO_TAB_V1: usize = 0x46_56_4E_01; // "FVN", for FAVNYR, + version 1
    const SMTO_BLOCK: u32 = 0x0001;
    const SMTO_ABORTIFHUNG: u32 = 0x0002;
    const SEND_TIMEOUT_MS: u32 = 2000;

    /// Property name marking a Favnyr window (detectable by other instances).
    /// NUL-terminated (UTF-16).
    fn prop_name() -> Vec<u16> {
        "FavnyrTabTargetWindow\0".encode_utf16().collect()
    }

    // HWND of THIS instance's main window (0 = not initialized). Used to
    // exclude itself from detection and to identify itself as the sender.
    static SELF_HWND: AtomicIsize = AtomicIsize::new(0);
    // REGISTERED window message IDs (RegisterWindowMessageW → same values
    // across all Favnyr instances for a given string). Resolved at init.
    static WM_HOVER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    static WM_HOVER_END: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    /// Hit code of the last non-client press, so `WM_ENTERSIZEMOVE` can tell a
    /// caption drag (move) from a border drag (resize).
    static LAST_HIT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// Incoming message: from another Favnyr instance, or from the system.
    pub enum Incoming {
        /// Tab DROPPED (serialized payload) → insert it.
        Transfer(String),
        /// HOVER in progress at SCREEN point `(x, y)` (physical) → insertion preview.
        Hover(i32, i32),
        /// End of hover (cursor left / drag ended elsewhere) → clear the preview.
        HoverEnd,
        /// The set of devices attached to the machine changed → re-scan the
        /// portable devices. Sent by Windows, not by another instance.
        DevicesChanged,
        /// The machine came back from sleep or hibernation. Sent by Windows.
        /// Long-lived per-window registrations are re-asserted at this point:
        /// the drop target survives an ordinary session, but a resume is where
        /// it has been observed to stop being consulted.
        PowerResumed,
        /// The user started (`true`) or finished (`false`) dragging or resizing
        /// the window. Sent by Windows. `resizing` distinguishes a border drag
        /// from a caption drag.
        WindowInteraction { started: bool, resizing: bool },
        /// Repaint the whole window (restored from minimised, or rescaled).
        Repaint,
        /// Recompute the layout, then repaint: the window came back at a size
        /// its layout may no longer match.
        Relayout,
    }

    /// Handler for incoming messages (on the UI thread).
    type Handler = Box<dyn Fn(Incoming)>;

    thread_local! {
        /// Handler being called (UI thread). Captured once at init; never
        /// crosses a thread (thread_local of the UI thread).
        static HANDLER: RefCell<Option<Handler>> = const { RefCell::new(None) };
    }

    fn hover_msg_name() -> Vec<u16> {
        "FavnyrTabHover\0".encode_utf16().collect()
    }
    fn hover_end_msg_name() -> Vec<u16> {
        "FavnyrTabHoverEnd\0".encode_utf16().collect()
    }

    /// Posts (UI thread) an `Incoming` variant to the handler, deferred.
    fn dispatch(incoming: Incoming) {
        // We're already on the UI thread (WndProc), but we DEFER to avoid
        // re-entering Slint from the message loop (consistent with the
        // transfer, and safe for a synchronous WM_COPYDATA).
        // `invoke_from_event_loop` requires `Send` → `Incoming` only captures
        // `Send` types.
        let _ = slint::invoke_from_event_loop(move || {
            HANDLER.with(|h| {
                if let Some(f) = h.borrow().as_ref() {
                    f(incoming);
                }
            });
        });
    }

    unsafe extern "system" fn find_main_cb(hwnd: Hwnd, lparam: isize) -> i32 {
        unsafe {
            // The first VISIBLE TOP-LEVEL window (no owner) of the thread = the
            // main window. `lparam` points to a `Hwnd` to fill in.
            if IsWindowVisible(hwnd) != 0 && GetWindow(hwnd, GW_OWNER) == 0 {
                *(lparam as *mut Hwnd) = hwnd;
                return 0; // stop
            }
            1 // continue
        }
    }

    unsafe extern "system" fn subclass_proc(
        hwnd: Hwnd,
        msg: u32,
        wparam: usize,
        lparam: isize,
        _id: usize,
        _data: usize,
    ) -> isize {
        unsafe {
            // Hover (async PostMessage): SCREEN coords packed into wparam/lparam.
            let hover = WM_HOVER.load(Ordering::Relaxed);
            let hover_end = WM_HOVER_END.load(Ordering::Relaxed);
            if hover != 0 && msg == hover {
                dispatch(Incoming::Hover(wparam as i32, lparam as i32));
                return 0;
            }
            if hover_end != 0 && msg == hover_end {
                dispatch(Incoming::HoverEnd);
                return 0;
            }
            // Device arrival/removal. Observed, never consumed: this is a
            // broadcast, so the default procedure and any other subclass on
            // this window must still receive it.
            if msg == WM_DEVICECHANGE && wparam == DBT_DEVNODES_CHANGED {
                dispatch(Incoming::DevicesChanged);
                return DefSubclassProc(hwnd, msg, wparam, lparam);
            }
            // Resume from sleep. Observed, never consumed, for the same reason
            // as the device broadcast above.
            if msg == WM_POWERBROADCAST
                && (wparam == PBT_APMRESUMESUSPEND || wparam == PBT_APMRESUMEAUTOMATIC)
            {
                dispatch(Incoming::PowerResumed);
                return DefSubclassProc(hwnd, msg, wparam, lparam);
            }
            // Start/end of an interactive window move or resize. Observed,
            // never consumed: the modal loop and the backend both need these.
            // The hit code seen at press time is what separates the two: a drag
            // on the caption must not raise the resize scrim, only a drag on a
            // border should.
            if msg == WM_NCLBUTTONDOWN {
                LAST_HIT.store(wparam, Ordering::Relaxed);
                // This, not `WM_ENTERSIZEMOVE`, is where an interactive resize
                // begins in practice: freeze the content machinery for the
                // duration of the drag.
                if RESIZE_HITS.contains(&wparam) {
                    dispatch(Incoming::WindowInteraction {
                        started: true,
                        resizing: true,
                    });
                }
                return DefSubclassProc(hwnd, msg, wparam, lparam);
            }
            if msg == WM_ENTERSIZEMOVE || msg == WM_EXITSIZEMOVE {
                let entering = msg == WM_ENTERSIZEMOVE;
                let resizing = entering && LAST_HIT.load(Ordering::Relaxed) != HTCAPTION;
                dispatch(Incoming::WindowInteraction {
                    started: entering,
                    resizing,
                });
                // Every drag or resize ends here: one full repaint, so the
                // software rasteriser cannot leave a region showing the window's
                // initial black.
                if !entering {
                    dispatch(Incoming::Repaint);
                }
                return DefSubclassProc(hwnd, msg, wparam, lparam);
            }
            // Safety net for a lost WM_EXITSIZEMOVE: an activation change cannot
            // happen mid-drag, so it means the interaction state is stale and the
            // window must go back to normal (otherwise the panels stay hidden and
            // the list stops updating until the next drag ends).
            if msg == WM_ACTIVATE {
                dispatch(Incoming::WindowInteraction {
                    started: false,
                    resizing: false,
                });
                // Back from the background or from another app: repaint, for the
                // same reason as the other boundaries.
                dispatch(Incoming::Relayout);
                // Erase and repaint the whole client area NOW, rather than
                // trusting a coalesced redraw request.
                InvalidateRect(hwnd, core::ptr::null(), 1);
                UpdateWindow(hwnd);
                return DefSubclassProc(hwnd, msg, wparam, lparam);
            }
            // Gesture boundaries that are not a window drag: the end of any left
            // click, and the window taking focus. Both are cheap to observe and
            // each one already costs a repaint of whatever the click changed, so
            // asking for the whole window there closes the last gaps the
            // software rasteriser was leaving black.
            if msg == WM_LBUTTONUP || msg == WM_SETFOCUS {
                // Release ends the interactive resize started at the border
                // press above, and the repaint settles whatever it left behind.
                if msg == WM_LBUTTONUP {
                    dispatch(Incoming::WindowInteraction {
                        started: false,
                        resizing: false,
                    });
                }
                dispatch(Incoming::Repaint);
                return DefSubclassProc(hwnd, msg, wparam, lparam);
            }
            // The window became visible again, or changed size through the
            // shell rather than a drag: ask for a full repaint, since the
            // software rasteriser's damage tracking has been seen to leave
            // regions unpainted after either.
            if msg == WM_SIZE && (wparam == SIZE_RESTORED || wparam == SIZE_MAXIMIZED) {
                dispatch(Incoming::Relayout);
                return DefSubclassProc(hwnd, msg, wparam, lparam);
            }
            // A cross-monitor drag lands here: `wparam` carries the new DPI (low
            // word) and `lparam` the rectangle Windows suggests. The rect is
            // logged as given and not applied — the backend owns that.
            if msg == WM_DPICHANGED {
                let suggested = &*(lparam as *const LogRect);
                // alone was measured NOT to do it: after crossing from a 200%
                // monitor to a 150% one the window kept its 2039 px physical
                // width at the new DPI, i.e. grew by a third on screen, and only
                // came back to the suggested 1529 px when the drag ended — which
                // is exactly the "it resizes itself mid-drag, with an animation"
                // that was reported. Kept narrow (z-order and activation
                // untouched) so it cannot steal focus from the drag.
                if suggested.right > suggested.left && suggested.bottom > suggested.top {
                    let _applied = {
                        SetWindowPos(
                            hwnd,
                            0,
                            suggested.left,
                            suggested.top,
                            suggested.right - suggested.left,
                            suggested.bottom - suggested.top,
                            SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
                        )
                    };

                }
                return DefSubclassProc(hwnd, msg, wparam, lparam);
            }
            if msg == WM_COPYDATA {
                let cds = &*(lparam as *const CopyDataStruct);
                // Only payloads marked with OUR protocol are processed: a
                // WM_COPYDATA from a third-party application passes through to
                // DefSubclassProc.
                if cds.dw_data != PROTO_TAB_V1 {
                    return DefSubclassProc(hwnd, msg, wparam, lparam);
                }
                if !cds.lp_data.is_null() && cds.cb_data > 0 {
                    let bytes =
                        std::slice::from_raw_parts(cds.lp_data as *const u8, cds.cb_data as usize);
                    let s = String::from_utf8_lossy(bytes).into_owned();
                    // DEFERRED processing on the UI thread (immediate return → the
                    // source isn't left blocked in its synchronous SendMessage).
                    dispatch(Incoming::Transfer(s));
                }
                return 1; // TRUE = message handled
            }
            DefSubclassProc(hwnd, msg, wparam, lparam)
        }
    }

    /// Marks + subclasses this instance's window and registers the handler
    /// for incoming messages. Must be called on the UI thread, AFTER the
    /// window exists (HWND created). Idempotent; `false` = window not found
    /// yet → retry.
    pub fn init(handler: impl Fn(Incoming) + 'static) -> bool {
        if SELF_HWND.load(Ordering::SeqCst) != 0 {
            return true; // already initialized
        }
        unsafe {
            let mut hwnd: Hwnd = 0;
            EnumThreadWindows(
                GetCurrentThreadId(),
                find_main_cb,
                &mut hwnd as *mut Hwnd as isize,
            );
            if hwnd == 0 {
                return false; // window not (yet) visible → caller retries
            }
            if SetWindowSubclass(hwnd, subclass_proc, SUBCLASS_ID, 0) == 0 {
                return false; // subclassing refused → IPC inactive, retryable
            }
            SetPropW(hwnd, prop_name().as_ptr(), 1);
            WM_HOVER.store(
                RegisterWindowMessageW(hover_msg_name().as_ptr()),
                Ordering::SeqCst,
            );
            WM_HOVER_END.store(
                RegisterWindowMessageW(hover_end_msg_name().as_ptr()),
                Ordering::SeqCst,
            );
            SELF_HWND.store(hwnd, Ordering::SeqCst);
        }
        HANDLER.with(|h| *h.borrow_mut() = Some(Box::new(handler)));
        true
    }

    /// Signals a HOVER in progress at SCREEN point `(x, y)` (physical) to the
    /// target window (async, non-blocking). Coords packed into wparam/lparam.
    pub fn hover(target: isize, x: i32, y: i32) {
        let msg = WM_HOVER.load(Ordering::SeqCst);
        if msg != 0 {
            unsafe { PostMessageW(target, msg, x as isize as usize, y as isize) };
        }
    }

    /// Signals the END of a hover to the target window (clears its preview).
    pub fn hover_end(target: isize) {
        let msg = WM_HOVER_END.load(Ordering::SeqCst);
        if msg != 0 {
            unsafe { PostMessageW(target, msg, 0, 0) };
        }
    }

    /// Favnyr window OTHER than self under the SCREEN point `(x, y)`
    /// (physical). `None` if the point isn't over a third-party Favnyr window.
    pub fn target_at(x: i32, y: i32) -> Option<isize> {
        unsafe {
            let top = GetAncestor(WindowFromPoint(Point { x, y }), GA_ROOT);
            if top == 0 || top == SELF_HWND.load(Ordering::SeqCst) {
                return None;
            }
            if GetPropW(top, prop_name().as_ptr()) != 0 {
                Some(top)
            } else {
                None
            }
        }
    }

    /// Main HWND of this instance, available after [`init`]. Shared with the
    /// OLE file drop target to avoid a second, fragile per-thread window
    /// detection.
    pub fn self_hwnd() -> Option<isize> {
        let hwnd = SELF_HWND.load(Ordering::SeqCst);
        (hwnd != 0).then_some(hwnd)
    }

    /// Converts a point from the Favnyr window's physical CLIENT coordinate
    /// space to screen coordinates. Unlike `Window::position`, this excludes
    /// exactly the Win32 borders and title bar.
    pub fn client_to_screen(x: i32, y: i32) -> Option<(i32, i32)> {
        let hwnd = SELF_HWND.load(Ordering::SeqCst);
        if hwnd == 0 {
            return None;
        }
        let mut point = Point { x, y };
        (unsafe { ClientToScreen(hwnd, &mut point) } != 0).then_some((point.x, point.y))
    }

    /// Inverse conversion, used to transform OLE and IPC screen coordinates
    /// into CLIENT coordinates before the Slint hit-test.
    pub fn screen_to_client(x: i32, y: i32) -> Option<(i32, i32)> {
        let hwnd = SELF_HWND.load(Ordering::SeqCst);
        if hwnd == 0 {
            return None;
        }
        let mut point = Point { x, y };
        (unsafe { ScreenToClient(hwnd, &mut point) } != 0).then_some((point.x, point.y))
    }

    /// Activates the target window after an OLE drop. Without this, the first
    /// click on the drop menu would sometimes only activate the window,
    /// without triggering the chosen entry.
    pub fn focus_self() {
        let hwnd = SELF_HWND.load(Ordering::SeqCst);
        if hwnd != 0 {
            unsafe {
                let activated = SetForegroundWindow(hwnd) != 0;
                SetFocus(hwnd);
                tracing::debug!(
                    activated,
                    foreground = GetForegroundWindow() == hwnd,
                    "OLE drop window activation"
                );
            }
        }
    }

    /// Sends the payload (UTF-8) to the target window and brings it to the
    /// foreground. `true` ONLY if the target PROCESSED the message (LRESULT of
    /// our subclassed proc) — the caller only removes the source tab in that
    /// case (otherwise it would be lost). Bounded timeout: a frozen target
    /// doesn't block the source.
    pub fn send(target: isize, payload: &str) -> bool {
        let bytes = payload.as_bytes();
        let cds = CopyDataStruct {
            dw_data: PROTO_TAB_V1,
            cb_data: bytes.len() as u32,
            lp_data: bytes.as_ptr() as *const core::ffi::c_void,
        };
        unsafe {
            let self_hwnd = SELF_HWND.load(Ordering::SeqCst) as usize;
            let mut lresult: usize = 0;
            let rc = SendMessageTimeoutW(
                target,
                WM_COPYDATA,
                self_hwnd,
                &cds as *const CopyDataStruct as isize,
                SMTO_BLOCK | SMTO_ABORTIFHUNG,
                SEND_TIMEOUT_MS,
                &mut lresult,
            );
            if rc == 0 || lresult != 1 {
                return false; // target frozen / gone / message not handled
            }
            SetForegroundWindow(target);
        }
        true
    }
}

#[cfg(not(windows))]
mod imp {
    /// Incoming message (inert variant outside Windows). The variants are
    /// filtered (`match`) by the handler but never CONSTRUCTED until
    /// cross-instance IPC is implemented on Linux → `allow(dead_code)`.
    #[allow(dead_code)]
    pub enum Incoming {
        Transfer(String),
        Hover(i32, i32),
        HoverEnd,
        DevicesChanged,
        PowerResumed,
        WindowInteraction { started: bool, resizing: bool },
        /// Repaint the whole window (restored from minimised, or rescaled).
        Repaint,
        /// Recompute the layout, then repaint: the window came back at a size
        /// its layout may no longer match.
        Relayout,
    }
    /// No-op outside Windows (`true` = don't retry). Cross-instance IPC is
    /// Windows-only for now (Linux: window detection + IPC still to be defined).
    pub fn init(_handler: impl Fn(Incoming) + 'static) -> bool {
        true
    }
    pub fn target_at(_x: i32, _y: i32) -> Option<isize> {
        None
    }
    pub fn send(_target: isize, _payload: &str) -> bool {
        false
    }
    pub fn hover(_target: isize, _x: i32, _y: i32) {}
    pub fn hover_end(_target: isize) {}
    // `self_hwnd` / `client_to_screen` / `screen_to_client` / `focus_self`: no
    // stub outside Windows — these window helpers (HWND, coordinate
    // conversions, focus) are only called under `#[cfg(windows)]` (OLE file
    // drag, screen↔client conversions). On Linux, `Window::position()` serves
    // as the fallback directly in the bridge (`window_logical_to_screen`, etc.).
}

pub use imp::{Incoming, hover, hover_end, init, send, target_at};
#[cfg(windows)]
pub use imp::{client_to_screen, focus_self, screen_to_client, self_hwnd};
