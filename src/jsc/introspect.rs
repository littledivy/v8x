//! Declarations for the native introspection glue (`introspect.cpp`): Proxy
//! handler, synchronous Promise state/result, and Map/Set iterator preview — all
//! reading JSC engine-internal state that has no C-API equivalent.
//!
//! `introspect.cpp` needs JSC's *private* C++ headers (`APICast.h`, `JSCInlines.h`,
//! `ProxyObject.h`, `JSMapIterator.h`, …) which only the vendored WebKit build
//! ships, so it is compiled for `vendor_jsc` only. For `system_jsc` (the OS
//! framework, public headers only) we provide public-API fallbacks below: promise
//! detection still works (so `IsPromise` is correct), but the settled state/result,
//! the Proxy handler, and live iterator preview can't be read and degrade
//! gracefully (Pending / null) rather than failing to link.

use crate::jsc::jsc_sys::{JSContextRef, JSValueRef};
use std::os::raw::c_int;

#[cfg(feature = "vendor_jsc")]
unsafe extern "C" {
  /// Handler object of a Proxy, or null if `value` isn't a Proxy.
  pub(crate) fn v82jsc_proxy_handler(
    ctx: JSContextRef,
    value: JSValueRef,
  ) -> JSValueRef;

  /// Promise settled state (0 pending, 1 fulfilled, 2 rejected, matching
  /// `v8::Promise::PromiseState`), or -1 if not a Promise. Writes the
  /// result/reason into `*result_out` (null when pending / not a promise).
  pub(crate) fn v82jsc_promise_status(
    ctx: JSContextRef,
    value: JSValueRef,
    result_out: *mut JSValueRef,
  ) -> c_int;

  /// Remaining entries of a Map/Set iterator (without consuming the caller's
  /// iterator) as an array JSValueRef, or null if not a Map/Set iterator.
  /// `*is_key_value_out` is set true for Map `entries` iterators (the array is
  /// flattened key,value,key,value...).
  pub(crate) fn v82jsc_iterator_preview(
    ctx: JSContextRef,
    value: JSValueRef,
    is_key_value_out: *mut bool,
  ) -> JSValueRef;
}

// --- system_jsc: public-API fallbacks (no private headers available) ---

#[cfg(not(feature = "vendor_jsc"))]
pub(crate) use fallback::{
  v82jsc_iterator_preview, v82jsc_promise_status, v82jsc_proxy_handler,
};

#[cfg(not(feature = "vendor_jsc"))]
mod fallback {
  use super::{JSContextRef, JSValueRef, c_int};
  use crate::jsc::jsc_sys::*;
  use std::os::raw::c_char;
  use std::ptr;

  /// The Proxy handler isn't reachable through the public C API; deno only uses
  /// it for `console`/`Deno.inspect`, which falls back to formatting the proxy
  /// transparently when this is null.
  pub(crate) unsafe fn v82jsc_proxy_handler(
    _ctx: JSContextRef,
    _value: JSValueRef,
  ) -> JSValueRef {
    ptr::null()
  }

  /// Detect promise-ness via the public API (so `IsPromise` stays correct), then
  /// read the settled state/result from the `__v8jsc_state` / `__v8jsc_result`
  /// properties that `track_promise` attaches via `.then` to every deno-created
  /// promise — the same property-emulation the JSC backend used before the native
  /// (vendor-only) `introspect.cpp` path. Accurate for promises deno tracks (e.g.
  /// module-evaluation promises); untracked user promises read as Pending.
  pub(crate) unsafe fn v82jsc_promise_status(
    ctx: JSContextRef,
    value: JSValueRef,
    result_out: *mut JSValueRef,
  ) -> c_int {
    if !result_out.is_null() {
      unsafe { *result_out = ptr::null() };
    }
    if !unsafe { is_promise(ctx, value) } {
      return -1;
    }
    let obj = value as JSObjectRef;
    if !result_out.is_null() {
      let r = unsafe { read_prop(ctx, obj, b"__v8jsc_result\0") };
      if !r.is_null() && !unsafe { JSValueIsUndefined(ctx, r) } {
        unsafe { *result_out = r };
      }
    }
    let sv = unsafe { read_prop(ctx, obj, b"__v8jsc_state\0") };
    if sv.is_null() || unsafe { JSValueIsUndefined(ctx, sv) } {
      return 0;
    }
    let mut exc: JSValueRef = ptr::null();
    match unsafe { JSValueToNumber(ctx, sv, &mut exc) } as i32 {
      1 => 1,
      2 => 2,
      _ => 0,
    }
  }

  unsafe fn read_prop(
    ctx: JSContextRef,
    obj: JSObjectRef,
    name: &[u8],
  ) -> JSValueRef {
    let key =
      unsafe { JSStringCreateWithUTF8CString(name.as_ptr() as *const c_char) };
    let mut exc: JSValueRef = ptr::null();
    let v = unsafe { JSObjectGetProperty(ctx, obj, key, &mut exc) };
    unsafe { JSStringRelease(key) };
    v
  }

  /// Peeking a live Map/Set iterator without consuming it needs engine internals;
  /// returning null makes the caller fall through to the iterator-method path.
  pub(crate) unsafe fn v82jsc_iterator_preview(
    _ctx: JSContextRef,
    _value: JSValueRef,
    _is_key_value_out: *mut bool,
  ) -> JSValueRef {
    ptr::null()
  }

  unsafe fn is_promise(ctx: JSContextRef, value: JSValueRef) -> bool {
    // `Object.prototype.toString.call(p)` is "[object Promise]" for any promise
    // (Promise.prototype[Symbol.toStringTag] === "Promise"), cross-realm-safe.
    let src =
      b"(function(v){try{return Object.prototype.toString.call(v)===\"[object Promise]\";}catch(e){return false;}})\0";
    let mut exc: JSValueRef = ptr::null();
    let fs =
      unsafe { JSStringCreateWithUTF8CString(src.as_ptr() as *const c_char) };
    let fnv = unsafe {
      JSEvaluateScript(ctx, fs, ptr::null_mut(), ptr::null_mut(), 1, &mut exc)
    };
    unsafe { JSStringRelease(fs) };
    if !exc.is_null() {
      return false;
    }
    let fnobj = unsafe { JSValueToObject(ctx, fnv, &mut exc) };
    if fnobj.is_null() {
      return false;
    }
    let args = [value];
    let r = unsafe {
      JSObjectCallAsFunction(
        ctx,
        fnobj,
        ptr::null_mut(),
        1,
        args.as_ptr(),
        &mut exc,
      )
    };
    if !exc.is_null() {
      return false;
    }
    unsafe { JSValueToBoolean(ctx, r) }
  }
}
