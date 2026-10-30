//! Small Win32 helpers shared by every module that talks to the `*W` APIs.
//!
//! Only `std` is used here — `encode_wide` comes from
//! `std::os::windows::ffi::OsStrExt`, so this module needs no dependency on
//! the `windows` crate and stays usable from `favnyr-core` (which has none).
//!
//! Every helper returns a **NUL-terminated** buffer: the `*W` entry points take
//! a `PWSTR` they will keep reading until that terminator, and a buffer without
//! one is the classic source of stack-truncated or over-read strings.

#![cfg(windows)]

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

/// NUL-terminated UTF-16 for a borrowed `OsStr` (the lossless common case).
pub fn wide_os(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

/// NUL-terminated UTF-16 for a path.
pub fn wide_path(path: &Path) -> Vec<u16> {
    wide_os(path.as_os_str())
}

/// NUL-terminated UTF-16 for text that is already a Rust `str`.
///
/// `encode_wide` and `encode_utf16` produce the same units for a well-formed
/// `str`; routing through `OsStr` keeps one implementation for all three
/// overloads.
pub fn wide_text(text: &str) -> Vec<u16> {
    wide_os(OsStr::new(text))
}

// The handful of `kernel32` entry points that more than one module needs.
//
// They live here once so the signatures cannot drift apart between the device,
// drive and process modules — a hand-written `extern` block that disagrees
// with its twin is a silent ABI bug, not a compile error. Modules that need a
// function `use` it; each keeps its own extra declarations.
#[link(name = "kernel32")]
unsafe extern "system" {
    pub fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security_attributes: *mut core::ffi::c_void,
        creation_disposition: u32,
        flags: u32,
        template: isize,
    ) -> isize;
    pub fn DeviceIoControl(
        handle: isize,
        control_code: u32,
        in_buffer: *mut core::ffi::c_void,
        in_size: u32,
        out_buffer: *mut core::ffi::c_void,
        out_size: u32,
        bytes_returned: *mut u32,
        overlapped: *mut core::ffi::c_void,
    ) -> i32;
    pub fn CloseHandle(handle: isize) -> i32;
    pub fn GetLastError() -> u32;
}
