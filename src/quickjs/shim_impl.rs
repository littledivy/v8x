//! Inert V8 global-init / Platform shims for the QuickJS backend.
//!
//! QuickJS initializes lazily and has no libplatform/task-runner, so these are
//! all no-ops (mirroring the JSC backend's `shim_impl.rs`).
#![allow(non_snake_case)]

use crate::Platform;
use crate::support::{SharedPtrBase, UniquePtr, long};
use std::os::raw::{c_char, c_int};
use std::ptr;

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

/// `Deno.versions.v8` for this backend: the REAL quickjs-ng version (via
/// `JS_GetVersion`, e.g. "0.15.1") tagged with the engine, instead of a
/// fabricated V8 number. Built once and leaked so the `*const c_char` stays
/// valid for the process lifetime.
#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__GetVersion() -> *const c_char {
    use std::sync::OnceLock;
    static VERSION: OnceLock<std::ffi::CString> = OnceLock::new();
    VERSION
        .get_or_init(|| {
            let raw = unsafe { crate::quickjs::quickjs_sys::JS_GetVersion() };
            let ver = if raw.is_null() {
                "unknown".to_string()
            } else {
                unsafe { std::ffi::CStr::from_ptr(raw) }
                    .to_string_lossy()
                    .into_owned()
            };
            std::ffi::CString::new(format!("{ver} (quickjs-ng)")).unwrap()
        })
        .as_ptr()
}

/// V8 command-line flag parsing has no QuickJS analogue. QuickJS ignores v8
/// flags, so report every flag as consumed by collapsing `argv` to just the
/// binary name (index 0); deno then sees no "unrecognized" leftovers and does
/// not exit(1).
#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__SetFlagsFromCommandLine(
    argc: *mut c_int,
    _argv: *mut *mut c_char,
    _usage: *const c_char,
) {
    if !argc.is_null() {
        unsafe {
            if *argc > 1 {
                *argc = 1;
            }
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__SetFlagsFromString(_flags: *const u8, _length: usize) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__SetEntropySource(_callback: *const std::ffi::c_void) {}

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
pub extern "C" fn v8__Platform__NewSingleThreadedDefaultPlatform(
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
pub extern "C" fn v8__Platform__NewDefaultPlatform(
    _thread_pool_size: c_int,
    _idle_task_support: bool,
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

// ===================================================================
// std::shared_ptr<v8::Platform>
//
// `Platform` is owned by the Rust side (a boxed dummy). We back the shared_ptr
// with a tiny manually-refcounted box. Layout: SharedPtrBase<T> is `[usize; 2]`
// — slot 0 is the Platform pointer, slot 1 the refcount box pointer. Mirrors
// the JSC backend's shim_misc.
// ===================================================================

#[repr(C)]
struct PlatformSharedRepr {
    platform: *mut std::ffi::c_void,
    refcount: *mut usize,
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__Platform__CONVERT__std__unique_ptr(
    unique_ptr: UniquePtr<Platform>,
) -> SharedPtrBase<Platform> {
    let raw = unique_ptr.into_raw() as *mut std::ffi::c_void;
    let repr = if raw.is_null() {
        PlatformSharedRepr {
            platform: ptr::null_mut(),
            refcount: ptr::null_mut(),
        }
    } else {
        PlatformSharedRepr {
            platform: raw,
            refcount: Box::into_raw(Box::new(1usize)),
        }
    };
    unsafe { std::mem::transmute::<PlatformSharedRepr, SharedPtrBase<Platform>>(repr) }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__Platform__get(
    ptr: *const SharedPtrBase<Platform>,
) -> *mut Platform {
    if ptr.is_null() {
        return ptr::null_mut();
    }
    let repr = ptr as *const PlatformSharedRepr;
    unsafe { (*repr).platform as *mut Platform }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__Platform__COPY(
    ptr: *const SharedPtrBase<Platform>,
) -> SharedPtrBase<Platform> {
    if ptr.is_null() {
        return SharedPtrBase::default();
    }
    let repr = ptr as *const PlatformSharedRepr;
    let (platform, refcount) = unsafe { ((*repr).platform, (*repr).refcount) };
    if !refcount.is_null() {
        unsafe { *refcount += 1 };
    }
    let copy = PlatformSharedRepr { platform, refcount };
    unsafe { std::mem::transmute::<PlatformSharedRepr, SharedPtrBase<Platform>>(copy) }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__Platform__reset(ptr: *mut SharedPtrBase<Platform>) {
    if ptr.is_null() {
        return;
    }
    let repr = ptr as *mut PlatformSharedRepr;
    unsafe {
        let refcount = (*repr).refcount;
        if !refcount.is_null() {
            *refcount -= 1;
            if *refcount == 0 {
                drop(Box::from_raw(refcount));
                let p = (*repr).platform as *mut u8;
                if !p.is_null() {
                    drop(Box::from_raw(p));
                }
            }
        }
        (*repr).platform = ptr::null_mut();
        (*repr).refcount = ptr::null_mut();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__Platform__use_count(
    ptr: *const SharedPtrBase<Platform>,
) -> long {
    if ptr.is_null() {
        return 0;
    }
    let repr = ptr as *const PlatformSharedRepr;
    let refcount = unsafe { (*repr).refcount };
    if refcount.is_null() {
        0
    } else {
        unsafe { *refcount as long }
    }
}
