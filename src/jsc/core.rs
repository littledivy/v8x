//! Foundation for the JSC-backed C-ABI shims.
//!
//! This module owns the representation that backs `*mut RealIsolate` and the
//! handle-scope / context machinery. Every other `shim_*` module builds on the
//! helpers exported here:
//!
//! - `iso_state(p)` — `&mut IsoState` behind a `*mut RealIsolate`.
//! - `current_ctx()` — innermost entered `JSContextRef` (thread-local).
//! - `current_iso()` — current `*mut RealIsolate` (thread-local).
//! - `intern::<T>(jsval)` — protect `jsval` against the current context, record
//!   it in the current handle scope, return it as a `*const T`. The pointer of
//!   a `Local<T>` *is* the JSC `JSValueRef`.
//! - `jsval(p)` / `ctx_of(c)` — reinterpret a handle / context pointer.
#![allow(non_snake_case)]

use crate::jsc::jsc_sys::*;
use crate::{Context, Data, Object, Primitive, RealIsolate};
use std::cell::RefCell;
use std::os::raw::c_void;
use std::ptr;

pub(crate) struct IsoState {
  pub group: JSContextGroupRef,

  pub contexts: Vec<JSGlobalContextRef>,

  pub owned_contexts: Vec<JSGlobalContextRef>,

  pub handles: Vec<(JSContextRef, JSValueRef)>,

  pub data_slots: [*mut c_void; 4],

  pub pending_exception: Option<(JSContextRef, JSValueRef)>,

  pub import_meta_cb:
    Option<crate::isolate::HostInitializeImportMetaObjectCallback>,

  pub cpp_heap: *mut c_void,
}

thread_local! {
    static CURRENT_ISO: RefCell<*mut RealIsolate> = const { RefCell::new(ptr::null_mut()) };
    static CURRENT_CTX: RefCell<JSContextRef> = const { RefCell::new(ptr::null()) };

    static LAST_ISO: RefCell<*mut RealIsolate> = const { RefCell::new(ptr::null_mut()) };
}

#[inline(always)]
pub(crate) fn iso_state<'a>(p: *mut RealIsolate) -> &'a mut IsoState {
  unsafe { &mut *(p as *mut IsoState) }
}

#[inline(always)]
pub(crate) fn current_iso() -> *mut RealIsolate {
  let cur = CURRENT_ISO.with(|c| *c.borrow());
  if !cur.is_null() {
    return cur;
  }

  LAST_ISO.with(|c| *c.borrow())
}

#[inline(always)]
pub(crate) fn current_ctx() -> JSContextRef {
  CURRENT_CTX.with(|c| *c.borrow())
}

fn set_current(iso: *mut RealIsolate) {
  CURRENT_ISO.with(|c| *c.borrow_mut() = iso);
  if !iso.is_null() {
    LAST_ISO.with(|c| *c.borrow_mut() = iso);
  }
}

pub(crate) fn clear_last_iso(iso: *mut RealIsolate) {
  LAST_ISO.with(|c| {
    if *c.borrow() == iso {
      *c.borrow_mut() = ptr::null_mut();
    }
  });
}

pub(crate) fn restore_current(iso: *mut RealIsolate) {
  if iso.is_null() {
    return;
  }
  set_current(iso);
  refresh_current_ctx(iso_state(iso));
}

fn refresh_current_ctx(st: &IsoState) {
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  CURRENT_CTX.with(|c| *c.borrow_mut() = ctx);
}

#[inline(always)]
pub(crate) fn jsval<T>(p: *const T) -> JSValueRef {
  p as JSValueRef
}

#[inline(always)]
pub(crate) fn ctx_of(c: *const Context) -> JSGlobalContextRef {
  c as JSGlobalContextRef
}

#[inline]
fn fallback_ctx(iso: *mut RealIsolate) -> JSContextRef {
  if iso.is_null() {
    return ptr::null();
  }
  let st = iso_state(iso);
  st.contexts
    .last()
    .or_else(|| st.owned_contexts.last())
    .copied()
    .unwrap_or(ptr::null_mut()) as JSContextRef
}

#[inline]
pub(crate) fn is_non_value_handle(
  iso: *mut RealIsolate,
  v: JSValueRef,
) -> bool {
  if crate::jsc::function::is_template_ptr(v as *const c_void) {
    return true;
  }
  if iso.is_null() {
    return false;
  }
  let st = iso_state(iso);
  let p = v as JSGlobalContextRef;
  st.owned_contexts.contains(&p) || st.contexts.contains(&p)
}

#[inline]
pub(crate) fn intern_ctx<T>(ctx: JSContextRef, v: JSValueRef) -> *const T {
  if v.is_null() {
    return ptr::null();
  }
  let iso = current_iso();

  if is_non_value_handle(iso, v) {
    return v as *const T;
  }

  let mut ctx = if ctx.is_null() { current_ctx() } else { ctx };
  if ctx.is_null() {
    ctx = fallback_ctx(iso);
  }
  if !iso.is_null() && !ctx.is_null() {
    unsafe {
      JSValueProtect(ctx, v);
      iso_state(iso).handles.push((ctx, v));
    }
  }
  v as *const T
}

#[inline]
pub(crate) fn intern<T>(v: JSValueRef) -> *const T {
  intern_ctx(current_ctx(), v)
}

pub(crate) fn record_pending_exception(ctx: JSContextRef, exc: JSValueRef) {
  if exc.is_null() {
    return;
  }
  let iso = current_iso();
  if iso.is_null() {
    return;
  }
  let ctx = if ctx.is_null() { current_ctx() } else { ctx };
  let ctx = if ctx.is_null() {
    fallback_ctx(iso)
  } else {
    ctx
  };
  if ctx.is_null() {
    return;
  }
  let st = iso_state(iso);

  if let Some((octx, ov)) = st.pending_exception.take() {
    if !octx.is_null() && !ov.is_null() {
      unsafe { JSValueUnprotect(octx, ov) };
    }
  }
  unsafe { JSValueProtect(ctx, exc) };
  st.pending_exception = Some((ctx, exc));
}

pub(crate) fn clear_pending_exception(iso: *mut RealIsolate) {
  if iso.is_null() {
    return;
  }
  let st = iso_state(iso);
  if let Some((ctx, v)) = st.pending_exception.take() {
    if !ctx.is_null() && !v.is_null() {
      unsafe { JSValueUnprotect(ctx, v) };
    }
  }
}

pub(crate) fn peek_pending_exception(iso: *mut RealIsolate) -> JSValueRef {
  if iso.is_null() {
    return ptr::null();
  }
  let st = iso_state(iso);
  st.pending_exception.map(|(_, v)| v).unwrap_or(ptr::null())
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__New(_params: *const c_void) -> *mut RealIsolate {
  let group = unsafe { JSContextGroupCreate() };
  let state = Box::new(IsoState {
    group,
    contexts: Vec::new(),
    owned_contexts: Vec::new(),
    handles: Vec::new(),
    data_slots: [ptr::null_mut(); 4],
    pending_exception: None,
    import_meta_cb: None,
    cpp_heap: crate::jsc::misc::current_cpp_heap(),
  });
  Box::into_raw(state) as *mut RealIsolate
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetCppHeap(
  isolate: *mut RealIsolate,
) -> *mut c_void {
  if isolate.is_null() {
    return crate::jsc::misc::current_cpp_heap();
  }
  let st = iso_state(isolate);
  if st.cpp_heap.is_null() {
    st.cpp_heap = crate::jsc::misc::current_cpp_heap();
  }
  st.cpp_heap
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__Dispose(this: *mut RealIsolate) {
  if this.is_null() {
    return;
  }
  unsafe {
    let mut st = Box::from_raw(this as *mut IsoState);
    if let Some((ctx, v)) = st.pending_exception.take() {
      if !ctx.is_null() && !v.is_null() {
        JSValueUnprotect(ctx, v);
      }
    }
    while let Some((ctx, v)) = st.handles.pop() {
      if !ctx.is_null() && !v.is_null() {
        JSValueUnprotect(ctx, v);
      }
    }
    for ctx in st.owned_contexts.drain(..) {
      JSGlobalContextRelease(ctx);
    }
    JSContextGroupRelease(st.group);
  }
  set_current(ptr::null_mut());
  clear_last_iso(this);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__Enter(this: *mut RealIsolate) {
  set_current(this);
  refresh_current_ctx(iso_state(this));
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__Exit(_this: *mut RealIsolate) {
  set_current(ptr::null_mut());
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetCurrent() -> *mut RealIsolate {
  current_iso()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetNumberOfDataSlots(
  _this: *const RealIsolate,
) -> u32 {
  4
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetData(
  isolate: *const RealIsolate,
  slot: u32,
) -> *mut c_void {
  let st = iso_state(isolate as *mut RealIsolate);
  *st.data_slots.get(slot as usize).unwrap_or(&ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetData(
  isolate: *const RealIsolate,
  slot: u32,
  data: *mut c_void,
) {
  let st = iso_state(isolate as *mut RealIsolate);
  if let Some(s) = st.data_slots.get_mut(slot as usize) {
    *s = data;
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetCurrentContext(
  isolate: *mut RealIsolate,
) -> *const Context {
  let st = iso_state(isolate);
  st.contexts.last().copied().unwrap_or(ptr::null_mut()) as *const Context
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__HandleScope__CONSTRUCT(
  buf: *mut usize,
  isolate: *mut RealIsolate,
) {
  set_current(isolate);
  let st = iso_state(isolate);
  refresh_current_ctx(st);
  unsafe {
    *buf.offset(0) = isolate as usize;
    *buf.offset(1) = st.handles.len();
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__HandleScope__DESTRUCT(this: *mut usize) {
  unsafe {
    let isolate = *this.offset(0) as *mut RealIsolate;
    let saved_depth = *this.offset(1);
    if isolate.is_null() {
      return;
    }
    let st = iso_state(isolate);
    while st.handles.len() > saved_depth {
      let (ctx, v) = st.handles.pop().unwrap();
      if !ctx.is_null() && !v.is_null() {
        JSValueUnprotect(ctx, v);
      }
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Local__New(
  isolate: *mut RealIsolate,
  other: *const Data,
) -> *const Data {
  let _ = isolate;

  intern::<Data>(jsval(other))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__EscapeSlot__reserve(isolate: *mut RealIsolate) -> usize {
  if isolate.is_null() {
    return usize::MAX;
  }
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  if ctx.is_null() {
    return usize::MAX;
  }
  let v = unsafe { JSValueMakeUndefined(ctx) };
  unsafe {
    JSValueProtect(ctx, v);
    st.handles.push((ctx, v));
  }
  st.handles.len() - 1
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__EscapeSlot__escape(
  isolate: *mut RealIsolate,
  index: usize,
  value: *const Data,
) -> *const Data {
  if isolate.is_null() || index == usize::MAX || value.is_null() {
    return value;
  }
  let st = iso_state(isolate);
  let new_val = jsval(value);
  let Some(slot) = st.handles.get_mut(index) else {
    return value;
  };
  let (ctx, old) = *slot;
  if ctx.is_null() {
    return value;
  }
  unsafe {
    JSValueProtect(ctx, new_val);
    if !old.is_null() {
      JSValueUnprotect(ctx, old);
    }
  }
  *slot = (ctx, new_val);
  value
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Undefined(isolate: *mut RealIsolate) -> *const Primitive {
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  if ctx.is_null() {
    return ptr::null();
  }
  let v = unsafe { JSValueMakeUndefined(ctx) };
  intern_ctx::<Primitive>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__New(
  isolate: *mut RealIsolate,
  _templ: *const c_void,
  _global_object: *const c_void,
  _microtask_queue: *mut c_void,
) -> *const Context {
  let st = iso_state(isolate);
  let ctx = unsafe { JSGlobalContextCreateInGroup(st.group, ptr::null_mut()) };
  st.owned_contexts.push(ctx);
  unsafe { install_context_compat_shims(ctx) };
  unsafe { install_stacktrace_shim(ctx) };

  unsafe { crate::jsc::isolate::install_unhandled_rejection_bridge(ctx) };
  ctx as *const Context
}

/// V8-compatible structured stack traces. JSC's `Error.captureStackTrace` writes
/// `.stack` as a STRING, ignoring `Error.prepareStackTrace`; many npm packages
/// (express's `depd`, stack-trace, ...) do `Error.captureStackTrace(obj);
/// obj.stack` and expect `prepareStackTrace(obj, callSites)` to run, then call
/// `callSite.getFileName()` etc. Override `captureStackTrace` to install a lazy
/// `stack` accessor that, when `prepareStackTrace` is set, parses JSC's native
/// stack string into V8-shaped CallSite objects and passes them through.
unsafe fn install_stacktrace_shim(ctx: JSGlobalContextRef) {
  if ctx.is_null() {
    return;
  }
  const SRC: &[u8] = b"(function(){\
    'use strict';\
    var E = Error;\
    var nativeCapture = E.captureStackTrace;\
    function parseFrames(s){\
      if (typeof s !== 'string' || !s) return [];\
      var out = [];\
      var lines = s.split('\\n');\
      for (var i=0;i<lines.length;i++){\
        var ln = lines[i].trim();\
        if (!ln) continue;\
        if (ln.slice(0,3) === 'at ') ln = ln.slice(3);\
        var fn='', loc=ln;\
        var at = ln.lastIndexOf('@');\
        if (at >= 0){ fn = ln.slice(0,at); loc = ln.slice(at+1); }\
        var file=loc, line=0, col=0;\
        var m = /^(.*):(\\d+):(\\d+)$/.exec(loc);\
        if (m){ file=m[1]; line=+m[2]; col=+m[3]; }\
        out.push({fn:fn, file:file, line:line, col:col, raw:lines[i]});\
      }\
      return out;\
    }\
    function makeCallSite(f){\
      return {\
        getFileName:function(){ return f.file || undefined; },\
        getScriptNameOrSourceURL:function(){ return f.file || undefined; },\
        getLineNumber:function(){ return f.line || undefined; },\
        getColumnNumber:function(){ return f.col || undefined; },\
        getFunctionName:function(){ return f.fn || null; },\
        getMethodName:function(){ return f.fn || null; },\
        getTypeName:function(){ return null; },\
        getThis:function(){ return undefined; },\
        getFunction:function(){ return undefined; },\
        getEvalOrigin:function(){ return undefined; },\
        isToplevel:function(){ return true; },\
        isEval:function(){ return false; },\
        isNative:function(){ return f.file === '[native code]'; },\
        isConstructor:function(){ return false; },\
        isAsync:function(){ return false; },\
        isPromiseAll:function(){ return false; },\
        getPromiseIndex:function(){ return null; },\
        toString:function(){ return f.raw; }\
      };\
    }\
    E.captureStackTrace = function(target, ctorOpt){\
      var holder = {};\
      try { if (nativeCapture) nativeCapture.call(E, holder, ctorOpt); } catch(e){}\
      var raw = holder.stack;\
      if (typeof raw !== 'string') raw = '';\
      Object.defineProperty(target, 'stack', {\
        configurable: true,\
        get: function(){\
          var prep = E.prepareStackTrace;\
          if (typeof prep === 'function'){\
            try { return prep(target, parseFrames(raw).map(makeCallSite)); } catch(e){ return raw; }\
          }\
          return raw;\
        },\
        set: function(v){ Object.defineProperty(target, 'stack', {value:v, writable:true, configurable:true}); }\
      });\
    };\
  })()\0";
  unsafe {
    let js = JSStringCreateWithUTF8CString(
      SRC.as_ptr() as *const std::os::raw::c_char
    );
    let mut exc: JSValueRef = ptr::null();
    JSEvaluateScript(ctx, js, ptr::null_mut(), ptr::null_mut(), 1, &mut exc);
    JSStringRelease(js);
  }
}

unsafe fn install_context_compat_shims(ctx: JSGlobalContextRef) {
  if ctx.is_null() {
    return;
  }

  const SRC: &[u8] = b"(function(){\
        'use strict';\
        var realO = Object.setPrototypeOf;\
        var realR = Reflect.setPrototypeOf;\
        var g = globalThis;\
        var gdop = Object.getOwnPropertyDescriptor;\
        var gopn = Object.getOwnPropertyNames;\
        var gops = Object.getOwnPropertySymbols;\
        var dp = Object.defineProperty;\
        var getProto = Object.getPrototypeOf;\
        var realIsProto = Object.prototype.isPrototypeOf;\
        var virtualChain = [];\
        function recordChain(p){\
            var cur = p;\
            while (cur && cur !== Object.prototype) {\
                if (virtualChain.indexOf(cur) === -1) virtualChain.push(cur);\
                cur = getProto(cur);\
            }\
        }\
        function flatten(t, p){\
            var chain = [];\
            var cur = p;\
            while (cur && cur !== Object.prototype && cur !== Function.prototype) {\
                chain.push(cur); cur = getProto(cur);\
            }\
            for (var i = chain.length - 1; i >= 0; i--) {\
                var proto = chain[i];\
                var keys = gopn(proto).concat(gops(proto));\
                for (var j = 0; j < keys.length; j++) {\
                    var k = keys[j];\
                    if (k === 'constructor') continue;\
                    if (Object.prototype.hasOwnProperty.call(t, k)) continue;\
                    var d = gdop(proto, k);\
                    if (!d) continue;\
                    try { dp(t, k, d); } catch(e) {}\
                }\
            }\
        }\
        function onGlobalProto(p){ flatten(g, p); recordChain(p); }\
        Object.setPrototypeOf = function(t, p){\
            if (t === g) { try { return realO(t, p); } catch(e) { onGlobalProto(p); return t; } }\
            return realO(t, p);\
        };\
        Reflect.setPrototypeOf = function(t, p){\
            if (t === g) { try { return realR(t, p); } catch(e) { onGlobalProto(p); return true; } }\
            return realR(t, p);\
        };\
        dp(Object.prototype, 'isPrototypeOf', { value: function(o){\
            if (o === g && virtualChain.indexOf(this) !== -1) return true;\
            return realIsProto.call(this, o);\
        }, writable: true, enumerable: false, configurable: true });\
    })()\0";
  unsafe {
    let js = JSStringCreateWithUTF8CString(
      SRC.as_ptr() as *const std::os::raw::c_char
    );
    let mut exc: JSValueRef = ptr::null();
    JSEvaluateScript(ctx, js, ptr::null_mut(), ptr::null_mut(), 1, &mut exc);
    JSStringRelease(js);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__Enter(this: *const Context) {
  let iso = current_iso();
  if iso.is_null() {
    return;
  }
  let st = iso_state(iso);
  st.contexts.push(ctx_of(this));
  refresh_current_ctx(st);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__Exit(_this: *const Context) {
  let iso = current_iso();
  if iso.is_null() {
    return;
  }
  let st = iso_state(iso);
  st.contexts.pop();
  refresh_current_ctx(st);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__Global(this: *const Context) -> *const Object {
  let ctx = ctx_of(this);
  let global = unsafe { JSContextGetGlobalObject(ctx) };
  intern_ctx::<Object>(ctx, global as JSValueRef)
}
