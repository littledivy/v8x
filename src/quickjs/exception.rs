//! QuickJS-backed shims for the "exception" family:
//! Exception (Error constructors / CreateMessage), Message, StackFrame,
//! StackTrace, Promise, PromiseResolver, PromiseRejectMessage, TryCatch.
//!
//! Ported from reference/qjs_v8_compat/src/{exception,promise}.rs onto the
//! QuickJS-ng C-ABI shape used by the JSC backend (src/exception.rs).
//!
//! ## Refcount discipline
//! Every shim that RETURNS a new v8 handle routes the JSValue through
//! `intern` (for owned +1 values returned by QuickJS C functions) or
//! `intern_dup` (for borrowed values we must not consume). Any JSValue we
//! create and don't keep is `JS_FreeValue`d exactly once.
//!
//! ## Errors
//! QuickJS has no public `JS_NewError` symbol in our binding, so error
//! objects are constructed by `new globalThis.<Ctor>(message)` via
//! JS_GetGlobalObject + JS_GetPropertyStr + JS_CallConstructor.
//!
//! ## TryCatch / pending exception
//! QuickJS keeps exactly one pending exception per context, reachable via
//! JS_HasException / JS_GetException / JS_Throw. We model v8's stack-allocated
//! TryCatch as a small buffer holding [isolate_ptr, rethrow_flag]; HasCaught /
//! Exception peek the context's pending slot, ReThrow re-arms it, Reset/Destruct
//! drain it (QJS-DIVERGE: no multi-level pending slot, matching the JSC backend).

#![allow(non_snake_case, unused)]

use super::core::{
  ctx_of, current_ctx, current_iso, intern, intern_dup, iso_state, jsval_of,
};
use super::quickjs_sys::*;
use crate::promise::{PromiseRejectEvent, PromiseRejectMessage, PromiseState};
use crate::support::{MaybeBool, int};
use crate::{
  Context, Function, Location, Message, Promise, PromiseResolver, RealIsolate,
  StackFrame, StackTrace, String, Value,
};
use std::os::raw::c_char;
use std::ptr;

unsafe fn peek_pending(ctx: *mut JSContext) -> Option<JSValue> {
  if ctx.is_null() || JS_HasException(ctx) == 0 {
    return None;
  }

  let exc = JS_GetException(ctx);
  let dup = JS_DupValue(ctx, exc);
  JS_Throw(ctx, dup);
  Some(exc)
}

unsafe fn clear_pending(ctx: *mut JSContext) {
  if ctx.is_null() || JS_HasException(ctx) == 0 {
    return;
  }
  let exc = JS_GetException(ctx);
  JS_FreeValue(ctx, exc);
}

unsafe fn make_named_error(message: *const String, name: &str) -> JSValue {
  let ctx = current_ctx();
  if ctx.is_null() {
    return jsv_undefined();
  }
  let global = JS_GetGlobalObject(ctx);
  let cname = match std::ffi::CString::new(name) {
    Ok(c) => c,
    Err(_) => {
      JS_FreeValue(ctx, global);
      return jsv_undefined();
    }
  };
  let ctor = JS_GetPropertyStr(ctx, global, cname.as_ptr());
  JS_FreeValue(ctx, global);
  if ctor.tag == JS_TAG_EXCEPTION || JS_IsConstructor(ctx, ctor) == 0 {
    JS_FreeValue(ctx, ctor);
    return jsv_undefined();
  }

  let msg = JS_DupValue(ctx, jsval_of(message));
  let mut args = [msg];
  let err = JS_CallConstructor(ctx, ctor, 1, args.as_mut_ptr());
  JS_FreeValue(ctx, ctor);
  JS_FreeValue(ctx, msg);
  if err.tag == JS_TAG_EXCEPTION {
    clear_pending(ctx);
    return jsv_undefined();
  }
  err
}

unsafe fn read_num_prop(
  ctx: *mut JSContext,
  obj: JSValue,
  prop: &[u8],
  default: int,
) -> int {
  let v = JS_GetPropertyStr(ctx, obj, prop.as_ptr() as *const c_char);
  if v.tag == JS_TAG_EXCEPTION {
    clear_pending(ctx);
    return default;
  }
  if jsv_is_undefined(&v) || jsv_is_null(&v) {
    JS_FreeValue(ctx, v);
    return default;
  }
  let mut out: i32 = 0;
  let ok = JS_ToInt32(ctx, &mut out, v);
  JS_FreeValue(ctx, v);
  if ok < 0 {
    clear_pending(ctx);
    default
  } else {
    out
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Exception__Error(message: *const String) -> *const Value {
  intern::<Value>(unsafe { make_named_error(message, "Error") })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Exception__RangeError(
  message: *const String,
) -> *const Value {
  intern::<Value>(unsafe { make_named_error(message, "RangeError") })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Exception__ReferenceError(
  message: *const String,
) -> *const Value {
  intern::<Value>(unsafe { make_named_error(message, "ReferenceError") })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Exception__SyntaxError(
  message: *const String,
) -> *const Value {
  intern::<Value>(unsafe { make_named_error(message, "SyntaxError") })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Exception__TypeError(
  message: *const String,
) -> *const Value {
  intern::<Value>(unsafe { make_named_error(message, "TypeError") })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Exception__CreateMessage(
  isolate: *mut RealIsolate,
  exception: *const Value,
) -> *const Message {
  let _ = isolate;
  if exception.is_null() {
    return ptr::null();
  }
  intern_dup::<Message>(current_ctx(), jsval_of(exception))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__Get(this: *const Message) -> *const String {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  unsafe {
    let mut len: usize = 0;
    let cstr = JS_ToCStringLen(ctx, &mut len, jsval_of(this));
    if cstr.is_null() {
      clear_pending(ctx);
      return ptr::null();
    }
    let s = JS_NewStringLen(ctx, cstr, len);
    JS_FreeCString(ctx, cstr);
    if s.tag == JS_TAG_EXCEPTION {
      return ptr::null();
    }
    intern::<String>(s)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__GetScriptResourceName(
  this: *const Message,
) -> *const Value {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return intern::<Value>(jsv_undefined());
  }
  unsafe {
    let v = JS_GetPropertyStr(
      ctx,
      jsval_of(this),
      b"fileName\0".as_ptr() as *const c_char,
    );
    if v.tag == JS_TAG_EXCEPTION {
      clear_pending(ctx);
      return intern::<Value>(jsv_undefined());
    }
    if jsv_is_undefined(&v) || jsv_is_null(&v) {
      JS_FreeValue(ctx, v);
      return intern::<Value>(jsv_undefined());
    }
    intern::<Value>(v)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__GetLineNumber(
  this: *const Message,
  context: *const Context,
) -> int {
  let ctx = ctx_of(context);
  let ctx = if ctx.is_null() { current_ctx() } else { ctx };
  if ctx.is_null() || this.is_null() {
    return -1;
  }
  unsafe { read_num_prop(ctx, jsval_of(this), b"lineNumber\0", -1) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__GetStartColumn(this: *const Message) -> int {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return 0;
  }
  unsafe { read_num_prop(ctx, jsval_of(this), b"columnNumber\0", 0) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__GetStackTrace(
  this: *const Message,
) -> *const StackTrace {
  if this.is_null() {
    return ptr::null();
  }
  intern_dup::<StackTrace>(current_ctx(), jsval_of(this))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Location__GetLineNumber(this: *const Location) -> int {
  if this.is_null() {
    return 0;
  }
  unsafe { *(this as *const i32) as int }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Location__GetColumnNumber(this: *const Location) -> int {
  if this.is_null() {
    return 0;
  }
  unsafe { *((this as *const i32).add(1)) as int }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackTrace__CurrentStackTrace(
  isolate: *mut RealIsolate,
  frame_limit: int,
) -> *const StackTrace {
  let _ = (isolate, frame_limit);
  ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackTrace__GetFrameCount(
  this: *const StackTrace,
) -> int {
  let _ = this;
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackTrace__GetFrame(
  this: *const StackTrace,
  isolate: *mut RealIsolate,
  index: u32,
) -> *const StackFrame {
  let _ = (this, isolate, index);
  ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__GetLineNumber(
  this: *const StackFrame,
) -> int {
  let _ = this;
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__GetColumn(this: *const StackFrame) -> int {
  let _ = this;
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__GetScriptName(
  this: *const StackFrame,
) -> *const String {
  let _ = this;
  ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__IsEval(this: *const StackFrame) -> bool {
  let _ = this;
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__IsUserJavaScript(
  this: *const StackFrame,
) -> bool {
  let _ = this;
  true
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__State(this: *const Promise) -> PromiseState {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return PromiseState::Pending;
  }

  match unsafe { JS_PromiseState(ctx, jsval_of(this)) } {
    1 => PromiseState::Fulfilled,
    2 => PromiseState::Rejected,
    _ => PromiseState::Pending,
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__MarkAsHandled(this: *const Promise) {
  let _ = this;
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__Result(this: *const Promise) -> *const Value {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }

  let v = unsafe { JS_PromiseResult(ctx, jsval_of(this)) };
  intern::<Value>(v)
}

unsafe fn call_promise_method(
  promise: *const Promise,
  context: *const Context,
  method: &[u8],
  handlers: &[JSValue],
) -> *const Promise {
  let ctx = ctx_of(context);
  let ctx = if ctx.is_null() { current_ctx() } else { ctx };
  if ctx.is_null() || promise.is_null() {
    return ptr::null();
  }
  let pv = jsval_of(promise);
  let f = JS_GetPropertyStr(ctx, pv, method.as_ptr() as *const c_char);
  if f.tag == JS_TAG_EXCEPTION || JS_IsFunction(ctx, f) == 0 {
    JS_FreeValue(ctx, f);
    clear_pending(ctx);
    return ptr::null();
  }
  let mut args: Vec<JSValue> = handlers.to_vec();
  let ret = JS_Call(ctx, f, pv, args.len() as i32, args.as_mut_ptr());
  JS_FreeValue(ctx, f);
  if ret.tag == JS_TAG_EXCEPTION {
    clear_pending(ctx);
    return ptr::null();
  }
  intern::<Promise>(ret)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__Catch(
  this: *const Promise,
  context: *const Context,
  handler: *const Function,
) -> *const Promise {
  unsafe {
    call_promise_method(this, context, b"catch\0", &[jsval_of(handler)])
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__Then2(
  this: *const Promise,
  context: *const Context,
  on_fulfilled: *const Function,
  on_rejected: *const Function,
) -> *const Promise {
  unsafe {
    call_promise_method(
      this,
      context,
      b"then\0",
      &[jsval_of(on_fulfilled), jsval_of(on_rejected)],
    )
  }
}

const RESOLVE_PROP: &[u8] = b"__v8qjs_resolve\0";
const REJECT_PROP: &[u8] = b"__v8qjs_reject\0";

#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__Resolver__New(
  context: *const Context,
) -> *const PromiseResolver {
  let ctx = ctx_of(context);
  let ctx = if ctx.is_null() { current_ctx() } else { ctx };
  if ctx.is_null() {
    return ptr::null();
  }
  unsafe {
    let mut funcs: [JSValue; 2] = [jsv_undefined(), jsv_undefined()];
    let promise = JS_NewPromiseCapability(ctx, funcs.as_mut_ptr());
    if promise.tag == JS_TAG_EXCEPTION {
      clear_pending(ctx);
      return ptr::null();
    }

    JS_SetPropertyStr(
      ctx,
      promise,
      RESOLVE_PROP.as_ptr() as *const c_char,
      funcs[0],
    );
    JS_SetPropertyStr(
      ctx,
      promise,
      REJECT_PROP.as_ptr() as *const c_char,
      funcs[1],
    );

    intern::<PromiseResolver>(promise)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__Resolver__GetPromise(
  this: *const PromiseResolver,
) -> *const Promise {
  if this.is_null() {
    return ptr::null();
  }
  intern_dup::<Promise>(current_ctx(), jsval_of(this))
}

unsafe fn resolver_settle(
  this: *const PromiseResolver,
  context: *const Context,
  value: *const Value,
  fn_prop: &[u8],
) -> MaybeBool {
  let ctx = ctx_of(context);
  let ctx = if ctx.is_null() { current_ctx() } else { ctx };
  if ctx.is_null() || this.is_null() {
    return MaybeBool::Nothing;
  }
  let holder = jsval_of(this);
  let f = JS_GetPropertyStr(ctx, holder, fn_prop.as_ptr() as *const c_char);
  if f.tag == JS_TAG_EXCEPTION || JS_IsFunction(ctx, f) == 0 {
    JS_FreeValue(ctx, f);
    clear_pending(ctx);
    return MaybeBool::Nothing;
  }
  let mut args = [jsval_of(value)];
  let ret = JS_Call(ctx, f, jsv_undefined(), 1, args.as_mut_ptr());
  JS_FreeValue(ctx, f);
  let threw = ret.tag == JS_TAG_EXCEPTION;
  if threw {
    clear_pending(ctx);
  } else {
    JS_FreeValue(ctx, ret);
  }
  if threw {
    MaybeBool::JustFalse
  } else {
    MaybeBool::JustTrue
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__Resolver__Resolve(
  this: *const PromiseResolver,
  context: *const Context,
  value: *const Value,
) -> MaybeBool {
  unsafe { resolver_settle(this, context, value, RESOLVE_PROP) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__Resolver__Reject(
  this: *const PromiseResolver,
  context: *const Context,
  value: *const Value,
) -> MaybeBool {
  unsafe { resolver_settle(this, context, value, REJECT_PROP) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PromiseRejectMessage__GetPromise(
  this: *const PromiseRejectMessage,
) -> *const Promise {
  if this.is_null() {
    return ptr::null();
  }
  unsafe { *(this as *const usize) as *const Promise }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PromiseRejectMessage__GetValue(
  this: *const PromiseRejectMessage,
) -> *const Value {
  if this.is_null() {
    return ptr::null();
  }
  unsafe { *((this as *const usize).add(1)) as *const Value }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PromiseRejectMessage__GetEvent(
  this: *const PromiseRejectMessage,
) -> PromiseRejectEvent {
  if this.is_null() {
    return PromiseRejectEvent::PromiseRejectWithNoHandler;
  }
  unsafe {
    match *((this as *const usize).add(2)) {
      1 => PromiseRejectEvent::PromiseHandlerAddedAfterReject,
      2 => PromiseRejectEvent::PromiseRejectAfterResolved,
      3 => PromiseRejectEvent::PromiseResolveAfterResolved,
      _ => PromiseRejectEvent::PromiseRejectWithNoHandler,
    }
  }
}

#[allow(non_camel_case_types)]
type TryCatch = usize;

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__CONSTRUCT(
  buf: *mut usize,
  isolate: *mut RealIsolate,
) {
  unsafe {
    *buf.add(0) = isolate as usize;
    *buf.add(1) = 0;
    *buf.add(2) = 0;
    *buf.add(3) = 0;
    *buf.add(4) = 0;
    *buf.add(5) = 0;

    let ctx = if isolate.is_null() {
      current_ctx()
    } else {
      let st = iso_state(isolate);
      st.contexts.last().copied().unwrap_or(st.ctx)
    };
    clear_pending(ctx);
  }
}

unsafe fn tc_ctx(this: *const TryCatch) -> *mut JSContext {
  let isolate = *(this as *const usize).add(0) as *mut RealIsolate;
  if isolate.is_null() {
    return current_ctx();
  }
  let st = iso_state(isolate);
  st.contexts.last().copied().unwrap_or(st.ctx)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__DESTRUCT(this: *mut usize) {
  if this.is_null() {
    return;
  }
  unsafe {
    let rethrow = *this.add(1) != 0;

    if !rethrow {
      clear_pending(tc_ctx(this as *const TryCatch));
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__HasCaught(this: *const TryCatch) -> bool {
  if this.is_null() {
    return false;
  }
  unsafe {
    let ctx = tc_ctx(this);
    !ctx.is_null() && JS_HasException(ctx) != 0
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__HasTerminated(this: *const TryCatch) -> bool {
  let _ = this;
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__Exception(
  this: *const TryCatch,
) -> *const Value {
  if this.is_null() {
    return ptr::null();
  }
  unsafe {
    let ctx = tc_ctx(this);
    match peek_pending(ctx) {
      Some(exc) => intern::<Value>(exc),
      None => ptr::null(),
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__Reset(this: *mut TryCatch) {
  if this.is_null() {
    return;
  }
  unsafe {
    *(this as *mut usize).add(1) = 0;
    clear_pending(tc_ctx(this as *const TryCatch));
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__ReThrow(this: *mut TryCatch) -> *const Value {
  if this.is_null() {
    return ptr::null();
  }
  unsafe {
    *(this as *mut usize).add(1) = 1;
    let ctx = tc_ctx(this as *const TryCatch);
    match peek_pending(ctx) {
      Some(exc) => intern::<Value>(exc),
      None => ptr::null(),
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__Message(
  this: *const TryCatch,
) -> *const Message {
  if this.is_null() {
    return ptr::null();
  }

  unsafe {
    let ctx = tc_ctx(this);
    match peek_pending(ctx) {
      Some(exc) => intern::<Message>(exc),
      None => ptr::null(),
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__GetSourceLine(
  this: *const Message,
  context: *const Context,
) -> *const String {
  let _ = (this, context);
  ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__GetFunctionName(
  this: *const StackFrame,
) -> *const String {
  let _ = this;
  ptr::null()
}
