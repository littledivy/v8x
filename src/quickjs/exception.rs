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
  if ctor.tag == JS_TAG_EXCEPTION || !JS_IsConstructor(ctx, ctor) {
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
  if !unsafe { JS_IsConstructor(ctx, ctor) } {
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

/// Return the `(line, col)` of the first `    at <…>:line:col` frame in a stack
/// string, or `(0, 0)` if none. Used by `Script::Compile` to recover a
/// SyntaxError's parse location for deno's `from_v8_message` fallback.
pub(crate) fn first_frame_line_col(stack: &str) -> (i32, i32) {
  for line in stack.lines() {
    let t = line.trim();
    let Some(rest) = t.strip_prefix("at ") else {
      continue;
    };
    // `<func> (<loc>)` or bare `<loc>`.
    let loc = match (rest.find('('), rest.strip_suffix(')')) {
      (Some(p), Some(_)) => &rest[p + 1..rest.len() - 1],
      _ => rest,
    };
    let (_, line_no, col_no) = parse_loc(loc.trim());
    if line_no > 0 {
      return (line_no, col_no);
    }
  }
  (0, 0)
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
  if f.tag == JS_TAG_EXCEPTION || !JS_IsFunction(ctx, f) {
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
  if f.tag == JS_TAG_EXCEPTION || !JS_IsFunction(ctx, f) {
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

// ---------------------------------------------------------------------------
// Error.prepareStackTrace shim — V8-accurate stack frames for deno_core.
//
// deno_core registers a native V8 PrepareStackTraceCallback
// (`v8__Isolate__SetPrepareStackTraceCallback`). On V8 that callback runs in
// place of `Error.prepareStackTrace`, reading structured CallSite frames and
// formatting them. QuickJS-ng has no equivalent native hook — it only consults
// `Error.prepareStackTrace`. The vendored fork exposes the full V8 CallSite API
// on its native CallSites, but with placeholder values (isToplevel /
// isConstructor always false, getColumnNumber = the construct CALL column,
// synthetic Promise/Error-construct native frames left in). Left as-is, deno's
// `Display for JsError` prints the raw multi-frame quickjs stack, e.g.
//   Error: fail
//       at construct ([native code])
//       at
//       at [native code]
//       at global code (a.js:1:25)
// where V8 prints just `Error: fail\n    at a.js:1:16`.
//
// We close the gap with our own `Error.prepareStackTrace`, installed ONLY on
// runtimes that called SetPrepareStackTraceCallback (deno_core — never the bare
// rusty_v8 cell, which keeps quickjs's native stack untouched). It reads the
// fork's CallSites and re-formats them V8-style: native/file-less frames are
// dropped, top-level frames print without a function name, and `new X()`
// construct columns are moved from the call `(` back to the `new` keyword
// (recovered from the source line registered at eval time).
// ---------------------------------------------------------------------------

use std::os::raw::c_int;

thread_local! {
  static PREPARE_STACK_ACTIVE: std::cell::Cell<bool> =
    const { std::cell::Cell::new(false) };

  // deno's native PrepareStackTraceCallback (registered via
  // v8__Isolate__SetPrepareStackTraceCallback). QuickJS has no engine hook for
  // it, so our JS `Error.prepareStackTrace` forwards the (corrected) frames into
  // this callback when it's set — deno's formatter then applies native source
  // maps (the `//# sourceMappingURL=` payloads surfaced by
  // UnboundModuleScript::GetSourceMappingURL) before producing the stack string.
  static PREPARE_STACK_TRACE_CB: std::cell::Cell<
    Option<crate::isolate::PrepareStackTraceCallback<'static>>,
  > = const { std::cell::Cell::new(None) };
}

pub(crate) fn is_prepare_stack_active() -> bool {
  PREPARE_STACK_ACTIVE.with(|c| c.get())
}

/// Store deno's native PrepareStackTraceCallback so our `Error.prepareStackTrace`
/// can forward into it (enabling source-map resolution). Also flips the active
/// flag so new contexts install the shim.
pub(crate) fn set_prepare_stack_trace_cb(
  cb: crate::isolate::PrepareStackTraceCallback<'static>,
) {
  PREPARE_STACK_TRACE_CB.with(|c| c.set(Some(cb)));
  activate_prepare_stack();
}

/// Record that this thread's runtime registered a PrepareStackTraceCallback, so
/// new contexts get our `Error.prepareStackTrace` from `install_default_globals`.
pub(crate) fn activate_prepare_stack() {
  PREPARE_STACK_ACTIVE.with(|c| c.set(true));
}

/// Install our `Error.prepareStackTrace` on `ctx`. No-op unless a
/// PrepareStackTraceCallback was registered for this isolate (see above).
pub(crate) fn install_prepare_stack_trace(ctx: *mut JSContext) {
  if ctx.is_null() || !is_prepare_stack_active() {
    return;
  }
  unsafe {
    let global = JS_GetGlobalObject(ctx);
    let error = JS_GetPropertyStr(ctx, global, c"Error".as_ptr());
    JS_FreeValue(ctx, global);
    if error.tag == JS_TAG_EXCEPTION || !JS_IsFunction(ctx, error) {
      JS_FreeValue(ctx, error);
      return;
    }
    let f = JS_NewCFunction(
      ctx,
      qjs_prepare_stack_trace,
      c"prepareStackTrace".as_ptr(),
      2,
    );
    JS_SetPropertyStr(ctx, error, c"prepareStackTrace".as_ptr(), f);
    JS_FreeValue(ctx, error);
  }
}

/// Coerce a JSValue to a Rust string, but only for actual strings — null /
/// undefined (e.g. a CallSite's absent file name) return `None`.
unsafe fn js_string_value(
  ctx: *mut JSContext,
  v: JSValue,
) -> Option<std::string::String> {
  if v.tag != JS_TAG_STRING && v.tag != JS_TAG_STRING_ROPE {
    return None;
  }
  let mut len: usize = 0;
  let s = unsafe { JS_ToCStringLen(ctx, &mut len, v) };
  if s.is_null() {
    unsafe { clear_pending(ctx) };
    return None;
  }
  let bytes = unsafe { std::slice::from_raw_parts(s as *const u8, len) };
  let out = std::string::String::from_utf8_lossy(bytes).into_owned();
  unsafe { JS_FreeCString(ctx, s) };
  Some(out)
}

unsafe fn read_str_prop(
  ctx: *mut JSContext,
  obj: JSValue,
  prop: &std::ffi::CStr,
) -> Option<std::string::String> {
  let v = unsafe { JS_GetPropertyStr(ctx, obj, prop.as_ptr()) };
  if v.tag == JS_TAG_EXCEPTION {
    unsafe { clear_pending(ctx) };
    return None;
  }
  let out = unsafe { js_string_value(ctx, v) };
  unsafe { JS_FreeValue(ctx, v) };
  out
}

/// Call a 0-arg method on a CallSite; returns `undefined` on any failure.
unsafe fn call_site_method(
  ctx: *mut JSContext,
  site: JSValue,
  name: &std::ffi::CStr,
) -> JSValue {
  let m = unsafe { JS_GetPropertyStr(ctx, site, name.as_ptr()) };
  if m.tag == JS_TAG_EXCEPTION || !unsafe { JS_IsFunction(ctx, m) } {
    unsafe { JS_FreeValue(ctx, m) };
    unsafe { clear_pending(ctx) };
    return jsv_undefined();
  }
  let ret = unsafe { JS_Call(ctx, m, site, 0, ptr::null_mut()) };
  unsafe { JS_FreeValue(ctx, m) };
  if ret.tag == JS_TAG_EXCEPTION {
    unsafe { clear_pending(ctx) };
    return jsv_undefined();
  }
  ret
}

unsafe fn call_site_str(
  ctx: *mut JSContext,
  site: JSValue,
  name: &std::ffi::CStr,
) -> Option<std::string::String> {
  let v = unsafe { call_site_method(ctx, site, name) };
  let out = unsafe { js_string_value(ctx, v) };
  unsafe { JS_FreeValue(ctx, v) };
  out
}

unsafe fn call_site_int(
  ctx: *mut JSContext,
  site: JSValue,
  name: &std::ffi::CStr,
) -> i32 {
  let v = unsafe { call_site_method(ctx, site, name) };
  let mut out: i32 = 0;
  let ok = unsafe { JS_ToInt32(ctx, &mut out, v) };
  unsafe { JS_FreeValue(ctx, v) };
  if ok < 0 {
    unsafe { clear_pending(ctx) };
    return 0;
  }
  out
}

fn append_location(
  out: &mut std::string::String,
  file: &str,
  line: i32,
  col: i32,
) {
  out.push_str(file);
  if line >= 1 {
    out.push(':');
    out.push_str(&line.to_string());
    if col >= 1 {
      out.push(':');
      out.push_str(&col.to_string());
    }
  }
}

/// V8 reports the `new` keyword as the column of a `new X()` construct frame;
/// quickjs reports a position inside the `new <member>(` span instead (the
/// whitespace after `new`, or the call's `(`, depending on the build). Given
/// the source `line` and quickjs's 1-based `col`, walk left across the
/// construct expression to the introducing `new` keyword and return its 1-based
/// column. If `col` isn't a `new <member>(` site, return it unchanged.
fn v8_new_expr_column(line: &str, col: i32) -> i32 {
  let chars: Vec<char> = line.chars().collect();
  let n = chars.len() as i32;
  if col < 1 || col > n {
    return col;
  }
  let is_member =
    |c: char| c.is_alphanumeric() || c == '_' || c == '$' || c == '.';
  let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
  let is_new_kw = |i: i32| {
    i >= 2
      && chars[i as usize] == 'w'
      && chars[(i - 1) as usize] == 'e'
      && chars[(i - 2) as usize] == 'n'
      && (i - 2 == 0 || !is_word(chars[(i - 3) as usize]))
  };
  let mut i = col - 1; // 0-based, at the reported column
  // Step over the construct's own opening paren if the column points at it
  // (`new X(` ← col on the `(`). Only this one paren is consumed — we must not
  // cross into an enclosing call, so a nested-argument frame like the `bar()`
  // in `new Foo(bar())` is left untouched.
  if chars[i as usize] == '(' {
    i -= 1;
  }
  loop {
    while i >= 0 && chars[i as usize].is_whitespace() {
      i -= 1;
    }
    if i < 0 {
      return col;
    }
    if is_new_kw(i) {
      return i - 2 + 1; // 1-based column of the `n`
    }
    // Skip one member-expression token (identifiers, `.`, `$`, `_`) and retry,
    // e.g. `new a.b.c(`. No progress → not a `new` site, leave `col` alone.
    let before = i;
    while i >= 0 && is_member(chars[i as usize]) {
      i -= 1;
    }
    if i == before {
      return col;
    }
  }
}

/// `Error.prepareStackTrace(error, callSites)` — see the module note above.
unsafe extern "C" fn qjs_prepare_stack_trace(
  ctx: *mut JSContext,
  _this: JSValue,
  argc: c_int,
  argv: *mut JSValue,
) -> JSValue {
  if argc < 2 || argv.is_null() || ctx.is_null() {
    return jsv_undefined();
  }
  let error = unsafe { *argv.add(0) };
  let sites = unsafe { *argv.add(1) };
  if std::env::var_os("QJS_DEBUG_PST").is_some() {
    unsafe {
      let mut l = 0usize;
      let s = JS_ToCStringLen(ctx, &mut l, error);
      if !s.is_null() {
        let b = std::slice::from_raw_parts(s as *const u8, l);
        eprintln!(
          "[QJS_PST] error={} rust_bt:\n{}",
          std::string::String::from_utf8_lossy(b),
          std::backtrace::Backtrace::force_capture()
        );
        JS_FreeCString(ctx, s);
      }
    }
  }

  // Header: `name: message` (V8's first stack line).
  let name = unsafe { read_str_prop(ctx, error, c"name") }
    .unwrap_or_else(|| "Error".to_string());
  let message =
    unsafe { read_str_prop(ctx, error, c"message") }.unwrap_or_default();
  let mut out = if message.is_empty() {
    name
  } else if name.is_empty() {
    message
  } else {
    format!("{name}: {message}")
  };

  let len = {
    let l = unsafe { JS_GetPropertyStr(ctx, sites, c"length".as_ptr()) };
    let mut n: i32 = 0;
    if unsafe { JS_ToInt32(ctx, &mut n, l) } < 0 {
      unsafe { clear_pending(ctx) };
    }
    unsafe { JS_FreeValue(ctx, l) };
    n.max(0)
  };

  // Corrected frames, collected for deno's native (source-mapping) formatter:
  // (file, line, column, function-name, is_top_level).
  let mut frames: Vec<(
    std::string::String,
    i32,
    i32,
    Option<std::string::String>,
    bool,
  )> = Vec::new();

  for i in 0..len as u32 {
    let site = unsafe { JS_GetPropertyUint32(ctx, sites, i) };
    if site.tag == JS_TAG_EXCEPTION {
      unsafe { clear_pending(ctx) };
      continue;
    }
    let file = unsafe { call_site_str(ctx, site, c"getFileName") };
    // Drop native / file-less frames — V8 elides quickjs's synthetic
    // Promise.reject / Error-construct frames.
    let Some(file) = file else {
      unsafe { JS_FreeValue(ctx, site) };
      continue;
    };
    // V8 reports an unnamed (eval'd) script as `<anonymous>`; quickjs uses our
    // `<eval>` sentinel. Normalise so deno's stack matches V8.
    let file = if file == "<eval>" {
      "<anonymous>".to_string()
    } else {
      file
    };
    let line = unsafe { call_site_int(ctx, site, c"getLineNumber") };
    let mut col = unsafe { call_site_int(ctx, site, c"getColumnNumber") };
    let func = unsafe { call_site_str(ctx, site, c"getFunctionName") };
    unsafe { JS_FreeValue(ctx, site) };

    // Recover V8's `new`-keyword column for `new X()` frames.
    if let Some(src) = super::core::script_source_line(&file, line) {
      col = v8_new_expr_column(&src, col);
    }

    // quickjs names top-level script frames `global code` / `<eval>`; V8
    // reports none, so deno prints them as `at file:line:col` (no wrapper).
    let is_top_level = matches!(
      func.as_deref(),
      None | Some("") | Some("global code") | Some("<eval>")
    );

    frames.push((file.clone(), line, col, func.clone(), is_top_level));

    out.push_str("\n    at ");
    if is_top_level {
      append_location(&mut out, &file, line, col);
    } else {
      out.push_str(func.as_deref().unwrap_or("<anonymous>"));
      out.push_str(" (");
      append_location(&mut out, &file, line, col);
      out.push(')');
    }
  }

  // Prefer deno's native formatter when it's registered: it applies source maps
  // (the `//# sourceMappingURL=` payloads) to each frame's file/line/column. We
  // hand it the SAME corrected frames computed above as synthetic CallSite
  // objects, so the column/function-name fixes survive and unmapped frames
  // (e.g. no source map) format identically to the string built here.
  if let Some(mapped) = unsafe { source_mapped_stack(ctx, error, &frames) } {
    return mapped;
  }

  let bytes = out.as_bytes();
  unsafe { JS_NewStringLen(ctx, bytes.as_ptr() as *const c_char, bytes.len()) }
}

/// JS factory that turns rows of `[file, line, col, funcOrNull, isTopLevel]`
/// into the V8-shaped CallSite objects deno's `from_callsite_object` consumes
/// (it calls each accessor and applies source maps to file/line/column).
const CALLSITE_FACTORY_SRC: &str = "(function(d){return d.map(function(f){\
  var file=f[0],line=f[1],col=f[2],top=f[4],fn=top?null:f[3];\
  return {\
    getFileName:function(){return file||undefined;},\
    getScriptNameOrSourceURL:function(){return file||undefined;},\
    getLineNumber:function(){return line||undefined;},\
    getColumnNumber:function(){return col||undefined;},\
    getFunctionName:function(){return fn;},\
    getMethodName:function(){return fn;},\
    getTypeName:function(){return null;},\
    getEvalOrigin:function(){return undefined;},\
    getThis:function(){return undefined;},\
    getFunction:function(){return undefined;},\
    isToplevel:function(){return top;},\
    isEval:function(){return false;},\
    isNative:function(){return false;},\
    isConstructor:function(){return false;},\
    isAsync:function(){return false;},\
    isPromiseAll:function(){return false;},\
    getPromiseIndex:function(){return null;},\
    toString:function(){return fn?(fn+' ('+file+':'+line+':'+col+')'):(file+':'+line+':'+col);}\
  };});})\0";

/// Build the synthetic CallSite array (one entry per corrected frame). Returns
/// an owned (+1) JSValue array, or `None` on failure.
unsafe fn build_callsites(
  ctx: *mut JSContext,
  frames: &[(
    std::string::String,
    i32,
    i32,
    Option<std::string::String>,
    bool,
  )],
) -> Option<JSValue> {
  unsafe {
    // Evaluate the factory function.
    let factory = JS_Eval(
      ctx,
      CALLSITE_FACTORY_SRC.as_ptr() as *const c_char,
      CALLSITE_FACTORY_SRC.len() - 1,
      c"<callsite-factory>".as_ptr(),
      JS_EVAL_TYPE_GLOBAL,
    );
    if factory.tag == JS_TAG_EXCEPTION || !JS_IsFunction(ctx, factory) {
      clear_pending(ctx);
      JS_FreeValue(ctx, factory);
      return None;
    }

    // Build the input rows array.
    let rows = JS_NewArray(ctx);
    if rows.tag == JS_TAG_EXCEPTION {
      clear_pending(ctx);
      JS_FreeValue(ctx, factory);
      return None;
    }
    for (i, (file, line, col, func, top)) in frames.iter().enumerate() {
      let row = JS_NewArray(ctx);
      let fv = JS_NewStringLen(ctx, file.as_ptr() as *const c_char, file.len());
      JS_SetPropertyUint32(ctx, row, 0, fv);
      JS_SetPropertyUint32(ctx, row, 1, JS_NewInt32(ctx, *line));
      JS_SetPropertyUint32(ctx, row, 2, JS_NewInt32(ctx, *col));
      let fnv = match func {
        Some(f) => JS_NewStringLen(ctx, f.as_ptr() as *const c_char, f.len()),
        None => jsv_null(),
      };
      JS_SetPropertyUint32(ctx, row, 3, fnv);
      JS_SetPropertyUint32(ctx, row, 4, JS_NewBool(ctx, *top as c_int));
      JS_SetPropertyUint32(ctx, rows, i as u32, row);
    }

    let mut args = [rows];
    let sites = JS_Call(ctx, factory, jsv_undefined(), 1, args.as_mut_ptr());
    JS_FreeValue(ctx, rows);
    JS_FreeValue(ctx, factory);
    if sites.tag == JS_TAG_EXCEPTION {
      clear_pending(ctx);
      return None;
    }
    Some(sites)
  }
}

/// Temporarily clear `globalThis.Error.prepareStackTrace` (returns the Error
/// object handle and the saved callback to restore afterwards). deno's native
/// callback re-dispatches to a user `Error.prepareStackTrace` if present — which
/// is *this* shim — so it must be cleared while we invoke the callback.
unsafe fn take_prepare_stack_trace(ctx: *mut JSContext) -> (JSValue, JSValue) {
  unsafe {
    let global = JS_GetGlobalObject(ctx);
    let error = JS_GetPropertyStr(ctx, global, c"Error".as_ptr());
    JS_FreeValue(ctx, global);
    if error.tag == JS_TAG_EXCEPTION || !JS_IsFunction(ctx, error) {
      clear_pending(ctx);
      JS_FreeValue(ctx, error);
      return (jsv_undefined(), jsv_undefined());
    }
    let saved = JS_GetPropertyStr(ctx, error, c"prepareStackTrace".as_ptr());
    JS_SetPropertyStr(
      ctx,
      error,
      c"prepareStackTrace".as_ptr(),
      jsv_undefined(),
    );
    (error, saved)
  }
}

/// Restore the previously-saved `Error.prepareStackTrace`.
unsafe fn restore_prepare_stack_trace(
  ctx: *mut JSContext,
  error: JSValue,
  saved: JSValue,
) {
  unsafe {
    if error.tag == JS_TAG_EXCEPTION || !JS_IsFunction(ctx, error) {
      JS_FreeValue(ctx, error);
      JS_FreeValue(ctx, saved);
      return;
    }
    // JS_SetPropertyStr consumes `saved`.
    JS_SetPropertyStr(ctx, error, c"prepareStackTrace".as_ptr(), saved);
    JS_FreeValue(ctx, error);
  }
}

/// Forward the corrected `frames` into deno's native PrepareStackTraceCallback,
/// returning the source-mapped, V8-formatted stack string. `None` when no
/// callback is registered (the bare rusty_v8 cell) or anything fails, so the
/// caller keeps its own string.
unsafe fn source_mapped_stack(
  ctx: *mut JSContext,
  error: JSValue,
  frames: &[(
    std::string::String,
    i32,
    i32,
    Option<std::string::String>,
    bool,
  )],
) -> Option<JSValue> {
  let cb = PREPARE_STACK_TRACE_CB.with(|c| c.get())?;
  if ctx.is_null() {
    return None;
  }
  // QuickJS builds `.stack` EAGERLY at throw time; V8 defers to the first
  // `.stack` read. An exception thrown while the embedder is still
  // initializing would reach its callback before any state pointer exists
  // (deno_core's prepare_stack_trace_callback does Rc::from_raw on embedder
  // slot 0 unconditionally). V8 can't call the callback in that window —
  // match it: decline until the embedder stores something in slot 0
  // (raw slot 1; slot 0 is the rusty_v8 annex).
  {
    let iso = super::core::current_iso();
    if iso.is_null()
      || super::core::iso_state(iso)
        .data_slots
        .get(1)
        .copied()
        .unwrap_or(std::ptr::null_mut())
        .is_null()
    {
      return None;
    }
  }
  let sites = unsafe { build_callsites(ctx, frames) }?;

  let (error_obj, saved) = unsafe { take_prepare_stack_trace(ctx) };

  let out = unsafe {
    let ctx_h = super::core::intern_ctx(ctx);
    let err_h = intern_dup::<Value>(ctx, error);
    let sites_h = intern::<crate::Array>(sites); // consumes `sites`
    match (
      crate::Local::from_raw(ctx_h),
      crate::Local::from_raw(err_h),
      crate::Local::from_raw(sites_h),
    ) {
      (Some(c_l), Some(e_l), Some(s_l)) => {
        let ret = cb(c_l, e_l, s_l);
        // PrepareStackTraceCallbackRet wraps a private `*const Value`.
        let v: *const Value = std::mem::transmute(ret);
        if v.is_null() || jsv_is_undefined(&jsval_of(v)) {
          // Null/undefined = the embedder declined (e.g. deno_core during
          // runtime init, before its state exists) — keep our own string.
          None
        } else {
          Some(JS_DupValue(ctx, jsval_of(v)))
        }
      }
      _ => None,
    }
  };

  unsafe { restore_prepare_stack_trace(ctx, error_obj, saved) };
  out
}
