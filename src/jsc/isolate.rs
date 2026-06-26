//! Family: "isolate" — Isolate extras (heap stats, callbacks, microtasks),
//! Context extras, MicrotaskQueue, ResourceConstraints,
//! AllowJavascriptExecutionScope.
//!
//! JSC has no public API for most V8 isolate-level hooks (promise hooks,
//! module import callbacks, wasm, prepare-stack-trace, heap statistics, etc).
//! Those are implemented as safe inert defaults that let Deno degrade
//! gracefully. Microtasks are driven by JSC's own run loop, so explicit
//! checkpoints / enqueue are best-effort.
#![allow(non_snake_case, unused)]

use crate::jsc::core::{
  ctx_of, current_ctx, current_iso, intern, intern_ctx, iso_state, jsval,
};
use crate::jsc::jsc_sys::*;
use crate::{
  Context, Data, Function, MicrotaskQueue, MicrotasksPolicy, Object,
  RealIsolate, Value,
};
use std::mem::MaybeUninit;
use std::os::raw::c_void;
use std::ptr;

use crate::support::int;

unsafe extern "C" {
  fn JSGlobalContextSetUnhandledRejectionCallback(
    ctx: JSGlobalContextRef,
    function: JSObjectRef,
    exception: *mut JSValueRef,
  );
}

thread_local! {
    static PROMISE_REJECT_CB: std::cell::Cell<
        Option<crate::isolate::PromiseRejectCallback>,
    > = const { std::cell::Cell::new(None) };
}

unsafe extern "C" fn unhandled_rejection_trampoline(
  _ctx: JSContextRef,
  _function: JSObjectRef,
  _this: JSObjectRef,
  argc: usize,
  argv: *const JSValueRef,
  _exception: *mut JSValueRef,
) -> JSValueRef {
  if std::env::var("V82JSC_DEBUG").is_ok() {
    eprintln!(
      "[v82jsc] unhandled_rejection_trampoline fired argc={}",
      argc
    );
  }
  let cb = PROMISE_REJECT_CB.with(|c| c.get());
  if let Some(cb) = cb {
    // JSC's JSGlobalContextSetUnhandledRejectionCallback invokes the callback
    // as (promise, reason): argv[0] is the rejected promise, argv[1] is the
    // rejection reason. deno's PromiseRejectMessage::GetValue must return the
    // REASON; getting these backwards makes deno format the pending promise
    // itself ("Uncaught (in promise) Promise { <pending> }") instead of the
    // real error.
    let promise = if argc >= 1 {
      unsafe { *argv }
    } else {
      ptr::null()
    };
    let reason = if argc >= 2 {
      unsafe { *argv.add(1) }
    } else {
      ptr::null()
    };

    let msg: [usize; 3] = [promise as usize, reason as usize, 0];
    unsafe {
      cb(std::mem::transmute::<
        [usize; 3],
        crate::promise::PromiseRejectMessage,
      >(msg));
    }
  }
  unsafe { JSValueMakeUndefined(_ctx) }
}

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
  _isolate: *mut RealIsolate,
  _context_snapshot_index: usize,
  _global_object: *const Value,
  _microtask_queue: *mut MicrotaskQueue,
) -> *const Context {
  ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetExtrasBindingObject(
  this: *const Context,
) -> *const Object {
  let ctx = ctx_of(this);
  unsafe {
    const SRC: &[u8] = b"(function(){\
            var __cped = undefined;\
            return {\
                console: {},\
                getContinuationPreservedEmbedderData: function(){ return __cped; },\
                setContinuationPreservedEmbedderData: function(v){ __cped = v; },\
            };\
        })()\0";
    let js = JSStringCreateWithUTF8CString(
      SRC.as_ptr() as *const std::os::raw::c_char
    );
    let mut exc: JSValueRef = ptr::null();
    let obj =
      JSEvaluateScript(ctx, js, ptr::null_mut(), ptr::null_mut(), 1, &mut exc);
    JSStringRelease(js);
    if obj.is_null() {
      let o = JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut());
      let console = JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut());
      let name = JSStringCreateWithUTF8CString(c"console".as_ptr());
      JSObjectSetProperty(
        ctx,
        o,
        name,
        console as JSValueRef,
        0,
        ptr::null_mut(),
      );
      JSStringRelease(name);
      return intern_ctx::<Object>(ctx, o as JSValueRef);
    }
    intern_ctx::<Object>(ctx, obj)
  }
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
  EMBEDDER_DATA.with(|m| {
    let mut map = m.borrow_mut();
    let v = map.entry(this as usize).or_default();
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

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetEmbedderData(
  this: *const Context,
  index: int,
) -> *const Value {
  if index < 0 {
    return ptr::null();
  }
  let p = embedder_slots_with(this, None, |v| {
    v.get(index as usize).copied().unwrap_or(ptr::null_mut())
  });
  p as *const Value
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__SetEmbedderData(
  this: *const Context,
  index: int,
  value: *const Value,
) {
  if index < 0 {
    return;
  }
  let ctx = ctx_of(this);
  if !value.is_null() && !ctx.is_null() {
    unsafe { JSValueProtect(ctx, jsval(value)) };
  }
  let idx = index as usize;
  embedder_slots_with(this, Some(idx + 1), |v| {
    v[idx] = value as *mut c_void;
  });
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetDataFromSnapshotOnce(
  _this: *const Context,
  _index: usize,
) -> *const Data {
  ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__SetPromiseHooks(
  _this: *const Context,
  _init_hook: *const Function,
  _before_hook: *const Function,
  _after_hook: *const Function,
  _resolve_hook: *const Function,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__SetContinuationPreservedEmbedderData(
  _this: *mut RealIsolate,
  value: *const Value,
) {
  CONTINUATION_DATA.with(|c| c.set(value as JSValueRef));
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetContinuationPreservedEmbedderData(
  _this: *mut RealIsolate,
) -> *const Value {
  let stored = CONTINUATION_DATA.with(|c| c.get());
  let ctx = current_ctx();
  if !stored.is_null() {
    return intern_ctx::<Value>(ctx, stored);
  }
  if ctx.is_null() {
    return ptr::null();
  }
  let undef = unsafe { JSValueMakeUndefined(ctx) };
  intern_ctx::<Value>(ctx, undef)
}

thread_local! {
    static CONTINUATION_DATA: std::cell::Cell<JSValueRef> =
        const { std::cell::Cell::new(ptr::null()) };
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
  crate::jsc::core::record_pending_exception(ctx, jsval(exception));
  intern_ctx::<Value>(ctx, jsval(exception))
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
  _callback: crate::isolate::PrepareStackTraceCallback,
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
  _isolate: *const RealIsolate,
) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetMicrotasksPolicy(
  _isolate: *mut RealIsolate,
  _policy: MicrotasksPolicy,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__PerformMicrotaskCheckpoint(
  isolate: *mut RealIsolate,
) {
  if isolate.is_null() {
    return;
  }
  let ctx = current_ctx();
  if ctx.is_null() {
    return;
  }
  unsafe {
    // V8 runs enqueued microtasks (EnqueueMicrotask callbacks, e.g. deno's
    // op_queue_microtask) and promise reaction jobs in one FIFO at the
    // checkpoint -- NOT synchronously at enqueue time. Drain both here. A
    // callback may enqueue more microtasks / promise jobs, so loop until both
    // queues are quiet (bounded). JSC drains its own promise-job queue when a
    // JS API entry unwinds, hence the `JSEvaluateScript("0")` pump.
    let s = JSStringCreateWithUTF8CString(c"0".as_ptr());
    for _ in 0..64 {
      // Run all currently-queued embedder microtasks (FIFO). New ones queued
      // during a callback are picked up on the next inner iteration.
      loop {
        let item = MICROTASK_QUEUE.with(|q| {
          let mut b = q.borrow_mut();
          if b.is_empty() {
            None
          } else {
            Some(b.remove(0))
          }
        });
        let Some((gctx, f)) = item else { break };
        let mut exc: JSValueRef = ptr::null();
        JSObjectCallAsFunction(
          gctx,
          f,
          ptr::null_mut(),
          0,
          ptr::null(),
          &mut exc,
        );
        JSValueUnprotect(gctx, f as JSValueRef);
      }
      // Pump one layer of JSC promise-reaction jobs.
      let mut exc: JSValueRef = ptr::null();
      JSEvaluateScript(ctx, s, ptr::null_mut(), ptr::null_mut(), 1, &mut exc);
    }
    JSStringRelease(s);
  }
}

thread_local! {
    // Embedder microtasks queued via v8__Isolate__EnqueueMicrotask. Each entry
    // is a (global context, protected function) pair, run + unprotected at the
    // next PerformMicrotaskCheckpoint. Running them at enqueue time would invoke
    // ops re-entrantly (deno panics: "op_queue_microtask ... re-entrantly
    // invoked op_node_fs_close").
    static MICROTASK_QUEUE: std::cell::RefCell<
        Vec<(JSGlobalContextRef, JSObjectRef)>,
    > = const { std::cell::RefCell::new(Vec::new()) };
}

thread_local! {

    static CHECKPOINT_FN: std::cell::Cell<(JSGlobalContextRef, JSObjectRef)> =
        const { std::cell::Cell::new((ptr::null_mut(), ptr::null_mut())) };
}

unsafe fn checkpoint_noop_fn(ctx: JSContextRef) -> JSObjectRef {
  let gctx = unsafe { JSContextGetGlobalContext(ctx) };
  let cached = CHECKPOINT_FN.with(|c| c.get());
  if cached.0 == gctx && !cached.1.is_null() {
    return cached.1;
  }
  unsafe {
    let name = JSStringCreateWithUTF8CString(c"".as_ptr());
    let f =
      JSObjectMakeFunctionWithCallback(ctx, name, Some(checkpoint_noop_cb));
    JSStringRelease(name);
    if !f.is_null() {
      JSValueProtect(gctx, f as JSValueRef);
      CHECKPOINT_FN.with(|c| c.set((gctx, f)));
    }
    f
  }
}

unsafe extern "C" fn checkpoint_noop_cb(
  _ctx: JSContextRef,
  _function: JSObjectRef,
  _this: JSObjectRef,
  _argc: usize,
  _argv: *const JSValueRef,
  _exception: *mut JSValueRef,
) -> JSValueRef {
  unsafe { JSValueMakeUndefined(_ctx) }
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
  unsafe {
    let f = jsval(function) as JSObjectRef;
    if JSValueIsObject(ctx, jsval(function)) && JSObjectIsFunction(ctx, f) {
      // QUEUE the microtask; do NOT run it now. Running synchronously at
      // enqueue makes the callback (and any ops it calls) execute inside the
      // caller's op -> deno's op-reentrancy guard panics. Drained at the next
      // PerformMicrotaskCheckpoint.
      let gctx = JSContextGetGlobalContext(ctx);
      JSValueProtect(gctx, f as JSValueRef);
      MICROTASK_QUEUE.with(|q| q.borrow_mut().push((gctx, f)));
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetHostInitializeImportMetaObjectCallback(
  isolate: *mut RealIsolate,
  callback: crate::isolate::HostInitializeImportMetaObjectCallback,
) {
  if !isolate.is_null() {
    crate::jsc::core::iso_state(isolate).import_meta_cb = Some(callback);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetHostImportModuleDynamicallyCallback(
  _isolate: *mut RealIsolate,
  callback: crate::isolate::RawHostImportModuleDynamicallyCallback,
) {
  crate::jsc::module::set_dynamic_import_callback(callback);
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

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetPromiseRejectCallback(
  _isolate: *mut RealIsolate,
  callback: crate::isolate::PromiseRejectCallback,
) {
  PROMISE_REJECT_CB.with(|c| c.set(Some(callback)));
  let iso = if _isolate.is_null() {
    current_iso()
  } else {
    _isolate
  };
  if iso.is_null() {
    return;
  }
  let st = iso_state(iso);
  let ctxs: Vec<JSGlobalContextRef> = st
    .owned_contexts
    .iter()
    .chain(st.contexts.iter())
    .copied()
    .collect();
  if std::env::var("V82JSC_DEBUG").is_ok() {
    eprintln!("[v82jsc] SetPromiseRejectCallback: {} ctxs", ctxs.len());
  }
  for gctx in ctxs {
    if gctx.is_null() {
      continue;
    }
    unsafe { install_unhandled_rejection_bridge(gctx) };
  }
}

pub(crate) unsafe fn install_unhandled_rejection_bridge(
  gctx: JSGlobalContextRef,
) {
  unsafe {
    let name =
      JSStringCreateWithUTF8CString(c"__v8jsc_onUnhandledRejection".as_ptr());
    let f = JSObjectMakeFunctionWithCallback(
      gctx,
      name,
      Some(unhandled_rejection_trampoline),
    );
    JSStringRelease(name);
    if f.is_null() {
      return;
    }
    let mut exc: JSValueRef = ptr::null();
    JSGlobalContextSetUnhandledRejectionCallback(gctx, f, &mut exc);
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
  st.contexts.last().copied().unwrap_or(ptr::null_mut()) as *const Context
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__CreateParams__CONSTRUCT(
  buf: *mut MaybeUninit<crate::isolate_create_params::raw::CreateParams>,
) {
  if !buf.is_null() {
    unsafe {
      ptr::write_bytes(
        buf as *mut u8,
        0,
        std::mem::size_of::<crate::isolate_create_params::raw::CreateParams>(),
      );
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__CreateParams__SIZEOF() -> usize {
  std::mem::size_of::<crate::isolate_create_params::raw::CreateParams>()
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
  _isolate: *mut RealIsolate,
  _queue: *const MicrotaskQueue,
) {
  let ctx = current_ctx();
  if !ctx.is_null() {
    unsafe { JSGarbageCollect(ctx) };
  }
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
  unsafe {
    let f = jsval(microtask) as JSObjectRef;
    if JSValueIsObject(ctx, jsval(microtask)) && JSObjectIsFunction(ctx, f) {
      let mut exc: JSValueRef = ptr::null();
      JSObjectCallAsFunction(ctx, f, ptr::null_mut(), 0, ptr::null(), &mut exc);
    }
  }
}

type RC = crate::isolate_create_params::raw::ResourceConstraints;

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

#[inline(always)]
unsafe fn rc_word(c: *const RC, idx: usize) -> usize {
  unsafe { *(c as *const usize).add(idx) }
}
#[inline(always)]
unsafe fn rc_set_word(c: *mut RC, idx: usize, v: usize) {
  unsafe { *(c as *mut usize).add(idx) = v };
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
pub extern "C" fn v8__Isolate__AddGCPrologueCallback(
  _isolate: *mut RealIsolate,
  _callback: *const c_void,
  _data: *mut c_void,
  _gc_type_filter: i32,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__AddGCEpilogueCallback(
  _isolate: *mut RealIsolate,
  _callback: *const c_void,
  _data: *mut c_void,
  _gc_type_filter: i32,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__AdjustAmountOfExternalAllocatedMemory(
  _isolate: *mut RealIsolate,
  _change_in_bytes: i64,
) -> i64 {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__DateTimeConfigurationChangeNotification(
  _isolate: *mut RealIsolate,
  _time_zone_detection: i32,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__LowMemoryNotification(
  _isolate: *mut RealIsolate,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__NumberOfHeapSpaces(
  _isolate: *mut RealIsolate,
) -> usize {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetHeapSpaceStatistics(
  _isolate: *mut RealIsolate,
  space_statistics: *mut c_void,
  _index: usize,
) -> bool {
  if !space_statistics.is_null() {
    unsafe { ptr::write_bytes(space_statistics as *mut u8, 0, 40) };
  }
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetHeapCodeAndMetadataStatistics(
  _isolate: *mut RealIsolate,
  code_statistics: *mut c_void,
) -> bool {
  if !code_statistics.is_null() {
    unsafe { ptr::write_bytes(code_statistics as *mut u8, 0, 32) };
  }
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetAllowWasmCodeGenerationCallback(
  _isolate: *mut RealIsolate,
  _callback: *const c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__HeapProfiler__TakeHeapSnapshot(
  _isolate: *mut RealIsolate,
  _callback: *const c_void,
  _arg: *mut c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetSecurityToken(
  _this: *const Context,
) -> *const Value {
  ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__SetSecurityToken(
  _this: *const Context,
  _value: *const Value,
) {
}
