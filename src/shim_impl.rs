//! Hand-written, JSC-backed implementations of the v8 C-ABI surface.
//!
//! Symbols defined here are EXCLUDED from the auto-generated stubs in
//! `shims.rs` (see tools/gen_shims.sh). This file grows as the runtime path is
//! brought up on JavaScriptCore; everything not yet here is an `unimplemented!`
//! stub.

#![allow(non_snake_case)]

use crate::Platform;
use std::os::raw::{c_char, c_int};

// --- V8 global init / teardown: all inert (JSC initializes lazily) ---

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__InitializePlatform(_platform: *mut Platform) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__Initialize() {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__Dispose() -> bool {
    true
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__DisposePlatform() {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__SetFlagsFromString(_flags: *const u8, _length: usize) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__SetFlagsFromCommandLine(
    _argc: *mut c_int,
    _argv: *mut *mut c_char,
    _usage: *const c_char,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__GetVersion() -> *const c_char {
    c"12.0.0 (v82jsc/JavaScriptCore)".as_ptr()
}

// --- Platform: an inert heap object; JSC needs no task runner ---

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__NewDefaultPlatform(
    _thread_pool_size: c_int,
    _idle_task_support: bool,
) -> *mut Platform {
    Box::into_raw(Box::new(0u8)) as *mut Platform
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__NewUnprotectedDefaultPlatform(
    _thread_pool_size: c_int,
    _idle_task_support: bool,
) -> *mut Platform {
    Box::into_raw(Box::new(0u8)) as *mut Platform
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__NewSingleThreadedDefaultPlatform(
    _idle_task_support: bool,
) -> *mut Platform {
    Box::into_raw(Box::new(0u8)) as *mut Platform
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__NewCustomPlatform(
    _thread_pool_size: c_int,
    _idle_task_support: bool,
    _unprotected: bool,
    _context: *mut std::ffi::c_void,
) -> *mut Platform {
    Box::into_raw(Box::new(0u8)) as *mut Platform
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__PumpMessageLoop(
    _platform: *mut Platform,
    _isolate: *mut std::ffi::c_void,
    _wait_for_work: bool,
) -> bool {
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__NotifyIsolateShutdown(
    _platform: *mut Platform,
    _isolate: *mut std::ffi::c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__DELETE(this: *mut Platform) {
    if !this.is_null() {
        unsafe { drop(Box::from_raw(this as *mut u8)) };
    }
}
