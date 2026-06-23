//! Extra C-ABI symbols referenced by the full `deno` cli (beyond deno_core).
//! Kept in a standalone module so it never collides with the family shims.
#![allow(non_snake_case, unused)]

use crate::Name;
use std::os::raw::{c_char, c_int, c_void};

/// Symbols the vendored v8 source references but deno dead-strips (so they only
/// surface as undefined when linking a test binary). Inert.
#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__IsSandboxEnabled() -> bool {
    false
}
#[unsafe(no_mangle)]
pub extern "C" fn v8__DisallowJavascriptExecutionScope__CONSTRUCT(
    _buf: *mut c_void,
    _isolate: *mut c_void,
) {
}
#[unsafe(no_mangle)]
pub extern "C" fn v8__DisallowJavascriptExecutionScope__DESTRUCT(_this: *mut c_void) {}
#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__Allocator__NewRustAllocator(
    _handle: *mut c_void,
    _vtable: *const c_void,
) -> *mut c_void {
    std::ptr::null_mut()
}

/// ICU default locale lookup (crate's icu module). Return a fixed default.
#[unsafe(no_mangle)]
pub extern "C" fn icu_get_default_locale(output: *mut c_char, output_len: usize) -> usize {
    let loc = b"en-US";
    if output.is_null() || output_len == 0 {
        return loc.len();
    }
    let n = loc.len().min(output_len.saturating_sub(1));
    unsafe {
        std::ptr::copy_nonoverlapping(loc.as_ptr() as *const c_char, output, n);
        *output.add(n) = 0; // NUL-terminate
    }
    n
}

/// Near-heap-limit callbacks: JSC exposes no equivalent. Inert.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__AddNearHeapLimitCallback(
    _isolate: *mut c_void,
    _callback: *const c_void,
    _data: *mut c_void,
) {
}

/// Identity hash of a Name (string/symbol). Derive a stable non-zero hash from
/// the handle pointer (the handle is stable for the value's lifetime).
#[unsafe(no_mangle)]
pub extern "C" fn v8__Name__GetIdentityHash(this: *const Name) -> c_int {
    let h = (this as usize as u64).wrapping_mul(0x9E3779B97F4A7C15);
    ((h >> 33) as u32 as i32) | 1
}

/// Fatal-error handler registration: JSC has no global fatal hook. Inert.
#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__SetFatalErrorHandler(_that: *const c_void) {}
