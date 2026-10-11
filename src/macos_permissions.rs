//! One-time macOS privacy prompts, fired when the service starts.
//!
//! The capture and emulation backends only *check* permissions silently, so
//! that re-enabling them (`mimouse cli enable-capture`) never pops a fresh
//! alert. This module is the single place that asks macOS to show the
//! prompts.
//!
//! On macOS 13+, the Accessibility grant transitively confers the
//! listen-only event-tap privilege that Input Monitoring gates and the
//! synthesize-event privilege that Post Event gates.

use std::ffi::{c_uchar, c_void};

// Apple declares `AXIsProcessTrusted` as returning `Boolean` (`unsigned char`),
// NOT C's `bool`. Keep these as `c_uchar` and normalize.
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> c_uchar;
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> c_uchar;
    // CFStringRef
    static kAXTrustedCheckOptionPrompt: *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFAllocatorDefault: *const c_void;
    static kCFTypeDictionaryKeyCallBacks: *const c_void;
    static kCFTypeDictionaryValueCallBacks: *const c_void;
    static kCFBooleanTrue: *const c_void;
    fn CFDictionaryCreate(
        allocator: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        num: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> *const c_void;
    fn CFRelease(cf: *const c_void);
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGRequestListenEventAccess() -> c_uchar;
    fn CGRequestPostEventAccess() -> c_uchar;
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: *const c_void,
        user_info: *const c_void,
    ) -> *const c_void;
}

/// Fire the system prompts for any missing permission. Call once per
/// process, before capture and emulation are created.
pub fn request_permissions() {
    if unsafe { AXIsProcessTrusted() } == 0 {
        // When Accessibility isn't granted yet, ONLY fire the Accessibility
        // prompt. Registering Input Monitoring or Post Event at this point has
        // been observed to surface a second Accessibility dialog. Once the
        // user grants Accessibility and restarts the service, the branch below
        // registers the other grants cleanly.
        log::info!("requesting Accessibility permission (restart mimouse after granting it)");
        unsafe {
            let key = kAXTrustedCheckOptionPrompt;
            let value = kCFBooleanTrue;
            let options = CFDictionaryCreate(
                kCFAllocatorDefault,
                &key as *const _,
                &value as *const _,
                1,
                kCFTypeDictionaryKeyCallBacks,
                kCFTypeDictionaryValueCallBacks,
            );
            AXIsProcessTrustedWithOptions(options);
            CFRelease(options);
        }
        return;
    }
    // Register Input Monitoring even if preflight succeeds, so the binary is
    // listed in System Settings under its own identity instead of inheriting
    // the grant from a parent process such as Terminal.
    unsafe {
        ensure_listed_in_input_monitoring();
        // Safe now that Accessibility is present: it won't surface the
        // generic Accessibility prompt.
        CGRequestPostEventAccess();
    }
}

/// `CGRequestListenEventAccess()` often silently no-ops (e.g. after
/// `tccutil reset ListenEvent`). Attempting a protected action, creating a
/// `CGEventTap`, reliably makes TCC list the binary under Input Monitoring.
/// A successfully created tap is released immediately.
unsafe fn ensure_listed_in_input_monitoring() {
    let req = CGRequestListenEventAccess();
    log::debug!("CGRequestListenEventAccess() = {req}");
    // kCGSessionEventTap (1), not kCGHIDEventTap (0): the HID tap also needs
    // Accessibility and would surface an extra Accessibility prompt.
    // kCGHeadInsertEventTap = 0, kCGEventTapOptionListenOnly = 1,
    // mask kCGEventKeyDown = 1 << 10.
    let cb = noop_tap_callback as *const c_void;
    let tap = CGEventTapCreate(1, 0, 1, 1 << 10, cb, std::ptr::null());
    log::debug!("CGEventTapCreate(kCGSessionEventTap) -> {tap:?}");
    if !tap.is_null() {
        CFRelease(tap);
    }
}

/// Never fires: the tap is never added to a run loop.
extern "C" fn noop_tap_callback(
    _proxy: *const c_void,
    _ty: u32,
    event: *const c_void,
    _refcon: *const c_void,
) -> *const c_void {
    event
}
