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

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__GetVersion() -> *const c_char {
    c"12.0.0 (v82jsc/QuickJS-ng)".as_ptr()
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
