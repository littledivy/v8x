//! Process/platform lifecycle for the porffor backend.
//!
//! Almost none of this is engine-specific: rusty_v8 wants a `Platform` object
//! it can put in a `shared_ptr`, plus the `V8::Initialize*` pair, before it
//! will let you build an isolate. Porffor has no scheduler or task runner of
//! its own to plug in here — it drains its own microtask queue inside the
//! compiled program — so the platform is an inert token that exists to satisfy
//! the shape, and `PumpMessageLoop` reports "no work done".
#![allow(non_snake_case)]

use crate::Platform;
use crate::support::{SharedPtrBase, UniquePtr, long};
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

static CURRENT_PLATFORM: AtomicPtr<c_void> =
  AtomicPtr::new(ptr::null_mut());

struct PorfforPlatform {
  custom_context: *mut c_void,
}

fn new_platform(custom_context: *mut c_void) -> *mut Platform {
  Box::into_raw(Box::new(PorfforPlatform { custom_context })) as *mut Platform
}

unsafe fn drop_platform(platform: *mut Platform) {
  if platform.is_null() {
    return;
  }
  if CURRENT_PLATFORM.load(Ordering::SeqCst) == platform as *mut c_void {
    CURRENT_PLATFORM.store(ptr::null_mut(), Ordering::SeqCst);
  }
  drop(unsafe { Box::from_raw(platform as *mut PorfforPlatform) });
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__InitializePlatform(platform: *mut Platform) {
  CURRENT_PLATFORM.store(platform as *mut c_void, Ordering::SeqCst);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__Initialize() {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__Dispose() -> bool {
  true
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__DisposePlatform() {
  CURRENT_PLATFORM.store(ptr::null_mut(), Ordering::SeqCst);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__GetVersion() -> *const c_char {
  c"14.9.207.2-porffor".as_ptr()
}

/// Porffor has no runtime flags to set: everything is decided at compile time.
#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__SetFlagsFromString(
  _flags: *const u8,
  _length: usize,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__SetFlagsFromCommandLine(
  _argc: *mut c_int,
  _argv: *mut *mut c_char,
  _remove_flags: bool,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__SetEntropySource(_callback: *const c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__NewDefaultPlatform(
  _thread_pool_size: c_int,
  _idle_task_support: bool,
) -> *mut Platform {
  new_platform(ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__NewSingleThreadedDefaultPlatform(
  _idle_task_support: bool,
) -> *mut Platform {
  new_platform(ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__NewUnprotectedDefaultPlatform(
  _thread_pool_size: c_int,
  _idle_task_support: bool,
) -> *mut Platform {
  new_platform(ptr::null_mut())
}

/// Porffor runs its own microtask queue inside the compiled program, so there
/// is nothing for an embedder-driven pump to do.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__PumpMessageLoop(
  _platform: *mut Platform,
  _isolate: *mut c_void,
  _wait_for_work: bool,
) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__NotifyIsolateShutdown(
  _platform: *mut Platform,
  _isolate: *mut c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__DELETE(this: *mut Platform) {
  unsafe { drop_platform(this) };
}

// std::shared_ptr<Platform>, laid out the way the vendored binding expects.
#[repr(C)]
struct PlatformSharedRepr {
  platform: *mut c_void,
  refcount: *mut usize,
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__Platform__CONVERT__std__unique_ptr(
  unique_ptr: UniquePtr<Platform>,
) -> SharedPtrBase<Platform> {
  let raw = unique_ptr.into_raw() as *mut c_void;
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
  unsafe {
    std::mem::transmute::<PlatformSharedRepr, SharedPtrBase<Platform>>(repr)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__Platform__get(
  ptr: *const SharedPtrBase<Platform>,
) -> *mut Platform {
  if ptr.is_null() {
    return ptr::null_mut();
  }
  unsafe { (*(ptr as *const PlatformSharedRepr)).platform as *mut Platform }
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
  unsafe {
    std::mem::transmute::<PlatformSharedRepr, SharedPtrBase<Platform>>(copy)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__Platform__reset(
  ptr: *mut SharedPtrBase<Platform>,
) {
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
        drop_platform((*repr).platform as *mut Platform);
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
  let refcount = unsafe { (*(ptr as *const PlatformSharedRepr)).refcount };
  if refcount.is_null() {
    0
  } else {
    unsafe { *refcount as long }
  }
}
