#![allow(non_snake_case, unused)]

use crate::jsc::core::{
  clear_pending_exception, ctx_of, current_ctx, current_iso, intern,
  intern_ctx, iso_state, jsval, peek_pending_exception,
  record_pending_exception,
};
use crate::jsc::jsc_sys::*;
use crate::promise::{PromiseRejectEvent, PromiseRejectMessage, PromiseState};
use crate::support::{MaybeBool, int};
use crate::{
  Context, Function, Location, Message, Promise, PromiseResolver, RealIsolate,
  StackFrame, StackTrace, String, Value,
};
use std::os::raw::c_char;
use std::ptr;

#[allow(non_camel_case_types)]
type TryCatch = usize;

unsafe fn make_named_error(message: *const String, name: &str) -> *const Value {
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }

  let msg_val = jsval(message);
  let args = [msg_val];
  let mut exc: JSValueRef = ptr::null();
  let err = JSObjectMakeError(ctx, 1, args.as_ptr(), &mut exc);
  if err.is_null() {
    return ptr::null();
  }

  if name != "Error" {
    let cname = std::ffi::CString::new(name).unwrap();
    let key =
      JSStringCreateWithUTF8CString(b"name\0".as_ptr() as *const c_char);
    let name_str = JSStringCreateWithUTF8CString(cname.as_ptr());
    let name_val = JSValueMakeString(ctx, name_str);
    JSObjectSetProperty(ctx, err, key, name_val, 0, ptr::null_mut());
    JSStringRelease(key);
    JSStringRelease(name_str);
  }
  intern_ctx::<Value>(ctx, err as JSValueRef)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Exception__Error(message: *const String) -> *const Value {
  unsafe { make_named_error(message, "Error") }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Exception__RangeError(
  message: *const String,
) -> *const Value {
  unsafe { make_named_error(message, "RangeError") }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Exception__ReferenceError(
  message: *const String,
) -> *const Value {
  unsafe { make_named_error(message, "ReferenceError") }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Exception__SyntaxError(
  message: *const String,
) -> *const Value {
  unsafe { make_named_error(message, "SyntaxError") }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Exception__TypeError(
  message: *const String,
) -> *const Value {
  unsafe { make_named_error(message, "TypeError") }
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
  intern::<Message>(jsval(exception))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__Get(this: *const Message) -> *const String {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let s = JSValueToStringCopy(ctx, jsval(this), &mut exc);
    if s.is_null() {
      return ptr::null();
    }
    let v = JSValueMakeString(ctx, s);
    JSStringRelease(s);
    intern_ctx::<String>(ctx, v)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__GetScriptResourceName(
  this: *const Message,
) -> *const Value {
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }
  let v = unsafe { JSValueMakeUndefined(ctx) };
  intern_ctx::<Value>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__GetLineNumber(
  this: *const Message,
  context: *const Context,
) -> int {
  let ctx = ctx_of(context) as JSContextRef;
  if ctx.is_null() || this.is_null() {
    return -1;
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let obj = JSValueToObject(ctx, jsval(this), &mut exc);
    if obj.is_null() {
      return -1;
    }
    let key =
      JSStringCreateWithUTF8CString(b"line\0".as_ptr() as *const c_char);
    let v = JSObjectGetProperty(ctx, obj, key, &mut exc);
    JSStringRelease(key);
    if v.is_null() || JSValueIsUndefined(ctx, v) {
      return -1;
    }
    let n = JSValueToNumber(ctx, v, &mut exc);
    if n.is_nan() { -1 } else { n as int }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__GetStartColumn(this: *const Message) -> int {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return 0;
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let obj = JSValueToObject(ctx, jsval(this), &mut exc);
    if obj.is_null() {
      return 0;
    }
    let key =
      JSStringCreateWithUTF8CString(b"column\0".as_ptr() as *const c_char);
    let v = JSObjectGetProperty(ctx, obj, key, &mut exc);
    JSStringRelease(key);
    if v.is_null() || JSValueIsUndefined(ctx, v) {
      return 0;
    }
    let n = JSValueToNumber(ctx, v, &mut exc);
    if n.is_nan() { 0 } else { n as int }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__GetStackTrace(
  this: *const Message,
) -> *const StackTrace {
  if this.is_null() {
    return ptr::null();
  }
  intern::<StackTrace>(jsval(this))
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

// JSC has no public structured stack-trace API, but `JSContextCreateBacktrace`
// (a stable C SPI, present in both the vendored and the system framework)
// returns a formatted backtrace of the current JS stack. We capture + parse it
// into our own frame structs and back v8's opaque StackTrace/StackFrame with
// them. Each frame line is `#<idx> <funcName>() at <sourceURL>:<line>` — JSC
// emits line but not column, so columns report as 0 (refine via C++ glue if a
// consumer needs exact columns). deno only ever reaches these frames through
// the C-ABI accessors below, so a private backing layout is sound.
unsafe extern "C" {
  fn JSContextCreateBacktrace(
    ctx: JSContextRef,
    max_stack_size: u32,
  ) -> JSStringRef;
}

pub(crate) struct JscStackFrame {
  line: i32,
  col: i32,
  url: Option<std::string::String>,
  func: Option<std::string::String>,
  is_user: bool,
}

pub(crate) struct JscStackTrace {
  frames: Vec<JscStackFrame>,
}

thread_local! {
  // Holds the previous capture so it can be freed on the next one, bounding the
  // leak to a single live trace (deno consumes each trace synchronously before
  // capturing the next on this thread).
  static LAST_STACK: std::cell::Cell<*mut JscStackTrace> =
    const { std::cell::Cell::new(ptr::null_mut()) };
}

fn jsstring_to_rust(s: JSStringRef) -> std::string::String {
  if s.is_null() {
    return std::string::String::new();
  }
  unsafe {
    let max = JSStringGetMaximumUTF8CStringSize(s);
    let mut buf = vec![0u8; max];
    let n = JSStringGetUTF8CString(s, buf.as_mut_ptr() as *mut c_char, max);
    if n > 0 {
      buf.truncate(n - 1);
      std::string::String::from_utf8_lossy(&buf).into_owned()
    } else {
      std::string::String::new()
    }
  }
}

fn parse_backtrace(raw: &str) -> Vec<JscStackFrame> {
  let mut frames = Vec::new();
  for line in raw.lines() {
    // Strip the `#<idx> ` prefix.
    let rest = match line.split_once(' ') {
      Some((idx, rest)) if idx.starts_with('#') => rest,
      _ => line,
    };
    // Split `<funcName>() at <sourceURL>[:<line>]`.
    let (func, tail) = match rest.split_once("() at ") {
      Some((f, t)) => (f.trim(), t),
      None => ("", rest),
    };
    // The trailing `:<line>` (if present). sourceURLs contain colons
    // (`file://`, `https://host:port/`), so only treat a trailing all-digit
    // segment as the line number.
    let (url, line_no) = match tail.rsplit_once(':') {
      Some((u, n))
        if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) =>
      {
        (u, n.parse::<i32>().unwrap_or(0))
      }
      _ => (tail, 0),
    };
    let url = url.trim();
    let has_url = !url.is_empty();
    frames.push(JscStackFrame {
      line: line_no,
      col: 0,
      url: has_url.then(|| url.to_string()),
      func: (!func.is_empty()).then(|| func.to_string()),
      // A frame with a source URL is user/script JS; an empty URL is a native
      // (host) frame, which v8 reports as not user-JavaScript.
      is_user: has_url,
    });
  }
  frames
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackTrace__CurrentStackTrace(
  isolate: *mut RealIsolate,
  frame_limit: int,
) -> *const StackTrace {
  let _ = isolate;
  // Free the previous capture (pure-Rust drop; no protected JS values held).
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
  let limit = if frame_limit <= 0 {
    16
  } else {
    frame_limit as u32
  };
  let raw = unsafe { JSContextCreateBacktrace(ctx, limit) };
  if raw.is_null() {
    return ptr::null();
  }
  let s = jsstring_to_rust(raw);
  unsafe { JSStringRelease(raw) };
  let frames = parse_backtrace(&s);
  let boxed = Box::into_raw(Box::new(JscStackTrace { frames }));
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
  let st = unsafe { &*(this as *const JscStackTrace) };
  st.frames.len() as int
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
  let st = unsafe { &*(this as *const JscStackTrace) };
  match st.frames.get(index as usize) {
    Some(frame) => frame as *const JscStackFrame as *const StackFrame,
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
  unsafe { (*(this as *const JscStackFrame)).line as int }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__GetColumn(this: *const StackFrame) -> int {
  if this.is_null() {
    return 0;
  }
  unsafe { (*(this as *const JscStackFrame)).col as int }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__GetScriptName(
  this: *const StackFrame,
) -> *const String {
  if this.is_null() {
    return ptr::null();
  }
  let frame = unsafe { &*(this as *const JscStackFrame) };
  let Some(url) = frame.url.as_deref() else {
    return ptr::null();
  };
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }
  unsafe {
    let cstr = match std::ffi::CString::new(url) {
      Ok(c) => c,
      Err(_) => return ptr::null(),
    };
    let js = JSStringCreateWithUTF8CString(cstr.as_ptr());
    let v = JSValueMakeString(ctx, js);
    JSStringRelease(js);
    intern_ctx::<String>(ctx, v)
  }
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
  unsafe { (*(this as *const JscStackFrame)).is_user }
}

unsafe fn track_promise(ctx: JSContextRef, promise: JSObjectRef) {
  if ctx.is_null() || promise.is_null() {
    return;
  }

  let guard_key = JSStringCreateWithUTF8CString(
    b"__v8jsc_tracked\0".as_ptr() as *const c_char
  );
  let mut exc: JSValueRef = ptr::null();
  let guard = JSObjectGetProperty(ctx, promise, guard_key, &mut exc);
  let already = !guard.is_null() && JSValueToBoolean(ctx, guard);
  if already {
    JSStringRelease(guard_key);
    return;
  }
  JSObjectSetProperty(
    ctx,
    promise,
    guard_key,
    JSValueMakeBoolean(ctx, true),
    1 << 1,
    ptr::null_mut(),
  );
  JSStringRelease(guard_key);

  let src = b"(function(p){p.__v8jsc_state=0;try{p.then(function(v){p.__v8jsc_state=1;p.__v8jsc_result=v;},function(e){p.__v8jsc_state=2;p.__v8jsc_result=e;});}catch(_){}})\0";
  let js = JSStringCreateWithUTF8CString(src.as_ptr() as *const c_char);
  let f =
    JSEvaluateScript(ctx, js, ptr::null_mut(), ptr::null_mut(), 1, &mut exc);
  JSStringRelease(js);
  if f.is_null() {
    return;
  }
  let fobj = JSValueToObject(ctx, f, &mut exc);
  if fobj.is_null() {
    return;
  }
  let args = [promise as JSValueRef];
  JSObjectCallAsFunction(
    ctx,
    fobj,
    ptr::null_mut(),
    1,
    args.as_ptr(),
    &mut exc,
  );
}

pub(crate) fn track_promise_pub(ctx: JSContextRef, promise: JSObjectRef) {
  unsafe { track_promise(ctx, promise) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__State(this: *const Promise) -> PromiseState {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return PromiseState::Pending;
  }
  unsafe {
    let obj = jsval(this) as JSObjectRef;
    let key = JSStringCreateWithUTF8CString(
      b"__v8jsc_state\0".as_ptr() as *const c_char
    );
    let mut exc: JSValueRef = ptr::null();
    let v = JSObjectGetProperty(ctx, obj, key, &mut exc);
    JSStringRelease(key);
    if v.is_null() || JSValueIsUndefined(ctx, v) {
      return PromiseState::Pending;
    }
    match JSValueToNumber(ctx, v, &mut exc) as i32 {
      1 => PromiseState::Fulfilled,
      2 => PromiseState::Rejected,
      _ => PromiseState::Pending,
    }
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
  unsafe {
    let obj = jsval(this) as JSObjectRef;
    let key = JSStringCreateWithUTF8CString(
      b"__v8jsc_result\0".as_ptr() as *const c_char
    );
    let mut exc: JSValueRef = ptr::null();
    let v = JSObjectGetProperty(ctx, obj, key, &mut exc);
    JSStringRelease(key);
    let v = if v.is_null() {
      JSValueMakeUndefined(ctx)
    } else {
      v
    };
    intern_ctx::<Value>(ctx, v)
  }
}

unsafe fn call_promise_method(
  promise: *const Promise,
  context: *const Context,
  method: &[u8],
  handlers: &[JSValueRef],
) -> *const Promise {
  let ctx = ctx_of(context) as JSContextRef;
  if ctx.is_null() || promise.is_null() {
    return ptr::null();
  }
  let mut exc: JSValueRef = ptr::null();
  let obj = JSValueToObject(ctx, jsval(promise), &mut exc);
  if obj.is_null() {
    return ptr::null();
  }
  let key = JSStringCreateWithUTF8CString(method.as_ptr() as *const c_char);
  let f = JSObjectGetProperty(ctx, obj, key, &mut exc);
  JSStringRelease(key);
  if f.is_null() {
    return ptr::null();
  }
  let fobj = JSValueToObject(ctx, f, &mut exc);
  if fobj.is_null() {
    return ptr::null();
  }
  let ret = JSObjectCallAsFunction(
    ctx,
    fobj,
    obj,
    handlers.len(),
    handlers.as_ptr(),
    &mut exc,
  );
  if ret.is_null() {
    return ptr::null();
  }
  intern_ctx::<Promise>(ctx, ret)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__Catch(
  this: *const Promise,
  context: *const Context,
  handler: *const Function,
) -> *const Promise {
  unsafe { call_promise_method(this, context, b"catch\0", &[jsval(handler)]) }
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
      &[jsval(on_fulfilled), jsval(on_rejected)],
    )
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__Resolver__New(
  context: *const Context,
) -> *const PromiseResolver {
  let ctx = ctx_of(context) as JSContextRef;
  if ctx.is_null() {
    return ptr::null();
  }
  unsafe {
    let mut resolve: JSObjectRef = ptr::null_mut();
    let mut reject: JSObjectRef = ptr::null_mut();
    let mut exc: JSValueRef = ptr::null();
    let promise =
      JSObjectMakeDeferredPromise(ctx, &mut resolve, &mut reject, &mut exc);
    if promise.is_null() {
      return ptr::null();
    }

    let attrs = 1 << 1;
    let set = |obj: JSObjectRef, name: &[u8], val: JSValueRef| {
      let key = JSStringCreateWithUTF8CString(name.as_ptr() as *const c_char);
      JSObjectSetProperty(ctx, obj, key, val, attrs, ptr::null_mut());
      JSStringRelease(key);
    };

    set(promise, b"__v8jsc_resolve\0", resolve as JSValueRef);
    set(promise, b"__v8jsc_reject\0", reject as JSValueRef);
    track_promise(ctx, promise);
    intern_ctx::<PromiseResolver>(ctx, promise as JSValueRef)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__Resolver__GetPromise(
  this: *const PromiseResolver,
) -> *const Promise {
  if this.is_null() {
    return ptr::null();
  }
  intern::<Promise>(jsval(this))
}

unsafe fn resolver_settle(
  this: *const PromiseResolver,
  context: *const Context,
  value: *const Value,
  fn_prop: &[u8],
) -> MaybeBool {
  let ctx = ctx_of(context) as JSContextRef;
  if ctx.is_null() || this.is_null() {
    return MaybeBool::Nothing;
  }
  let obj = jsval(this) as JSObjectRef;
  let key = JSStringCreateWithUTF8CString(fn_prop.as_ptr() as *const c_char);
  let mut exc: JSValueRef = ptr::null();
  let f = JSObjectGetProperty(ctx, obj, key, &mut exc);
  JSStringRelease(key);
  if f.is_null() {
    return MaybeBool::Nothing;
  }
  let fobj = JSValueToObject(ctx, f, &mut exc);
  if fobj.is_null() {
    return MaybeBool::Nothing;
  }
  let args = [jsval(value)];
  JSObjectCallAsFunction(
    ctx,
    fobj,
    ptr::null_mut(),
    1,
    args.as_ptr(),
    &mut exc,
  );
  if !exc.is_null() {
    return MaybeBool::JustFalse;
  }
  MaybeBool::JustTrue
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__Resolver__Resolve(
  this: *const PromiseResolver,
  context: *const Context,
  value: *const Value,
) -> MaybeBool {
  unsafe { resolver_settle(this, context, value, b"__v8jsc_resolve\0") }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__Resolver__Reject(
  this: *const PromiseResolver,
  context: *const Context,
  value: *const Value,
) -> MaybeBool {
  unsafe { resolver_settle(this, context, value, b"__v8jsc_reject\0") }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PromiseRejectMessage__GetPromise(
  this: *const PromiseRejectMessage,
) -> *const Promise {
  if this.is_null() {
    return ptr::null();
  }
  unsafe {
    let slot0 = *(this as *const usize);
    slot0 as *const Promise
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PromiseRejectMessage__GetValue(
  this: *const PromiseRejectMessage,
) -> *const Value {
  if this.is_null() {
    return ptr::null();
  }
  unsafe {
    let slot1 = *((this as *const usize).add(1));
    slot1 as *const Value
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PromiseRejectMessage__GetEvent(
  this: *const PromiseRejectMessage,
) -> PromiseRejectEvent {
  if this.is_null() {
    return PromiseRejectEvent::PromiseRejectWithNoHandler;
  }

  unsafe {
    let slot2 = *((this as *const usize).add(2));
    match slot2 {
      1 => PromiseRejectEvent::PromiseHandlerAddedAfterReject,
      2 => PromiseRejectEvent::PromiseRejectAfterResolved,
      3 => PromiseRejectEvent::PromiseResolveAfterResolved,
      _ => PromiseRejectEvent::PromiseRejectWithNoHandler,
    }
  }
}

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
  }

  clear_pending_exception(isolate);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__DESTRUCT(this: *mut usize) {
  if this.is_null() {
    return;
  }
  unsafe {
    let isolate = *this.add(0) as *mut RealIsolate;
    let rethrow = *this.add(1) != 0;

    if !rethrow {
      clear_pending_exception(isolate);
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__HasCaught(this: *const TryCatch) -> bool {
  if this.is_null() {
    return false;
  }
  unsafe {
    let isolate = *(this as *const usize).add(0) as *mut RealIsolate;
    !peek_pending_exception(isolate).is_null()
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
    let isolate = *(this as *const usize).add(0) as *mut RealIsolate;
    let v = peek_pending_exception(isolate);
    if v.is_null() {
      return ptr::null();
    }

    intern::<Value>(v)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__Reset(this: *mut TryCatch) {
  if this.is_null() {
    return;
  }
  unsafe {
    let isolate = *(this as *const usize).add(0) as *mut RealIsolate;
    *(this as *mut usize).add(1) = 0;
    clear_pending_exception(isolate);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__ReThrow(this: *mut TryCatch) -> *const Value {
  if this.is_null() {
    return ptr::null();
  }
  unsafe {
    let isolate = *(this as *const usize).add(0) as *mut RealIsolate;

    *(this as *mut usize).add(1) = 1;
    let v = peek_pending_exception(isolate);
    v as *const Value
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__GetSourceLine(
  this: *const Message,
  context: *const Context,
) -> *const String {
  let ctx = ctx_of(context) as JSContextRef;
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let obj = JSValueToObject(ctx, jsval(this), &mut exc);
    if obj.is_null() {
      return ptr::null();
    }
    let key =
      JSStringCreateWithUTF8CString(b"sourceLine\0".as_ptr() as *const c_char);
    let v = JSObjectGetProperty(ctx, obj, key, &mut exc);
    JSStringRelease(key);
    if v.is_null() || !JSValueIsString(ctx, v) {
      return ptr::null();
    }
    intern_ctx::<String>(ctx, v)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__GetFunctionName(
  this: *const StackFrame,
) -> *const String {
  if this.is_null() {
    return ptr::null();
  }
  let frame = unsafe { &*(this as *const JscStackFrame) };
  let Some(func) = frame.func.as_deref() else {
    return ptr::null();
  };
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }
  unsafe {
    let cstr = match std::ffi::CString::new(func) {
      Ok(c) => c,
      Err(_) => return ptr::null(),
    };
    let js = JSStringCreateWithUTF8CString(cstr.as_ptr());
    let v = JSValueMakeString(ctx, js);
    JSStringRelease(js);
    intern_ctx::<String>(ctx, v)
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
    let isolate = *(this as *const usize).add(0) as *mut RealIsolate;
    let v = peek_pending_exception(isolate);
    if v.is_null() {
      return ptr::null();
    }
    intern::<Message>(v)
  }
}
