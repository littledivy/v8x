//! Inert V8 global-init / Platform shims for the QuickJS backend.
//!
//! QuickJS initializes lazily and has no libplatform/task-runner, so these are
//! all no-ops (mirroring the JSC backend's `init.rs`).
#![allow(non_snake_case)]

use crate::Platform;
use crate::support::{SharedPtrBase, UniquePtr, long};
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

type RawEntropySource = unsafe extern "C" fn(*mut u8, usize) -> bool;

unsafe extern "C" {
  fn v8__Platform__CustomPlatform__BASE__PostTask(
    context: *mut c_void,
    isolate: *mut c_void,
    task: *mut c_void,
  );
  fn v8__Platform__CustomPlatform__BASE__DROP(context: *mut c_void);
}

struct QuickJsPlatform {
  custom_context: *mut c_void,
}

static ENTROPY_SOURCE: AtomicPtr<std::ffi::c_void> =
  AtomicPtr::new(ptr::null_mut());
static CURRENT_PLATFORM: AtomicPtr<std::ffi::c_void> =
  AtomicPtr::new(ptr::null_mut());

pub(crate) fn has_entropy_source() -> bool {
  !ENTROPY_SOURCE.load(Ordering::SeqCst).is_null()
}

pub(crate) fn fill_entropy(buf: &mut [u8]) -> bool {
  let callback = ENTROPY_SOURCE.load(Ordering::SeqCst);
  if callback.is_null() {
    return false;
  }
  let callback: RawEntropySource = unsafe { std::mem::transmute(callback) };
  unsafe { callback(buf.as_mut_ptr(), buf.len()) }
}

pub(crate) fn post_foreground_task() {
  let platform = CURRENT_PLATFORM.load(Ordering::SeqCst);
  if platform.is_null() {
    return;
  }
  let platform = platform as *mut QuickJsPlatform;
  let context = unsafe { (*platform).custom_context };
  if context.is_null() {
    return;
  }
  unsafe {
    v8__Platform__CustomPlatform__BASE__PostTask(
      context,
      super::core::current_iso() as *mut c_void,
      ptr::null_mut(),
    );
  }
}

fn new_platform(custom_context: *mut c_void) -> *mut Platform {
  Box::into_raw(Box::new(QuickJsPlatform { custom_context })) as *mut Platform
}

unsafe fn drop_platform(platform: *mut Platform) {
  if platform.is_null() {
    return;
  }
  if CURRENT_PLATFORM.load(Ordering::SeqCst) == platform as *mut c_void {
    CURRENT_PLATFORM.store(ptr::null_mut(), Ordering::SeqCst);
  }
  let platform = unsafe { Box::from_raw(platform as *mut QuickJsPlatform) };
  if !platform.custom_context.is_null() {
    unsafe {
      v8__Platform__CustomPlatform__BASE__DROP(platform.custom_context)
    };
  }
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
  // Report the V8 version string our vendored rusty_v8 surface was generated
  // against (`v8::VERSION_STRING`), so `V8::get_version()` round-trips exactly.
  // This is a compat shim emulating V8; downstream code (e.g. Deno) compares
  // against the V8 version, not QuickJS's.
  use std::sync::OnceLock;
  static VERSION: OnceLock<std::ffi::CString> = OnceLock::new();
  VERSION
    .get_or_init(|| std::ffi::CString::new(crate::VERSION_STRING).unwrap())
    .as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__SetFlagsFromCommandLine(
  argc: *mut c_int,
  argv: *mut *mut c_char,
  _usage: *const c_char,
) {
  if argc.is_null() || argv.is_null() {
    return;
  }

  unsafe {
    let len = *argc;
    if len <= 1 {
      return;
    }

    let mut write = 1;
    for read in 1..len {
      let arg = *argv.add(read as usize);
      if arg.is_null() {
        continue;
      }
      let arg = std::ffi::CStr::from_ptr(arg).to_string_lossy();
      if consume_v8_flag(&arg) {
        continue;
      }
      *argv.add(write as usize) = *argv.add(read as usize);
      write += 1;
    }
    *argc = write;
  }
}

fn consume_v8_flag(flag: &str) -> bool {
  let name = flag
    .trim_start_matches('-')
    .split_once('=')
    .map_or(flag.trim_start_matches('-'), |(name, _)| name)
    .replace('-', "_");
  match name.as_str() {
    "use_strict" => {
      crate::quickjs::core::FORCE_STRICT
        .store(true, std::sync::atomic::Ordering::Relaxed);
      true
    }
    "no_use_strict" => {
      crate::quickjs::core::FORCE_STRICT
        .store(false, std::sync::atomic::Ordering::Relaxed);
      true
    }
    // Recognized V8 flags that do not have a QuickJS observable equivalent.
    "help" | "log_colour" | "log_color" => true,
    _ => false,
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__SetFlagsFromString(flags: *const u8, length: usize) {
  // Most V8 flags are inert for the QuickJS backend, but `--use_strict` changes
  // observable JS semantics (it makes top-level scripts run in strict mode), so
  // honor it. V8 normalizes `-`/`_` in flag names and supports a `--no` prefix
  // to clear booleans, so accept both spellings.
  if flags.is_null() || length == 0 {
    return;
  }
  let bytes = unsafe { std::slice::from_raw_parts(flags, length) };
  let Ok(text) = std::str::from_utf8(bytes) else {
    return;
  };
  for tok in text.split_whitespace() {
    consume_v8_flag(tok);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__SetEntropySource(callback: *const std::ffi::c_void) {
  ENTROPY_SOURCE.store(callback.cast_mut(), Ordering::SeqCst);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__NewCustomPlatform(
  _thread_pool_size: c_int,
  _idle_task_support: bool,
  _unprotected: bool,
  context: *mut std::ffi::c_void,
) -> *mut Platform {
  new_platform(context)
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

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__NewDefaultPlatform(
  _thread_pool_size: c_int,
  _idle_task_support: bool,
) -> *mut Platform {
  new_platform(ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__PumpMessageLoop(
  _platform: *mut Platform,
  isolate: *mut std::ffi::c_void,
  _wait_for_work: bool,
) -> bool {
  let isolate = isolate as *mut crate::RealIsolate;
  if isolate.is_null() {
    return false;
  }
  let st = super::core::iso_state(isolate);
  if st.rt.is_null() {
    return false;
  }
  unsafe {
    let mut pctx = std::ptr::null_mut();
    super::quickjs_sys::JS_ExecutePendingJob(st.rt, &mut pctx) > 0
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__NotifyIsolateShutdown(
  _platform: *mut Platform,
  _isolate: *mut std::ffi::c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__DELETE(this: *mut Platform) {
  unsafe { drop_platform(this) };
}

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
  let repr = ptr as *const PlatformSharedRepr;
  let refcount = unsafe { (*repr).refcount };
  if refcount.is_null() {
    0
  } else {
    unsafe { *refcount as long }
  }
}
