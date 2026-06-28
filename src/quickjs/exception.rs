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
  if ctx.is_null() || !JS_HasException(ctx) {
    return None;
  }

  let exc = JS_GetException(ctx);
  let dup = JS_DupValue(ctx, exc);
  JS_Throw(ctx, dup);
  Some(exc)
}

unsafe fn clear_pending(ctx: *mut JSContext) {
  if ctx.is_null() || !JS_HasException(ctx) {
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

// QuickJS records the current call stack into a fresh Error's `.stack` (V8-ish
// `    at <fn> (<file>:<line>:<col>)` lines). We capture + parse that into our
// own frame structs and back v8's opaque StackTrace/StackFrame with them —
// deno reaches these frames only via the C-ABI accessors below.
struct QjsStackFrame {
  line: i32,
  col: i32,
  url: Option<std::string::String>,
  func: Option<std::string::String>,
  is_user: bool,
}

struct QjsStackTrace {
  frames: Vec<QjsStackFrame>,
}

thread_local! {
  static LAST_STACK: std::cell::Cell<*mut QjsStackTrace> =
    const { std::cell::Cell::new(ptr::null_mut()) };
}

unsafe fn current_backtrace_string(ctx: *mut JSContext) -> std::string::String {
  // `new Error()` captures the current backtrace; read its `.stack`.
  let global = unsafe { JS_GetGlobalObject(ctx) };
  let ctor = unsafe { JS_GetPropertyStr(ctx, global, c"Error".as_ptr()) };
  unsafe { JS_FreeValue(ctx, global) };
  if unsafe { JS_IsConstructor(ctx, ctor) } == 0 {
    unsafe { JS_FreeValue(ctx, ctor) };
    return std::string::String::new();
  }
  let err = unsafe { JS_CallConstructor(ctx, ctor, 0, ptr::null_mut()) };
  unsafe { JS_FreeValue(ctx, ctor) };
  if err.tag == JS_TAG_EXCEPTION {
    unsafe { clear_pending(ctx) };
    return std::string::String::new();
  }
  let stack = unsafe { JS_GetPropertyStr(ctx, err, c"stack".as_ptr()) };
  unsafe { JS_FreeValue(ctx, err) };
  let mut len: usize = 0;
  let cstr = unsafe { JS_ToCStringLen(ctx, &mut len, stack) };
  let out = if cstr.is_null() {
    std::string::String::new()
  } else {
    let bytes = unsafe { std::slice::from_raw_parts(cstr as *const u8, len) };
    let s = std::string::String::from_utf8_lossy(bytes).into_owned();
    unsafe { JS_FreeCString(ctx, cstr) };
    s
  };
  unsafe { JS_FreeValue(ctx, stack) };
  out
}

fn parse_qjs_backtrace(raw: &str) -> Vec<QjsStackFrame> {
  let mut frames = Vec::new();
  for line in raw.lines() {
    let mut ln = line.trim();
    if ln.is_empty() {
      continue;
    }
    if let Some(rest) = ln.strip_prefix("at ") {
      ln = rest;
    }
    // `<func> (<loc>)` or just `<loc>`.
    let (func, loc) = match (ln.find('('), ln.strip_suffix(')')) {
      (Some(p), Some(_)) => (ln[..p].trim(), ln[p + 1..ln.len() - 1].trim()),
      _ => {
        // `<func>@<loc>` (some builds) or bare `<loc>`.
        match ln.rfind('@') {
          Some(p) => (ln[..p].trim(), ln[p + 1..].trim()),
          None => ("", ln),
        }
      }
    };
    let (url, line_no, col_no) = parse_loc(loc);
    let has_url = !url.is_empty() && url != "<anonymous>";
    frames.push(QjsStackFrame {
      line: line_no,
      col: col_no,
      url: has_url.then(|| url.to_string()),
      func: (!func.is_empty()).then(|| func.to_string()),
      is_user: has_url,
    });
  }
  frames
}

/// Split `<file>:<line>:<col>` / `<file>:<line>` into (file, line, col),
/// tolerating colons inside the URL (`file://`, `https://host:port/`).
fn parse_loc(loc: &str) -> (&str, i32, i32) {
  let mut file = loc;
  let mut line = 0;
  let mut col = 0;
  if let Some((rest, last)) = loc.rsplit_once(':') {
    if last.bytes().all(|b| b.is_ascii_digit()) && !last.is_empty() {
      if let Some((rest2, mid)) = rest.rsplit_once(':') {
        if mid.bytes().all(|b| b.is_ascii_digit()) && !mid.is_empty() {
          file = rest2;
          line = mid.parse().unwrap_or(0);
          col = last.parse().unwrap_or(0);
          return (file, line, col);
        }
      }
      file = rest;
      line = last.parse().unwrap_or(0);
    }
  }
  (file, line, col)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackTrace__CurrentStackTrace(
  isolate: *mut RealIsolate,
  frame_limit: int,
) -> *const StackTrace {
  let _ = (isolate, frame_limit);
  LAST_STACK.with(|cell| {
    let prev = cell.replace(ptr::null_mut());
    if !prev.is_null() {
      unsafe { drop(Box::from_raw(prev)) };
    }
  });
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }
  let raw = unsafe { current_backtrace_string(ctx) };
  let frames = parse_qjs_backtrace(&raw);
  let boxed = Box::into_raw(Box::new(QjsStackTrace { frames }));
  LAST_STACK.with(|cell| cell.set(boxed));
  boxed as *const StackTrace
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackTrace__GetFrameCount(
  this: *const StackTrace,
) -> int {
  if this.is_null() {
    return 0;
  }
  unsafe { (*(this as *const QjsStackTrace)).frames.len() as int }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackTrace__GetFrame(
  this: *const StackTrace,
  isolate: *mut RealIsolate,
  index: u32,
) -> *const StackFrame {
  let _ = isolate;
  if this.is_null() {
    return ptr::null();
  }
  let st = unsafe { &*(this as *const QjsStackTrace) };
  match st.frames.get(index as usize) {
    Some(frame) => frame as *const QjsStackFrame as *const StackFrame,
    None => ptr::null(),
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__GetLineNumber(
  this: *const StackFrame,
) -> int {
  if this.is_null() {
    return 0;
  }
  unsafe { (*(this as *const QjsStackFrame)).line as int }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__GetColumn(this: *const StackFrame) -> int {
  if this.is_null() {
    return 0;
  }
  unsafe { (*(this as *const QjsStackFrame)).col as int }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__GetScriptName(
  this: *const StackFrame,
) -> *const String {
  if this.is_null() {
    return ptr::null();
  }
  let frame = unsafe { &*(this as *const QjsStackFrame) };
  let Some(url) = frame.url.as_deref() else {
    return ptr::null();
  };
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }
  let cstr = match std::ffi::CString::new(url) {
    Ok(c) => c,
    Err(_) => return ptr::null(),
  };
  let v = unsafe { JS_NewString(ctx, cstr.as_ptr()) };
  intern(v)
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
  if this.is_null() {
    return false;
  }
  unsafe { (*(this as *const QjsStackFrame)).is_user }
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
    !ctx.is_null() && JS_HasException(ctx)
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

// ---------------------------------------------------------------------------
// Link-stubs for v8 C-ABI symbols that `test_api.rs` references but this
// backend doesn't implement yet. Each returns a benign default
// (null / 0 / false / `Nothing`) so the target LINKS and the many tests that
// don't touch these paths run; tests that do exercise them fail gracefully
// without crashing. Promote individual stubs to real implementations over time.
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__Exception__CaptureStackTrace(
  _context: *const std::os::raw::c_void,
  _object: *const std::os::raw::c_void,
) -> crate::support::MaybeBool {
  crate::support::MaybeBool::Nothing
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Exception__GetStackTrace(
  _exception: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__ErrorLevel(
  _this: *const std::os::raw::c_void,
) -> crate::support::int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__GetEndColumn(
  _this: *const std::os::raw::c_void,
) -> crate::support::int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__GetEndPosition(
  _this: *const std::os::raw::c_void,
) -> crate::support::int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__GetStartPosition(
  _this: *const std::os::raw::c_void,
) -> crate::support::int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__GetWasmFunctionIndex(
  _this: *const std::os::raw::c_void,
) -> crate::support::int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__IsOpaque(
  _this: *const std::os::raw::c_void,
) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__IsSharedCrossOrigin(
  _this: *const std::os::raw::c_void,
) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__HasHandler(
  _this: *const std::os::raw::c_void,
) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__GetScriptId(
  _this: *const std::os::raw::c_void,
) -> crate::support::int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__GetScriptNameOrSourceURL(
  _this: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__GetScriptSource(
  _this: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__GetScriptSourceMappingURL(
  _this: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__IsConstructor(
  _this: *const std::os::raw::c_void,
) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__IsWasm(
  _this: *const std::os::raw::c_void,
) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackTrace__CurrentScriptNameOrSourceURL(
  _isolate: *mut std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__StackTrace(
  _this: *const std::os::raw::c_void,
  _context: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}
