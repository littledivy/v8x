#![allow(non_snake_case, unused)]

use crate::quickjs::core::{
  ctx_of, current_ctx, current_iso, intern, intern_dup, iso_state, jsval_of,
};
use crate::quickjs::quickjs_sys::*;
use crate::support::int;
use crate::{
  Context, Data, Function, MicrotaskQueue, MicrotasksPolicy, Object,
  RealIsolate, Value,
};
use std::mem::MaybeUninit;
use std::os::raw::c_void;
use std::ptr;

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__AllowCodeGenerationFromStrings(
  _this: *const Context,
  _allow: bool,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context_IsCodeGenerationFromStringsAllowed(
  _this: *const Context,
) -> bool {
  true
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__FromSnapshot(
  isolate: *mut RealIsolate,
  _context_snapshot_index: usize,
  _global_object: *const Value,
  _microtask_queue: *mut MicrotaskQueue,
) -> *const Context {
  if isolate.is_null() {
    return ptr::null();
  }
  super::core::intern_ctx(super::core::iso_state(isolate).ctx)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetDataFromSnapshotOnce(
  _this: *const Context,
  _index: usize,
) -> *const Data {
  ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetExtrasBindingObject(
  this: *const Context,
) -> *const Object {
  let ctx = ctx_of(this);
  if ctx.is_null() {
    return ptr::null();
  }
  const SRC: &[u8] = b"(function(){\
        var __cped = undefined;\
        return {\
            console: {},\
            getContinuationPreservedEmbedderData: function(){ return __cped; },\
            setContinuationPreservedEmbedderData: function(v){ __cped = v; },\
        };\
    })()\0";
  let fname = c"<extras>";
  let obj = unsafe {
    JS_Eval(
      ctx,
      SRC.as_ptr() as *const std::os::raw::c_char,
      SRC.len() - 1,
      fname.as_ptr(),
      JS_EVAL_TYPE_GLOBAL,
    )
  };
  if obj.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    let o = unsafe { JS_NewObject(ctx) };
    let console = unsafe { JS_NewObject(ctx) };
    unsafe {
      JS_SetPropertyStr(ctx, o, c"console".as_ptr(), console);
    }
    return intern::<Object>(o);
  }

  intern::<Object>(obj)
}

thread_local! {
    static EMBEDDER_DATA: std::cell::RefCell<
        std::collections::HashMap<usize, Vec<*mut c_void>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

fn embedder_slots_with<R>(
  this: *const Context,
  grow_to: Option<usize>,
  f: impl FnOnce(&mut Vec<*mut c_void>) -> R,
) -> R {
  let key = super::core::ctx_of(this) as usize;
  EMBEDDER_DATA.with(|m| {
    let mut map = m.borrow_mut();
    let v = map.entry(key).or_default();
    if let Some(n) = grow_to {
      if v.len() < n {
        v.resize(n, ptr::null_mut());
      }
    }
    f(v)
  })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetNumberOfEmbedderDataFields(
  this: *const Context,
) -> u32 {
  embedder_slots_with(this, None, |v| v.len() as u32)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetAlignedPointerFromEmbedderData(
  this: *const Context,
  index: int,
) -> *mut c_void {
  if index < 0 {
    return ptr::null_mut();
  }
  embedder_slots_with(this, None, |v| {
    v.get(index as usize).copied().unwrap_or(ptr::null_mut())
  })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__SetAlignedPointerInEmbedderData(
  this: *const Context,
  index: int,
  value: *mut c_void,
) {
  if index < 0 {
    return;
  }
  let idx = index as usize;
  embedder_slots_with(this, Some(idx + 1), |v| {
    v[idx] = value;
  });
}

type JSPromiseHookType = i32;

unsafe extern "C" {
  fn JS_SetPromiseHook(
    rt: *mut JSRuntime,
    hook: Option<
      unsafe extern "C" fn(
        ctx: *mut JSContext,
        ty: JSPromiseHookType,
        promise: JSValue,
        parent: JSValue,
        opaque: *mut c_void,
      ),
    >,
    opaque: *mut c_void,
  );
}

thread_local! {
  // [init, before, after, resolve] JS hook fns (+1 ref each; undefined = unset)
  // and a re-entrancy guard.
  static PROMISE_HOOKS: std::cell::Cell<[JSValue; 4]> =
    std::cell::Cell::new([jsv_undefined(); 4]);
  static PROMISE_HOOK_BUSY: std::cell::Cell<bool> = std::cell::Cell::new(false);
}

unsafe extern "C" fn promise_hook_trampoline(
  ctx: *mut JSContext,
  ty: JSPromiseHookType,
  promise: JSValue,
  parent: JSValue,
  _opaque: *mut c_void,
) {
  let idx = ty as usize;
  if idx >= 4 || ctx.is_null() {
    return;
  }
  // Guard against a hook that itself creates/awaits promises recursing forever.
  if PROMISE_HOOK_BUSY.with(|b| b.get()) {
    return;
  }
  let f = PROMISE_HOOKS.with(|h| h.get()[idx]);
  if jsv_is_undefined(&f) {
    return;
  }
  PROMISE_HOOK_BUSY.with(|b| b.set(true));
  // init (idx 0) also receives the parent promise.
  let mut args = [promise, parent];
  let argc = if idx == 0 { 2 } else { 1 };
  let ret =
    unsafe { JS_Call(ctx, f, jsv_undefined(), argc, args.as_mut_ptr()) };
  if ret.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
  } else {
    unsafe { JS_FreeValue(ctx, ret) };
  }
  PROMISE_HOOK_BUSY.with(|b| b.set(false));
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__SetPromiseHooks(
  _this: *const Context,
  init_hook: *const Function,
  before_hook: *const Function,
  after_hook: *const Function,
  resolve_hook: *const Function,
) {
  let ctx = current_ctx();
  if ctx.is_null() {
    return;
  }
  let promote = |p: *const Function| -> JSValue {
    if p.is_null() {
      jsv_undefined()
    } else {
      let v = jsval_of(p);
      if jsv_is_undefined(&v) {
        jsv_undefined()
      } else {
        unsafe { JS_DupValue(ctx, v) }
      }
    }
  };
  let new = [
    promote(init_hook),
    promote(before_hook),
    promote(after_hook),
    promote(resolve_hook),
  ];
  let old = PROMISE_HOOKS.with(|h| h.replace(new));
  for v in old {
    if !jsv_is_undefined(&v) {
      unsafe { JS_FreeValue(ctx, v) };
    }
  }
  let any = new.iter().any(|v| !jsv_is_undefined(v));
  let rt = unsafe { JS_GetRuntime(ctx) };
  unsafe {
    JS_SetPromiseHook(
      rt,
      if any {
        Some(promise_hook_trampoline)
      } else {
        None
      },
      ptr::null_mut(),
    )
  };
}

thread_local! {

    static CONTINUATION_DATA: std::cell::Cell<JSValue> =
        std::cell::Cell::new(jsv_undefined());
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__SetContinuationPreservedEmbedderData(
  _this: *mut RealIsolate,
  value: *const Value,
) {
  let ctx = current_ctx();
  let new = if value.is_null() {
    jsv_undefined()
  } else if ctx.is_null() {
    jsval_of(value)
  } else {
    unsafe { JS_DupValue(ctx, jsval_of(value)) }
  };
  let old = CONTINUATION_DATA.with(|c| c.replace(new));
  if !ctx.is_null() {
    unsafe { JS_FreeValue(ctx, old) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetContinuationPreservedEmbedderData(
  _this: *mut RealIsolate,
) -> *const Value {
  let stored = CONTINUATION_DATA.with(|c| c.get());
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }
  intern_dup::<Value>(ctx, stored)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__TerminateExecution(
  _isolate: *const RealIsolate,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__IsExecutionTerminating(
  _isolate: *const RealIsolate,
) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__CancelTerminateExecution(
  _isolate: *const RealIsolate,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__RequestInterrupt(
  _isolate: *const RealIsolate,
  _callback: crate::isolate::InterruptCallback,
  _data: *mut c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__ThrowException(
  _isolate: *mut RealIsolate,
  exception: *const Value,
) -> *const Value {
  let ctx = current_ctx();
  if ctx.is_null() || exception.is_null() {
    return exception;
  }
  let v = jsval_of(exception);
  let dup = unsafe { JS_DupValue(ctx, v) };
  unsafe { JS_Throw(ctx, dup) };
  intern_dup::<Value>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetCaptureStackTraceForUncaughtExceptions(
  _this: *mut RealIsolate,
  _capture: bool,
  _frame_limit: i32,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetPrepareStackTraceCallback(
  _isolate: *mut RealIsolate,
  _callback: crate::isolate::PrepareStackTraceCallback<'static>,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetHeapStatistics(
  _this: *mut RealIsolate,
  s: *mut crate::binding::v8__HeapStatistics,
) {
  if !s.is_null() {
    unsafe {
      ptr::write_bytes(
        s as *mut u8,
        0,
        std::mem::size_of::<crate::binding::v8__HeapStatistics>(),
      );
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__RemoveNearHeapLimitCallback(
  _isolate: *mut RealIsolate,
  _callback: crate::isolate::NearHeapLimitCallback,
  _heap_limit: usize,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetIdle(
  _isolate: *mut RealIsolate,
  _is_idle: bool,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__HasPendingBackgroundTasks(
  isolate: *const RealIsolate,
) -> bool {
  if isolate.is_null() {
    return false;
  }
  let st = iso_state(isolate as *mut RealIsolate);
  if st.rt.is_null() {
    return false;
  }
  unsafe { JS_IsJobPending(st.rt) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetMicrotasksPolicy(
  _isolate: *mut RealIsolate,
  _policy: MicrotasksPolicy,
) {
}

fn drain_jobs(rt: *mut JSRuntime) {
  if rt.is_null() {
    return;
  }
  unsafe {
    let mut pctx: *mut JSContext = ptr::null_mut();

    loop {
      let r = JS_ExecutePendingJob(rt, &mut pctx);
      if r == 0 {
        break;
      }
      if r < 0 {
        if !pctx.is_null() {
          let exc = JS_GetException(pctx);
          JS_FreeValue(pctx, exc);
        }
      }
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__PerformMicrotaskCheckpoint(
  isolate: *mut RealIsolate,
) {
  if isolate.is_null() {
    return;
  }
  let st = iso_state(isolate);
  drain_jobs(st.rt);
}

thread_local! {

    static ENQUEUE_HELPER: std::cell::Cell<Option<JSValue>> = const { std::cell::Cell::new(None) };
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__EnqueueMicrotask(
  _isolate: *mut RealIsolate,
  function: *const Function,
) {
  let ctx = current_ctx();
  if ctx.is_null() || function.is_null() {
    return;
  }
  let f = jsval_of(function);
  unsafe {
    if JS_IsFunction(ctx, f) == 0 {
      return;
    }
    let helper = ENQUEUE_HELPER.with(|c| {
      if let Some(h) = c.get() {
        return h;
      }
      let src = c"(f)=>{Promise.resolve().then(f);}";
      let h = JS_Eval(
        ctx,
        src.as_ptr(),
        src.to_bytes().len(),
        c"<enqueue-microtask>".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      if h.tag == JS_TAG_EXCEPTION {
        let exc = JS_GetException(ctx);
        JS_FreeValue(ctx, exc);
        return jsv_undefined();
      }
      c.set(Some(h));
      h
    });
    if JS_IsFunction(ctx, helper) == 0 {
      return;
    }
    let mut argv = [JS_DupValue(ctx, f)];
    let ret = JS_Call(ctx, helper, jsv_undefined(), 1, argv.as_mut_ptr());
    JS_FreeValue(ctx, argv[0]);
    if ret.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
    } else {
      JS_FreeValue(ctx, ret);
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetHostInitializeImportMetaObjectCallback(
  _isolate: *mut RealIsolate,
  callback: crate::isolate::HostInitializeImportMetaObjectCallback,
) {
  super::module::set_import_meta_callback(callback);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetHostImportModuleDynamicallyCallback(
  _isolate: *mut RealIsolate,
  callback: crate::isolate::RawHostImportModuleDynamicallyCallback,
) {
  super::module::set_dynamic_import_callback(callback);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetHostImportModuleWithPhaseDynamicallyCallback(
  _isolate: *mut RealIsolate,
  _callback: crate::isolate::RawHostImportModuleWithPhaseDynamicallyCallback,
) {
}

#[cfg(not(target_os = "windows"))]
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetHostCreateShadowRealmContextCallback(
  _isolate: *mut RealIsolate,
  _callback: unsafe extern "C" fn(
    initiator_context: crate::Local<Context>,
  ) -> *mut Context,
) {
}

#[cfg(target_os = "windows")]
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetHostCreateShadowRealmContextCallback(
  _isolate: *mut RealIsolate,
  _callback: unsafe extern "C" fn(
    rv: *mut *mut Context,
    initiator_context: crate::Local<Context>,
  ) -> *mut *mut Context,
) {
}

thread_local! {

    static PROMISE_REJECT_CB: std::cell::Cell<
        Option<crate::isolate::PromiseRejectCallback>,
    > = const { std::cell::Cell::new(None) };
}

unsafe extern "C" fn promise_rejection_tracker(
  ctx: *mut JSContext,
  promise: JSValue,
  reason: JSValue,
  is_handled: std::os::raw::c_int,
  _opaque: *mut c_void,
) {
  let cb = PROMISE_REJECT_CB.with(|c| c.get());
  let Some(cb) = cb else { return };

  let promise_h = intern_dup::<crate::Promise>(ctx, promise);
  let reason_h = intern_dup::<Value>(ctx, reason);

  let event: usize = if is_handled != 0 { 1 } else { 0 };
  let msg: [usize; 3] = [promise_h as usize, reason_h as usize, event];
  unsafe {
    cb(std::mem::transmute::<
      [usize; 3],
      crate::promise::PromiseRejectMessage,
    >(msg));
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetPromiseRejectCallback(
  isolate: *mut RealIsolate,
  callback: crate::isolate::PromiseRejectCallback,
) {
  PROMISE_REJECT_CB.with(|c| c.set(Some(callback)));
  let iso = if isolate.is_null() {
    current_iso()
  } else {
    isolate
  };
  if iso.is_null() {
    return;
  }
  let st = iso_state(iso);
  if st.rt.is_null() {
    return;
  }
  unsafe {
    JS_SetHostPromiseRejectionTracker(
      st.rt,
      Some(promise_rejection_tracker),
      ptr::null_mut(),
    );
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetWasmAsyncResolvePromiseCallback(
  _isolate: *mut RealIsolate,
  _callback: crate::isolate::WasmAsyncResolvePromiseCallback,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetWasmStreamingCallback(
  _isolate: *mut RealIsolate,
  _callback: unsafe extern "C" fn(*const crate::function::FunctionCallbackInfo),
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetEnteredOrMicrotaskContext(
  isolate: *mut RealIsolate,
) -> *const Context {
  if isolate.is_null() {
    return ptr::null();
  }
  let st = iso_state(isolate);
  let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
  super::core::intern_ctx(ctx)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__MicrotaskQueue__New(
  _isolate: *mut RealIsolate,
  _policy: MicrotasksPolicy,
) -> *mut MicrotaskQueue {
  let b: Box<u8> = Box::new(0);
  Box::into_raw(b) as *mut MicrotaskQueue
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__MicrotaskQueue__DESTRUCT(queue: *mut MicrotaskQueue) {
  if !queue.is_null() {
    unsafe {
      drop(Box::from_raw(queue as *mut u8));
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__MicrotaskQueue__PerformCheckpoint(
  isolate: *mut RealIsolate,
  _queue: *const MicrotaskQueue,
) {
  if isolate.is_null() {
    return;
  }
  let st = iso_state(isolate);
  drain_jobs(st.rt);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__MicrotaskQueue__IsRunningMicrotasks(
  _queue: *const MicrotaskQueue,
) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__MicrotaskQueue__GetMicrotasksScopeDepth(
  _queue: *const MicrotaskQueue,
) -> int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__MicrotaskQueue__EnqueueMicrotask(
  _isolate: *mut RealIsolate,
  _queue: *const MicrotaskQueue,
  microtask: *const Function,
) {
  let ctx = current_ctx();
  if ctx.is_null() || microtask.is_null() {
    return;
  }
  let f = jsval_of(microtask);
  unsafe {
    if JS_IsFunction(ctx, f) != 0 {
      let ret = JS_Call(ctx, f, jsv_undefined(), 0, ptr::null_mut());
      if ret.tag == JS_TAG_EXCEPTION {
        let exc = JS_GetException(ctx);
        JS_FreeValue(ctx, exc);
      } else {
        JS_FreeValue(ctx, ret);
      }
    }
  }
}

type RC = crate::isolate_create_params::raw::ResourceConstraints;

#[inline(always)]
unsafe fn rc_word(c: *const RC, idx: usize) -> usize {
  unsafe { *(c as *const usize).add(idx) }
}
#[inline(always)]
unsafe fn rc_set_word(c: *mut RC, idx: usize, v: usize) {
  unsafe { *(c as *mut usize).add(idx) = v };
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__ConfigureDefaults(
  constraints: *mut RC,
  _physical_memory: u64,
  _virtual_memory_limit: u64,
) {
  if !constraints.is_null() {
    unsafe {
      ptr::write_bytes(constraints as *mut u8, 0, std::mem::size_of::<RC>())
    };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__ConfigureDefaultsFromHeapSize(
  constraints: *mut RC,
  _initial_heap_size_in_bytes: usize,
  _maximum_heap_size_in_bytes: usize,
) {
  if !constraints.is_null() {
    unsafe {
      ptr::write_bytes(constraints as *mut u8, 0, std::mem::size_of::<RC>())
    };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__code_range_size_in_bytes(
  constraints: *const RC,
) -> usize {
  if constraints.is_null() {
    return 0;
  }
  unsafe { rc_word(constraints, 0) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__set_code_range_size_in_bytes(
  constraints: *mut RC,
  limit: usize,
) {
  if !constraints.is_null() {
    unsafe { rc_set_word(constraints, 0, limit) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__max_old_generation_size_in_bytes(
  constraints: *const RC,
) -> usize {
  if constraints.is_null() {
    return 0;
  }
  unsafe { rc_word(constraints, 1) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__set_max_old_generation_size_in_bytes(
  constraints: *mut RC,
  limit: usize,
) {
  if !constraints.is_null() {
    unsafe { rc_set_word(constraints, 1, limit) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__max_young_generation_size_in_bytes(
  constraints: *const RC,
) -> usize {
  if constraints.is_null() {
    return 0;
  }
  unsafe { rc_word(constraints, 2) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__set_max_young_generation_size_in_bytes(
  constraints: *mut RC,
  limit: usize,
) {
  if !constraints.is_null() {
    unsafe { rc_set_word(constraints, 2, limit) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__initial_old_generation_size_in_bytes(
  constraints: *const RC,
) -> usize {
  if constraints.is_null() {
    return 0;
  }
  unsafe { rc_word(constraints, 3) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__set_initial_old_generation_size_in_bytes(
  constraints: *mut RC,
  initial_size: usize,
) {
  if !constraints.is_null() {
    unsafe { rc_set_word(constraints, 3, initial_size) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__initial_young_generation_size_in_bytes(
  constraints: *const RC,
) -> usize {
  if constraints.is_null() {
    return 0;
  }
  unsafe { rc_word(constraints, 4) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__set_initial_young_generation_size_in_bytes(
  constraints: *mut RC,
  initial_size: usize,
) {
  if !constraints.is_null() {
    unsafe { rc_set_word(constraints, 4, initial_size) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__stack_limit(
  constraints: *const RC,
) -> *mut u32 {
  if constraints.is_null() {
    return ptr::null_mut();
  }

  unsafe { *((constraints as *const usize).add(6) as *const *mut u32) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__set_stack_limit(
  constraints: *mut RC,
  value: *mut u32,
) {
  if !constraints.is_null() {
    unsafe { *((constraints as *mut usize).add(6) as *mut *mut u32) = value };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__AllowJavascriptExecutionScope__CONSTRUCT(
  _buf: *mut std::ffi::c_void,
  _isolate: *mut RealIsolate,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__AllowJavascriptExecutionScope__DESTRUCT(
  _this: *mut std::ffi::c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__DisallowJavascriptExecutionScope__CONSTRUCT(
  buf: *mut std::ffi::c_void,
  _isolate: *mut RealIsolate,
  _on_failure: crate::scope::OnFailure,
) {
  if !buf.is_null() {
    unsafe {
      ptr::write_bytes(buf as *mut u8, 0, 2 * std::mem::size_of::<usize>())
    };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__DisallowJavascriptExecutionScope__DESTRUCT(
  _this: *mut std::ffi::c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__IsSandboxEnabled() -> bool {
  false
}
