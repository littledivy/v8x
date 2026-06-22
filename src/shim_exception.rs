// Family: "exception" — Exception / Message / StackFrame / StackTrace /
// TryCatch / Promise / PromiseResolver / PromiseRejectMessage.
#![allow(non_snake_case, unused)]

use crate::jsc_sys::*;
use crate::promise::{PromiseRejectEvent, PromiseRejectMessage, PromiseState};
use crate::support::{int, MaybeBool};
use crate::{
    Context, Function, Location, Message, Promise, PromiseResolver, RealIsolate, StackFrame,
    StackTrace, String, Value,
};
use crate::shim_core::{
    ctx_of, current_ctx, current_iso, intern, intern_ctx, iso_state, jsval,
};
use std::os::raw::c_char;
use std::ptr;

// The raw TryCatch object is a small buffer (see scope/raw.rs). We back it with
// usize slots: [tag, has_caught, exception_ptr]. ABI is just a pointer.
#[allow(non_camel_case_types)]
type TryCatch = usize;

// Extra JSC C functions not declared in jsc_sys.rs.
unsafe extern "C" {
    fn JSObjectMakeError(
        ctx: JSContextRef,
        argument_count: usize,
        arguments: *const JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSObjectRef;
    fn JSObjectMakeDeferredPromise(
        ctx: JSContextRef,
        resolve: *mut JSObjectRef,
        reject: *mut JSObjectRef,
        exception: *mut JSValueRef,
    ) -> JSObjectRef;
    fn JSValueToObject(
        ctx: JSContextRef,
        value: JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSObjectRef;
    fn JSObjectCallAsFunction(
        ctx: JSContextRef,
        object: JSObjectRef,
        this_object: JSObjectRef,
        argument_count: usize,
        arguments: *const JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSValueRef;
    fn JSObjectGetProperty(
        ctx: JSContextRef,
        object: JSObjectRef,
        property_name: JSStringRef,
        exception: *mut JSValueRef,
    ) -> JSValueRef;
    fn JSValueIsObjectOfClass(
        ctx: JSContextRef,
        value: JSValueRef,
        js_class: JSClassRef,
    ) -> bool;
}

// ===================================================================
// Helpers
// ===================================================================

/// Build a JSC error object whose `name` matches the requested V8 error kind,
/// with the given message string value. Returns the new value handle.
unsafe fn make_named_error(message: *const String, name: &str) -> *const Value {
    let ctx = current_ctx();
    if ctx.is_null() {
        return ptr::null();
    }
    // message handle pointer IS a JSValueRef (a JS string value).
    let msg_val = jsval(message);
    let args = [msg_val];
    let mut exc: JSValueRef = ptr::null();
    let err = JSObjectMakeError(ctx, 1, args.as_ptr(), &mut exc);
    if err.is_null() {
        return ptr::null();
    }
    // Override the `name` property so RangeError/TypeError/etc. read correctly.
    if name != "Error" {
        let cname = std::ffi::CString::new(name).unwrap();
        let key = JSStringCreateWithUTF8CString(b"name\0".as_ptr() as *const c_char);
        let name_str = JSStringCreateWithUTF8CString(cname.as_ptr());
        let name_val = JSValueMakeString(ctx, name_str);
        // JSObjectSetProperty is not declared locally; reuse eval-free path via
        // a declared setter. Fall back: set via JSObjectSetProperty.
        JSObjectSetProperty(ctx, err, key, name_val, 0, ptr::null_mut());
        JSStringRelease(key);
        JSStringRelease(name_str);
    }
    intern_ctx::<Value>(ctx, err as JSValueRef)
}

unsafe extern "C" {
    fn JSObjectSetProperty(
        ctx: JSContextRef,
        object: JSObjectRef,
        property_name: JSStringRef,
        value: JSValueRef,
        attributes: u32,
        exception: *mut JSValueRef,
    );
}

// ===================================================================
// Exception
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Exception__Error(message: *const String) -> *const Value {
    unsafe { make_named_error(message, "Error") }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Exception__RangeError(message: *const String) -> *const Value {
    unsafe { make_named_error(message, "RangeError") }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Exception__ReferenceError(message: *const String) -> *const Value {
    unsafe { make_named_error(message, "ReferenceError") }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Exception__SyntaxError(message: *const String) -> *const Value {
    unsafe { make_named_error(message, "SyntaxError") }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Exception__TypeError(message: *const String) -> *const Value {
    unsafe { make_named_error(message, "TypeError") }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Exception__CreateMessage(
    isolate: *mut RealIsolate,
    exception: *const Value,
) -> *const Message {
    // Represent a v8::Message by the exception value itself; the Message
    // accessors below read properties off this object. Re-intern so the
    // Message handle is independently GC-rooted.
    let _ = isolate;
    if exception.is_null() {
        return ptr::null();
    }
    intern::<Message>(jsval(exception))
}

// ===================================================================
// Message — backed by the exception value (a JS Error object).
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__Get(this: *const Message) -> *const String {
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return ptr::null();
    }
    unsafe {
        // Convert the error value to a string (e.g. "TypeError: foo").
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
pub extern "C" fn v8__Message__GetScriptResourceName(this: *const Message) -> *const Value {
    // TODO(v82jsc): JSC errors do not always expose a resource name; return undefined.
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
    // Read the `line` property off the error object if present.
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
        let key = JSStringCreateWithUTF8CString(b"line\0".as_ptr() as *const c_char);
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
    // Read the `column` property off the error object if present.
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
        let key = JSStringCreateWithUTF8CString(b"column\0".as_ptr() as *const c_char);
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
pub extern "C" fn v8__Message__GetStackTrace(this: *const Message) -> *const StackTrace {
    // Represent the stack trace by the same error object; StackTrace accessors
    // below read the `stack` string. Re-intern so it is independently rooted.
    if this.is_null() {
        return ptr::null();
    }
    intern::<StackTrace>(jsval(this))
}

// ===================================================================
// Location — a flat [line, column] pair (repr [i32; 2]).
// ===================================================================

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

// ===================================================================
// StackTrace / StackFrame — JSC has no rich programmatic stack-frame API;
// expose inert-but-safe values so Deno degrades gracefully.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackTrace__CurrentStackTrace(
    isolate: *mut RealIsolate,
    frame_limit: int,
) -> *const StackTrace {
    // TODO(v82jsc): JSC C API has no current-stack-trace capture; return null.
    let _ = (isolate, frame_limit);
    ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackTrace__GetFrameCount(this: *const StackTrace) -> int {
    // TODO(v82jsc): no structured frames available.
    let _ = this;
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackTrace__GetFrame(
    this: *const StackTrace,
    isolate: *mut RealIsolate,
    index: u32,
) -> *const StackFrame {
    // TODO(v82jsc): no structured frames available.
    let _ = (this, isolate, index);
    ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__GetLineNumber(this: *const StackFrame) -> int {
    let _ = this;
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__GetColumn(this: *const StackFrame) -> int {
    let _ = this;
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__GetScriptName(this: *const StackFrame) -> *const String {
    // TODO(v82jsc): no structured frames available.
    let _ = this;
    ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__IsEval(this: *const StackFrame) -> bool {
    let _ = this;
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StackFrame__IsUserJavaScript(this: *const StackFrame) -> bool {
    let _ = this;
    true
}

// ===================================================================
// Promise
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__State(this: *const Promise) -> PromiseState {
    // JSC has no C API to query promise internal state; report Pending.
    // TODO(v82jsc): cannot inspect [[PromiseState]] via the JSC C API.
    let _ = this;
    PromiseState::Pending
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__MarkAsHandled(this: *const Promise) {
    // TODO(v82jsc): no [[PromiseIsHandled]] hook in JSC C API.
    let _ = this;
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__Result(this: *const Promise) -> *const Value {
    // TODO(v82jsc): cannot read [[PromiseResult]] via the JSC C API.
    let ctx = current_ctx();
    if ctx.is_null() {
        return ptr::null();
    }
    let v = unsafe { JSValueMakeUndefined(ctx) };
    intern_ctx::<Value>(ctx, v)
}

/// Call `promise.<method>(...handlers)` and return the resulting promise.
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

// ===================================================================
// PromiseResolver — backed by JSObjectMakeDeferredPromise. We store the
// {promise, resolve, reject} triple on the resolver object's own properties
// so GetPromise / Resolve / Reject can recover them.
// ===================================================================

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
        let promise = JSObjectMakeDeferredPromise(ctx, &mut resolve, &mut reject, &mut exc);
        if promise.is_null() {
            return ptr::null();
        }
        // Stash promise/resolve/reject on a holder object that serves as the
        // resolver handle.
        let attrs = 1 << 1 /* DontEnum */;
        let set = |obj: JSObjectRef, name: &[u8], val: JSValueRef| {
            let key = JSStringCreateWithUTF8CString(name.as_ptr() as *const c_char);
            JSObjectSetProperty(ctx, obj, key, val, attrs, ptr::null_mut());
            JSStringRelease(key);
        };
        // Use the promise object itself as the resolver handle and hang the
        // functions off it.
        set(promise, b"__v8jsc_resolve\0", resolve as JSValueRef);
        set(promise, b"__v8jsc_reject\0", reject as JSValueRef);
        intern_ctx::<PromiseResolver>(ctx, promise as JSValueRef)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__Resolver__GetPromise(
    this: *const PromiseResolver,
) -> *const Promise {
    // The resolver handle IS the promise object.
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
    JSObjectCallAsFunction(ctx, fobj, ptr::null_mut(), 1, args.as_ptr(), &mut exc);
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

// ===================================================================
// PromiseRejectMessage — repr is [usize; 3]; we treat field 0 as the promise
// JSValueRef and field 1 as the value JSValueRef (populated by whoever emits
// these). Inert-safe accessors otherwise.
// ===================================================================

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
    // Field 2 holds the event discriminant when populated.
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

// ===================================================================
// TryCatch — buffer is [MaybeUninit<usize>; 6].
// Layout we use: [0]=isolate ptr, [1]=caught flag (0/1), [2]=exception JSValueRef.
// JSC's C API surfaces exceptions via out-params on each call rather than a
// thread pending slot, so HasCaught/Exception are partly inert: they reflect
// whatever was recorded into the isolate's pending slot (data_slots is not used
// for this; we keep TryCatch self-contained and rely on ReThrow/Reset).
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__CONSTRUCT(buf: *mut usize, isolate: *mut RealIsolate) {
    unsafe {
        *buf.add(0) = isolate as usize;
        *buf.add(1) = 0; // has_caught = false
        *buf.add(2) = 0; // exception = null
        *buf.add(3) = 0;
        *buf.add(4) = 0;
        *buf.add(5) = 0;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__DESTRUCT(this: *mut usize) {
    // Nothing protected here; exception slot is a borrowed handle.
    let _ = this;
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__HasCaught(this: *const TryCatch) -> bool {
    if this.is_null() {
        return false;
    }
    unsafe { *((this as *const usize).add(1)) != 0 }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__HasTerminated(this: *const TryCatch) -> bool {
    // TODO(v82jsc): JSC has no execution-termination concept exposed here.
    let _ = this;
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__Exception(this: *const TryCatch) -> *const Value {
    if this.is_null() {
        return ptr::null();
    }
    unsafe {
        let v = *((this as *const usize).add(2));
        v as *const Value
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__Reset(this: *mut TryCatch) {
    if this.is_null() {
        return;
    }
    unsafe {
        let p = this as *mut usize;
        *p.add(1) = 0;
        *p.add(2) = 0;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__ReThrow(this: *mut TryCatch) -> *const Value {
    if this.is_null() {
        return ptr::null();
    }
    unsafe {
        // Returning the stored exception; the caller re-propagates it.
        let v = *((this as *const usize).add(2));
        v as *const Value
    }
}
