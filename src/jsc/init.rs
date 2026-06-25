//! Hand-written, JSC-backed implementations of the v8 C-ABI surface.
//!
//! Symbols defined here are EXCLUDED from the auto-generated stubs in
//! `shims.rs` (see tools/gen_shims.sh). This file grows as the runtime path is
//! brought up on JavaScriptCore; everything not yet here is an `unimplemented!`
//! stub.

#![allow(non_snake_case)]

use crate::Platform;
use std::os::raw::{c_char, c_int};

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__InitializePlatform(_platform: *mut Platform) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__Initialize() {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__Dispose() -> bool {
  true
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__DisposePlatform() {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__SetFlagsFromString(
  _flags: *const u8,
  _length: usize,
) {
}

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
pub extern "C" fn v8__V8__GetVersion() -> *const c_char {
  use std::sync::OnceLock;
  static VERSION: OnceLock<std::ffi::CString> = OnceLock::new();
  VERSION.get_or_init(jsc_version_string).as_ptr()
}

#[cfg(target_os = "macos")]
fn jsc_version_string() -> std::ffi::CString {
  use std::os::raw::c_void;
  type CFRef = *const c_void;
  #[allow(non_snake_case)]
  unsafe extern "C" {
    fn CFBundleGetBundleWithIdentifier(id: CFRef) -> CFRef;
    fn CFBundleGetValueForInfoDictionaryKey(b: CFRef, key: CFRef) -> CFRef;
    fn CFStringCreateWithCString(
      alloc: CFRef,
      s: *const c_char,
      enc: u32,
    ) -> CFRef;
    fn CFStringGetCString(
      s: CFRef,
      buf: *mut c_char,
      len: isize,
      enc: u32,
    ) -> bool;
    fn CFRelease(r: CFRef);
  }
  const UTF8: u32 = 0x0800_0100;
  let ver = unsafe {
    let id = CFStringCreateWithCString(
      std::ptr::null(),
      c"com.apple.JavaScriptCore".as_ptr(),
      UTF8,
    );
    let bundle = CFBundleGetBundleWithIdentifier(id);
    if !id.is_null() {
      CFRelease(id);
    }
    let mut out = String::new();
    if !bundle.is_null() {
      let key = CFStringCreateWithCString(
        std::ptr::null(),
        c"CFBundleVersion".as_ptr(),
        UTF8,
      );
      let val = CFBundleGetValueForInfoDictionaryKey(bundle, key);
      if !key.is_null() {
        CFRelease(key);
      }
      if !val.is_null() {
        let mut buf = [0i8; 128];
        if CFStringGetCString(val, buf.as_mut_ptr(), buf.len() as isize, UTF8) {
          out = std::ffi::CStr::from_ptr(buf.as_ptr())
            .to_string_lossy()
            .into_owned();
        }
      }
    }
    out
  };
  let label = if ver.is_empty() {
    "JavaScriptCore".to_string()
  } else {
    format!("{ver} (JavaScriptCore)")
  };
  std::ffi::CString::new(label).unwrap()
}

#[cfg(not(target_os = "macos"))]
fn jsc_version_string() -> std::ffi::CString {
  std::ffi::CString::new("JavaScriptCore").unwrap()
}

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
