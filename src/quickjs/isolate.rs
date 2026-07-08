#![allow(non_snake_case, unused)]

use crate::quickjs::core::{
  ctx_of, current_ctx, current_host_defined_options, current_iso, intern,
  intern_dup, iso_state, jsval_of,
};
use crate::quickjs::quickjs_sys::*;
use crate::support::int;
use crate::{
  Context, Data, Function, MicrotaskQueue, MicrotasksPolicy, Object,
  RealIsolate, Value,
};
use std::mem::MaybeUninit;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::Ordering;

// --- code generation from strings (V8 `Context::AllowCodeGenerationFromStrings`)
//
// QuickJS has no native toggle for `eval` / `new Function`, so we emulate V8's
// per-context flag by swapping the context's global `eval` and `Function`
// bindings for a thrower while codegen is disallowed, and restoring the
// originals when it's re-allowed. The thrower raises an `EvalError` whose
// message matches V8's ("Code generation from strings disallowed for this
// context"), which now propagates to a C++ `TryCatch` correctly (see the
// `JS_HasException` ABI fix). Inert unless a context actually disables codegen,
// so it never touches the common path.
thread_local! {
  // ctx -> (saved global.eval, saved global.Function), both owning one ref.
  static CODEGEN_DISABLED: std::cell::RefCell<
    std::collections::HashMap<*mut JSContext, (JSValue, JSValue)>,
  > = std::cell::RefCell::new(std::collections::HashMap::new());
}

unsafe extern "C" fn codegen_thrower(
  ctx: *mut JSContext,
  _this: JSValue,
  _argc: c_int,
  _argv: *mut JSValue,
) -> JSValue {
  const MSG: &str = "Code generation from strings disallowed for this context";
  unsafe {
    // Build `new EvalError(MSG)` and throw it; fall back to a TypeError with
    // the same message if the EvalError constructor is somehow unavailable.
    let global = JS_GetGlobalObject(ctx);
    let ctor = JS_GetPropertyStr(ctx, global, c"EvalError".as_ptr());
    JS_FreeValue(ctx, global);
    if ctor.tag == JS_TAG_EXCEPTION || !JS_IsConstructor(ctx, ctor) {
      JS_FreeValue(ctx, ctor);
      // MSG is a fixed literal with no `%`, so using it directly as the
      // printf-style format string is safe.
      return JS_ThrowTypeError(
        ctx,
        c"Code generation from strings disallowed for this context".as_ptr(),
      );
    }
    let msg = JS_NewStringLen(ctx, MSG.as_ptr() as *const c_char, MSG.len());
    let mut args = [msg];
    let err = JS_CallConstructor(ctx, ctor, 1, args.as_mut_ptr());
    JS_FreeValue(ctx, ctor);
    JS_FreeValue(ctx, msg);
    if err.tag == JS_TAG_EXCEPTION {
      return err; // exception already pending
    }
    JS_Throw(ctx, err)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__AllowCodeGenerationFromStrings(
  this: *const Context,
  allow: bool,
) {
  let ctx = ctx_of(this);
  if ctx.is_null() {
    return;
  }
  unsafe {
    if allow {
      // Restore the original eval/Function bindings, if we swapped them.
      if let Some((eval, func)) =
        CODEGEN_DISABLED.with(|m| m.borrow_mut().remove(&ctx))
      {
        let global = JS_GetGlobalObject(ctx);
        JS_SetPropertyStr(ctx, global, c"eval".as_ptr(), eval);
        JS_SetPropertyStr(ctx, global, c"Function".as_ptr(), func);
        JS_FreeValue(ctx, global);
      }
    } else {
      // Already disabled? Nothing to do (don't clobber the saved originals).
      if CODEGEN_DISABLED.with(|m| m.borrow().contains_key(&ctx)) {
        return;
      }
      let global = JS_GetGlobalObject(ctx);
      // Save the current bindings (owning a ref each) so we can restore them.
      let saved_eval = JS_GetPropertyStr(ctx, global, c"eval".as_ptr());
      let saved_func = JS_GetPropertyStr(ctx, global, c"Function".as_ptr());
      let thrower_eval =
        JS_NewCFunction(ctx, codegen_thrower, c"eval".as_ptr(), 1);
      let thrower_func =
        JS_NewCFunction(ctx, codegen_thrower, c"Function".as_ptr(), 1);
      JS_SetPropertyStr(ctx, global, c"eval".as_ptr(), thrower_eval);
      JS_SetPropertyStr(ctx, global, c"Function".as_ptr(), thrower_func);
      JS_FreeValue(ctx, global);
      CODEGEN_DISABLED
        .with(|m| m.borrow_mut().insert(ctx, (saved_eval, saved_func)));
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context_IsCodeGenerationFromStringsAllowed(
  this: *const Context,
) -> bool {
  let ctx = ctx_of(this);
  if ctx.is_null() {
    return true;
  }
  !CODEGEN_DISABLED.with(|m| m.borrow().contains_key(&ctx))
}

/// Release any saved `eval`/`Function` bindings for `ctx` before its
/// `JSContext` is freed. Called from `v8__Isolate__Dispose` for every context.
pub(crate) fn codegen_release_ctx(ctx: *mut JSContext) {
  if let Some((eval, func)) =
    CODEGEN_DISABLED.with(|m| m.borrow_mut().remove(&ctx))
  {
    unsafe {
      JS_FreeValue(ctx, eval);
      JS_FreeValue(ctx, func);
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__FromSnapshot(
  isolate: *mut RealIsolate,
  context_snapshot_index: usize,
  _global_object: *const Value,
  _microtask_queue: *mut MicrotaskQueue,
) -> *const Context {
  let _ = context_snapshot_index;
  if isolate.is_null() {
    return ptr::null();
  }
  // No snapshot support on quickjs: the embedder falls back to New-init.
  ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetDataFromSnapshotOnce(
  this: *const Context,
  index: usize,
) -> *const Data {
  let _ = index;
  let ctx = super::core::ctx_of(this);
  let iso = super::core::current_iso();
  if ctx.is_null() || iso.is_null() {
    return ptr::null();
  }
  // No snapshot support on quickjs.
  ptr::null()
}

/// Native `getContinuationPreservedEmbedderData` for the extras binding object.
/// Reads the SAME per-thread store as
/// `v8__Context__GetContinuationPreservedEmbedderData` (`CONTINUATION_DATA`) so
/// the JS-visible "async context" (deno's `getAsyncContext`) and the native one
/// deno's Rust reads inside `promise_reject_callback` are one and the same.
/// Before this, the extras object stored it in a JS closure variable, so the
/// async context captured at promise-rejection time (read natively) was always
/// `undefined`.
unsafe extern "C" fn extras_get_cped(
  ctx: *mut JSContext,
  _this: JSValue,
  _argc: c_int,
  _argv: *mut JSValue,
) -> JSValue {
  let stored = CONTINUATION_DATA.with(|c| c.get());
  unsafe { JS_DupValue(ctx, stored) }
}

/// Native `setContinuationPreservedEmbedderData` for the extras binding object;
/// writes the shared `CONTINUATION_DATA` store (see [`extras_get_cped`]).
unsafe extern "C" fn extras_set_cped(
  ctx: *mut JSContext,
  _this: JSValue,
  argc: c_int,
  argv: *mut JSValue,
) -> JSValue {
  let v = if argc >= 1 {
    unsafe { *argv }
  } else {
    jsv_undefined()
  };
  let new = unsafe { JS_DupValue(ctx, v) };
  let old = CONTINUATION_DATA.with(|c| c.replace(new));
  unsafe { JS_FreeValue(ctx, old) };
  jsv_undefined()
}

/// Tape replay entry to the extras binding object (same shim object the
/// C-ABI fn returns, minus the handle-arg plumbing).
pub(crate) fn extras_binding_for_ctx(ctx: *mut JSContext) -> *const Object {
  if ctx.is_null() {
    return ptr::null();
  }
  extras_binding_impl(ctx)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetExtrasBindingObject(
  this: *const Context,
) -> *const Object {
  let ctx = ctx_of(this);
  if ctx.is_null() {
    return ptr::null();
  }
  let h = extras_binding_impl(ctx);
  return h;
}

fn extras_binding_impl(ctx: *mut JSContext) -> *const Object {
  unsafe {
    let o = JS_NewObject(ctx);
    let console = JS_NewObject(ctx);
    JS_SetPropertyStr(ctx, o, c"console".as_ptr(), console);

    let getf = JS_NewCFunction(
      ctx,
      extras_get_cped,
      c"getContinuationPreservedEmbedderData".as_ptr(),
      0,
    );
    let setf = JS_NewCFunction(
      ctx,
      extras_set_cped,
      c"setContinuationPreservedEmbedderData".as_ptr(),
      1,
    );
    JS_SetPropertyStr(
      ctx,
      o,
      c"getContinuationPreservedEmbedderData".as_ptr(),
      getf,
    );
    JS_SetPropertyStr(
      ctx,
      o,
      c"setContinuationPreservedEmbedderData".as_ptr(),
      setf,
    );
    intern::<Object>(o)
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

struct MicrotaskQueueState {
  _policy: MicrotasksPolicy,
}

pub(crate) fn new_microtask_queue_state(
  policy: MicrotasksPolicy,
) -> *mut MicrotaskQueue {
  Box::into_raw(Box::new(MicrotaskQueueState { _policy: policy }))
    as *mut MicrotaskQueue
}

pub(crate) unsafe fn drop_microtask_queue_state(queue: *mut MicrotaskQueue) {
  if !queue.is_null() {
    unsafe {
      drop(Box::from_raw(queue as *mut MicrotaskQueueState));
    }
  }
}

const PROMISE_REACTION_HOOK_SOURCE: &str = r#"
(function(beforeHook, afterHook) {
  var key = Symbol.for("v8x.quickjs.promiseHooks");
  var state = globalThis[key];
  if (!state) {
    state = {};
    state.originalThen = Promise.prototype.then;
    state.before = void 0;
    state.after = void 0;
    state.busy = false;
    Object.defineProperty(globalThis, key, { value: state });
    Promise.prototype.then = function(onFulfilled, onRejected) {
      var promise = this;
      function callHook(hook) {
        if (state.busy || typeof hook !== "function") return;
        state.busy = true;
        try {
          hook(promise);
        } catch (e) {
        } finally {
          state.busy = false;
        }
      }
      function wrap(handler, isReject) {
        var callable = typeof handler === "function";
        return function(value) {
          callHook(state.before);
          try {
            if (callable) return handler.call(this, value);
            if (isReject) throw value;
            return value;
          } finally {
            callHook(state.after);
          }
        };
      }
      return state.originalThen.call(
        this,
        wrap(onFulfilled, false),
        wrap(onRejected, true)
      );
    };
  }
  state.before = typeof beforeHook === "function" ? beforeHook : void 0;
  state.after = typeof afterHook === "function" ? afterHook : void 0;
})
"#;

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

unsafe fn install_promise_reaction_hooks(
  ctx: *mut JSContext,
  before_hook: JSValue,
  after_hook: JSValue,
) {
  let Ok(source) = std::ffi::CString::new(PROMISE_REACTION_HOOK_SOURCE) else {
    return;
  };
  let helper = unsafe {
    JS_Eval(
      ctx,
      source.as_ptr(),
      source.as_bytes().len(),
      c"<v8-promise-hooks>".as_ptr(),
      JS_EVAL_TYPE_GLOBAL,
    )
  };
  if helper.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return;
  }
  if unsafe { !JS_IsFunction(ctx, helper) } {
    unsafe { JS_FreeValue(ctx, helper) };
    return;
  }

  let mut args = [before_hook, after_hook];
  let ret =
    unsafe { JS_Call(ctx, helper, jsv_undefined(), 2, args.as_mut_ptr()) };
  unsafe { JS_FreeValue(ctx, helper) };
  if ret.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
  } else {
    unsafe { JS_FreeValue(ctx, ret) };
  }
}

thread_local! {
  // [init, before, after, resolve] JS hook fns (+1 ref each; undefined = unset)
  // and a re-entrancy guard.
  static PROMISE_HOOKS: std::cell::Cell<[JSValue; 4]> =
    std::cell::Cell::new([jsv_undefined(); 4]);
  static PROMISE_HOOK_CB: std::cell::Cell<Option<crate::isolate::PromiseHook>> =
    const { std::cell::Cell::new(None) };
  static PROMISE_HOOK_BUSY: std::cell::Cell<bool> = std::cell::Cell::new(false);
}

fn has_promise_hooks() -> bool {
  let has_js_hook =
    PROMISE_HOOKS.with(|h| h.get().iter().any(|v| !jsv_is_undefined(v)));
  has_js_hook || PROMISE_HOOK_CB.with(|h| h.get().is_some())
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
  let v8_ty = match ty {
    0 => crate::isolate::PromiseHookType::Init,
    1 => crate::isolate::PromiseHookType::Before,
    2 => crate::isolate::PromiseHookType::After,
    3 => crate::isolate::PromiseHookType::Resolve,
    _ => return,
  };
  // Guard against a hook that itself creates/awaits promises recursing forever.
  if PROMISE_HOOK_BUSY.with(|b| b.get()) {
    return;
  }
  let native_hook = PROMISE_HOOK_CB.with(|h| h.get());
  let f = PROMISE_HOOKS.with(|h| h.get()[idx]);
  if native_hook.is_none() && jsv_is_undefined(&f) {
    return;
  }
  PROMISE_HOOK_BUSY.with(|b| b.set(true));
  if let Some(hook) = native_hook {
    let promise_h = intern_dup::<crate::Promise>(ctx, promise);
    let parent_h = intern_dup::<Value>(ctx, parent);
    unsafe {
      hook(
        v8_ty,
        crate::Local::from_raw(promise_h).unwrap(),
        crate::Local::from_raw(parent_h).unwrap(),
      );
    }
  }
  // init (idx 0) also receives the parent promise.
  if !jsv_is_undefined(&f) {
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
  unsafe { install_promise_reaction_hooks(ctx, new[1], new[2]) };
  let old = PROMISE_HOOKS.with(|h| h.replace(new));
  for v in old {
    if !jsv_is_undefined(&v) {
      unsafe { JS_FreeValue(ctx, v) };
    }
  }
  let rt = unsafe { JS_GetRuntime(ctx) };
  unsafe {
    JS_SetPromiseHook(
      rt,
      if has_promise_hooks() {
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

/// Resolve the isolate to operate on: the explicit argument if non-null,
/// otherwise the current/last isolate on this thread (the C-ABI surface
/// sometimes passes null and relies on the thread's active isolate, mirroring
/// other entry points here).
fn terminate_target(isolate: *const RealIsolate) -> *mut RealIsolate {
  if isolate.is_null() {
    current_iso()
  } else {
    isolate as *mut RealIsolate
  }
}

/// QuickJS interrupt callback. Returns non-zero (→ uncatchable "interrupted"
/// error that unwinds the running script) once `TerminateExecution` is
/// requested. Polled at loop back-edges and calls, so it terminates a runaway
/// loop; the op-dispatch boundary (see `function.rs`) handles the more common
/// "first op after terminate" case immediately, without waiting for a poll.
pub(crate) unsafe extern "C" fn terminate_interrupt_handler(
  _rt: *mut JSRuntime,
  opaque: *mut c_void,
) -> c_int {
  let iso = opaque as *mut RealIsolate;
  if iso.is_null() {
    return 0;
  }
  run_pending_interrupts(iso);
  iso_state(iso).is_terminating() as c_int
}

pub(crate) fn run_pending_interrupts(iso: *mut RealIsolate) {
  if iso.is_null() {
    return;
  }
  let pending = {
    let mut pending = iso_state(iso)
      .pending_interrupts
      .lock()
      .unwrap_or_else(|poison| poison.into_inner());
    std::mem::take(&mut *pending)
  };
  let raw = crate::isolate::UnsafeRawIsolatePtr::from_real_ptr(iso);
  for entry in pending {
    unsafe { (entry.callback)(raw, entry.data) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__TerminateExecution(isolate: *const RealIsolate) {
  let iso = terminate_target(isolate);
  if iso.is_null() {
    return;
  }
  iso_state(iso)
    .terminating
    .store(true, std::sync::atomic::Ordering::Release);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__IsExecutionTerminating(
  isolate: *const RealIsolate,
) -> bool {
  let iso = terminate_target(isolate);
  !iso.is_null() && iso_state(iso).is_terminating()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__CancelTerminateExecution(
  isolate: *const RealIsolate,
) {
  let iso = terminate_target(isolate);
  if iso.is_null() {
    return;
  }
  iso_state(iso)
    .terminating
    .store(false, std::sync::atomic::Ordering::Release);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__RequestInterrupt(
  isolate: *const RealIsolate,
  callback: crate::isolate::InterruptCallback,
  data: *mut c_void,
) {
  let iso = terminate_target(isolate);
  if iso.is_null() {
    return;
  }
  let mut pending = iso_state(iso)
    .pending_interrupts
    .lock()
    .unwrap_or_else(|poison| poison.into_inner());
  pending.push(super::core::InterruptEntry { callback, data });
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
  isolate: *mut RealIsolate,
  callback: crate::isolate::PrepareStackTraceCallback<'static>,
) {
  // Install our V8-accurate `Error.prepareStackTrace` on this isolate's contexts
  // and any created later. It first computes corrected CallSite frames (the
  // fork's native CallSites carry placeholder column/flags), then forwards them
  // into deno's native `callback` so its formatter can apply source maps before
  // producing the final stack string.
  super::exception::set_prepare_stack_trace_cb(callback);
  if isolate.is_null() {
    return;
  }
  let st = super::core::iso_state(isolate);
  super::exception::install_prepare_stack_trace(st.ctx);
  for &c in &st.extra_contexts {
    super::exception::install_prepare_stack_trace(c);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetHeapStatistics(
  this: *mut RealIsolate,
  s: *mut crate::binding::v8__HeapStatistics,
) {
  if !s.is_null() {
    unsafe {
      ptr::write_bytes(
        s as *mut u8,
        0,
        std::mem::size_of::<crate::binding::v8__HeapStatistics>(),
      );
      if !this.is_null() {
        let st = iso_state(this);
        let mut usage = JSMemoryUsage::default();
        if !st.rt.is_null() {
          JS_ComputeMemoryUsage(st.rt, &mut usage);
        }
        let malloc_size = usage.malloc_size.max(0) as usize;
        let memory_used_size = usage.memory_used_size.max(0) as usize;
        let malloc_limit = usage.malloc_limit.max(0) as usize;
        let total_heap_size = malloc_size.max(memory_used_size).max(1);
        let heap_size_limit = if malloc_limit > total_heap_size {
          malloc_limit
        } else {
          total_heap_size.saturating_add(1)
        };
        let global_handle_count =
          st.global_handles.load(Ordering::SeqCst).max(0) as usize;
        let global_handle_bytes =
          global_handle_count.saturating_mul(std::mem::size_of::<JSValue>());

        (*s).total_heap_size_ = total_heap_size;
        (*s).total_physical_size_ = total_heap_size;
        (*s).total_available_size_ =
          heap_size_limit.saturating_sub(total_heap_size);
        (*s).used_heap_size_ = memory_used_size.max(1).min(total_heap_size);
        (*s).heap_size_limit_ = heap_size_limit;
        (*s).malloced_memory_ = malloc_size.max(1);
        (*s).peak_malloced_memory_ = malloc_size.max(1);
        (*s).external_memory_ =
          st.external_memory.load(Ordering::SeqCst).max(0) as usize;
        (*s).number_of_native_contexts_ =
          usize::from(st.main_ctx_claimed) + st.extra_contexts.len();
        (*s).total_global_handles_size_ = global_handle_bytes;
        (*s).used_global_handles_size_ = global_handle_bytes;
        (*s).total_allocated_bytes_ = total_heap_size as u64;
      }
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
  super::wasm::has_pending_streaming_task() || unsafe { JS_IsJobPending(st.rt) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetMicrotasksPolicy(
  isolate: *mut RealIsolate,
  policy: MicrotasksPolicy,
) {
  if isolate.is_null() {
    return;
  }
  iso_state(isolate).microtasks_policy = policy;
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

pub(crate) fn run_microtasks_if_auto() {
  let iso = current_iso();
  if iso.is_null() {
    return;
  }
  let st = iso_state(iso);
  if st.microtasks_policy == MicrotasksPolicy::Auto {
    drain_jobs(st.rt);
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

fn enqueue_microtask_function(ctx: *mut JSContext, function: *const Function) {
  if ctx.is_null() || function.is_null() {
    return;
  }
  let f = jsval_of(function);
  unsafe {
    if !JS_IsFunction(ctx, f) {
      return;
    }
    let src = c"(f)=>{Promise.resolve().then(f);}";
    let helper = JS_Eval(
      ctx,
      src.as_ptr(),
      src.to_bytes().len(),
      c"<enqueue-microtask>".as_ptr(),
      JS_EVAL_TYPE_GLOBAL,
    );
    if helper.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      return;
    }
    if !JS_IsFunction(ctx, helper) {
      JS_FreeValue(ctx, helper);
      return;
    }
    let mut argv = [JS_DupValue(ctx, f)];
    let ret = JS_Call(ctx, helper, jsv_undefined(), 1, argv.as_mut_ptr());
    JS_FreeValue(ctx, helper);
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
pub extern "C" fn v8__Isolate__EnqueueMicrotask(
  _isolate: *mut RealIsolate,
  function: *const Function,
) {
  let ctx = current_ctx();
  enqueue_microtask_function(ctx, function);
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

  let event: usize = if is_handled != 0 { 1 } else { 0 };
  let promise_h = intern_dup::<crate::Promise>(ctx, promise);
  let reason_h = if event == 1 {
    ptr::null()
  } else {
    intern_dup::<Value>(ctx, reason)
  };
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
  callback: unsafe extern "C" fn(*const crate::function::FunctionCallbackInfo),
) {
  super::wasm::set_streaming_callback(Some(callback));
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
  policy: MicrotasksPolicy,
) -> *mut MicrotaskQueue {
  new_microtask_queue_state(policy)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__MicrotaskQueue__DESTRUCT(queue: *mut MicrotaskQueue) {
  unsafe { drop_microtask_queue_state(queue) };
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
  enqueue_microtask_function(ctx, microtask);
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
  buf: *mut std::ffi::c_void,
  isolate: *mut RealIsolate,
) {
  if !buf.is_null() {
    unsafe {
      ptr::write_bytes(buf as *mut u8, 0, 2 * std::mem::size_of::<usize>());
      *(buf as *mut usize) = isolate as usize;
    }
  }
  if !isolate.is_null() {
    iso_state(isolate).javascript_execution_allow_depth += 1;
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__AllowJavascriptExecutionScope__DESTRUCT(
  this: *mut std::ffi::c_void,
) {
  if this.is_null() {
    return;
  }
  let isolate = unsafe { *(this as *const usize) as *mut RealIsolate };
  if !isolate.is_null() {
    let st = iso_state(isolate);
    st.javascript_execution_allow_depth =
      st.javascript_execution_allow_depth.saturating_sub(1);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__DisallowJavascriptExecutionScope__CONSTRUCT(
  buf: *mut std::ffi::c_void,
  isolate: *mut RealIsolate,
  on_failure: crate::scope::OnFailure,
) {
  if !buf.is_null() {
    unsafe {
      ptr::write_bytes(buf as *mut u8, 0, 2 * std::mem::size_of::<usize>());
      *(buf as *mut usize) = isolate as usize;
    };
  }
  if !isolate.is_null() {
    iso_state(isolate)
      .javascript_execution_disallow_scopes
      .push(on_failure);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__DisallowJavascriptExecutionScope__DESTRUCT(
  this: *mut std::ffi::c_void,
) {
  if this.is_null() {
    return;
  }
  let isolate = unsafe { *(this as *const usize) as *mut RealIsolate };
  if !isolate.is_null() {
    iso_state(isolate)
      .javascript_execution_disallow_scopes
      .pop();
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__IsSandboxEnabled() -> bool {
  false
}

// ---------------------------------------------------------------------------
// Link-stubs for v8 C-ABI symbols that `test_api.rs` references but this
// backend doesn't implement yet. Each returns a benign default
// (null / 0 / false / `Nothing`) so the target LINKS and the many tests that
// don't touch these paths run; tests that do exercise them fail gracefully
// without crashing. Promote individual stubs to real implementations over time.
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetMicrotaskQueue(
  this: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  let iso = current_iso();
  if iso.is_null() {
    return ptr::null();
  }
  let ctx = ctx_of(this as *const Context);
  if ctx.is_null() {
    return ptr::null();
  }
  let st = iso_state(iso);
  st.context_microtask_queues
    .get(&(ctx as usize))
    .copied()
    .unwrap_or(st.default_microtask_queue) as *const std::os::raw::c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__SetMicrotaskQueue(
  this: *const std::os::raw::c_void,
  microtask_queue: *const std::os::raw::c_void,
) {
  let iso = current_iso();
  if iso.is_null() {
    return;
  }
  let ctx = ctx_of(this as *const Context);
  if ctx.is_null() {
    return;
  }
  let st = iso_state(iso);
  if microtask_queue.is_null() {
    st.context_microtask_queues.remove(&(ctx as usize));
  } else {
    st.context_microtask_queues
      .insert(ctx as usize, microtask_queue as *mut MicrotaskQueue);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CpuProfiler__CollectSample(
  _isolate: *mut std::os::raw::c_void,
  _trace_id: *const std::os::raw::c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CpuProfiler__UseDetailedSourcePositionsForProfiling(
  _isolate: *mut std::os::raw::c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__AddMessageListener(
  isolate: *mut std::os::raw::c_void,
  callback: crate::isolate::MessageCallback,
) -> bool {
  if isolate.is_null() {
    return false;
  }
  iso_state(isolate as *mut RealIsolate)
    .message_listeners
    .push(callback);
  true
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__AddMessageListenerWithErrorLevel(
  isolate: *mut std::os::raw::c_void,
  callback: crate::isolate::MessageCallback,
  _message_levels: crate::isolate::MessageErrorLevel,
) -> bool {
  v8__Isolate__AddMessageListener(isolate, callback)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__ClearKeptObjects(
  isolate: *mut std::os::raw::c_void,
) {
  if !isolate.is_null() {
    let st = iso_state(isolate as *mut RealIsolate);
    st.kept_objects_cleared = true;
    crate::quickjs::core::clear_kept_objects_for_context(st.ctx);
    for ctx in &st.extra_contexts {
      crate::quickjs::core::clear_kept_objects_for_context(*ctx);
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetCurrentHostDefinedOptions(
  _this: *mut std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  current_host_defined_options() as *const std::os::raw::c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetDataFromSnapshotOnce(
  this: *mut std::os::raw::c_void,
  index: usize,
) -> *const std::os::raw::c_void {
  let iso = this as *mut RealIsolate;
  if iso.is_null() {
    return ptr::null();
  }
  let _ = index;
  // No snapshot support on quickjs.
  ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetMicrotasksPolicy(
  isolate: *const std::os::raw::c_void,
) -> crate::MicrotasksPolicy {
  if isolate.is_null() {
    return crate::MicrotasksPolicy::Auto;
  }
  iso_state(isolate as *mut RealIsolate).microtasks_policy
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__MemoryPressureNotification(
  _this: *mut std::os::raw::c_void,
  _level: u8,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__RemoveGCEpilogueCallback(
  isolate: *mut std::os::raw::c_void,
  callback: *const std::os::raw::c_void,
  data: *mut std::os::raw::c_void,
) {
  if !isolate.is_null() {
    iso_state(isolate as *mut RealIsolate)
      .gc_epilogue_callbacks
      .retain(|entry| {
        entry.callback as *const std::os::raw::c_void != callback
          || entry.data != data
      });
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__RemoveGCPrologueCallback(
  isolate: *mut std::os::raw::c_void,
  callback: *const std::os::raw::c_void,
  data: *mut std::os::raw::c_void,
) {
  if !isolate.is_null() {
    iso_state(isolate as *mut RealIsolate)
      .gc_prologue_callbacks
      .retain(|entry| {
        entry.callback as *const std::os::raw::c_void != callback
          || entry.data != data
      });
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetAllowAtomicsWait(
  isolate: *mut std::os::raw::c_void,
  allow: bool,
) {
  if isolate.is_null() {
    return;
  }
  let rt = iso_state(isolate as *mut RealIsolate).rt;
  unsafe { JS_SetCanBlock(rt, allow) };
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetOOMErrorHandler(
  _isolate: *mut std::os::raw::c_void,
  _callback: *const std::os::raw::c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetPromiseHook(
  isolate: *mut RealIsolate,
  hook: crate::isolate::PromiseHook,
) {
  PROMISE_HOOK_CB.with(|h| h.set(Some(hook)));
  let iso = if isolate.is_null() {
    current_iso()
  } else {
    isolate
  };
  if iso.is_null() {
    return;
  }
  let rt = iso_state(iso).rt;
  if rt.is_null() {
    return;
  }
  unsafe {
    JS_SetPromiseHook(rt, Some(promise_hook_trampoline), ptr::null_mut());
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetUseCounterCallback(
  isolate: *mut RealIsolate,
  callback: crate::isolate::UseCounterCallback,
) {
  if isolate.is_null() {
    return;
  }
  iso_state(isolate).use_counter_callback = Some(callback);
}
