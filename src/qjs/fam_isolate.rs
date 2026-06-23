// Family: "isolate" — Isolate extras (heap stats, callbacks, microtasks),
// Context extras, MicrotaskQueue, ResourceConstraints,
// AllowJavascriptExecutionScope. QuickJS-ng backend.
//
// QuickJS has no direct analogue for most V8 isolate-level hooks (snapshots,
// promise-lifecycle hooks, module-import callbacks, wasm, prepare-stack-trace,
// granular heap statistics, cross-thread interrupts). Those are implemented as
// safe inert defaults so deno degrades gracefully — never `unimplemented!`.
//
// What IS real on QuickJS:
//   * microtask draining     -> JS_ExecutePendingJob loop (drains the job queue)
//   * pending background jobs -> JS_IsJobPending
//   * unhandled-rejection callback -> JS_SetHostPromiseRejectionTracker bridge
//   * ThrowException          -> JS_Throw on the current context
//   * EnqueueMicrotask        -> JS_Call (best-effort synchronous invoke; QuickJS
//                                 has no public "enqueue a job" entry point)
//   * Context embedder data / continuation-preserved data -> side tables
//   * ResourceConstraints     -> plain getters/setters over the #[repr(C)] struct
//
// Refcount discipline: every returned handle is routed through intern/intern_dup;
// every JSValue we create and don't keep is JS_FreeValue'd exactly once.
#![allow(non_snake_case, unused)]

use crate::qjs::quickjs_sys::*;
use crate::qjs::shim_core::{
    ctx_of, current_ctx, current_iso, intern, intern_dup, iso_state, jsval_of,
};
use crate::support::int;
use crate::{
    Context, Data, Function, MicrotaskQueue, MicrotasksPolicy, Object, RealIsolate, Value,
};
use std::mem::MaybeUninit;
use std::os::raw::c_void;
use std::ptr;

// ===================================================================
// Context extras: code-generation toggle (QuickJS always allows eval).
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__AllowCodeGenerationFromStrings(_this: *const Context, _allow: bool) {
    // TODO(qjs): QuickJS always permits eval / `new Function`; no toggle in the
    // public API. Inert no-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context_IsCodeGenerationFromStringsAllowed(_this: *const Context) -> bool {
    // QuickJS always permits eval / the Function constructor.
    true
}

// ===================================================================
// Context: snapshots (unsupported — QuickJS has no snapshot format).
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__FromSnapshot(
    isolate: *mut RealIsolate,
    _context_snapshot_index: usize,
    _global_object: *const Value,
    _microtask_queue: *mut MicrotaskQueue,
) -> *const Context {
    // QuickJS has no snapshot format. Returning null here makes deno_core treat
    // the context as un-deserialized; instead we hand back this isolate's fresh
    // context (same as `Context::New`). deno_core then re-runs the extension
    // bootstrap from source (`init_mode == New`), which is exactly what we want
    // since nothing was actually restored from a snapshot.
    if isolate.is_null() {
        return ptr::null();
    }
    super::shim_core::intern_ctx(super::shim_core::iso_state(isolate).ctx)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetDataFromSnapshotOnce(
    _this: *const Context,
    _index: usize,
) -> *const Data {
    // TODO(qjs): no snapshot support.
    ptr::null()
}

// ===================================================================
// Context: extras binding object.
//
// V8 exposes an internal "extras binding" object. deno reads `console` off it
// and destructures get/setContinuationPreservedEmbedderData. QuickJS has none,
// so synthesize an object providing these, backed by a closure variable for the
// async-context "snapshot" (correct single-threaded get/set semantics — all
// deno needs for AsyncContext bookkeeping).
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetExtrasBindingObject(this: *const Context) -> *const Object {
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
        // Drop the exception and fall back to a bare object carrying `console`.
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        let o = unsafe { JS_NewObject(ctx) };
        let console = unsafe { JS_NewObject(ctx) };
        unsafe {
            // JS_SetPropertyStr consumes the `console` refcount.
            JS_SetPropertyStr(ctx, o, c"console".as_ptr(), console);
        }
        return intern::<Object>(o);
    }
    // `obj` is owned (+1); move it into an arena slot.
    intern::<Object>(obj)
}

// ===================================================================
// Per-context embedder data.
//
// V8 exposes a single growable array of slots holding either aligned pointers
// or Value handles, indexed identically by both accessor flavours. We back it
// with a side table keyed by the context pointer.
// ===================================================================

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
    // Key by the *decoded* `*mut JSContext`, which is stable across the many
    // distinct Context handles (arena slots) that encode the same underlying
    // context. Keying by the handle pointer would miss, because deno sets data
    // on one handle and reads it back via a freshly-created `GetCurrentContext`
    // handle.
    let key = super::shim_core::ctx_of(this) as usize;
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
pub extern "C" fn v8__Context__GetNumberOfEmbedderDataFields(this: *const Context) -> u32 {
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

// ===================================================================
// Context: promise hooks (no per-context promise lifecycle hooks in QuickJS).
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__SetPromiseHooks(
    _this: *const Context,
    _init_hook: *const Function,
    _before_hook: *const Function,
    _after_hook: *const Function,
    _resolve_hook: *const Function,
) {
    // TODO(qjs): QuickJS exposes no per-context promise lifecycle hooks. Inert.
}

// ===================================================================
// Context: continuation-preserved embedder data (async-context storage).
//
// Note the V8 C-ABI routes these through *mut RealIsolate, not *const Context.
// We store a single per-thread JSValue handle (deno's AsyncContext snapshot).
// ===================================================================

thread_local! {
    // Owns one refcount on the stored JSValue (or `undefined`). `jsv_undefined`
    // is not a const fn, so this initializer can't be `const`.
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
        // Take our own refcount so the value outlives the caller's handle scope.
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
    // Async-context storage. Return the stored value (or `undefined`) — NEVER
    // null: the vendored wrapper unwraps this, so a null handle would panic deno
    // on every unhandled promise rejection. intern_dup keeps our stored
    // refcount intact while handing the scope its own.
    let stored = CONTINUATION_DATA.with(|c| c.get());
    let ctx = current_ctx();
    if ctx.is_null() {
        return ptr::null();
    }
    intern_dup::<Value>(ctx, stored)
}

// ===================================================================
// Isolate: termination / interrupts (no imperative terminate in QuickJS's FFI).
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__TerminateExecution(_isolate: *const RealIsolate) {
    // TODO(qjs): QuickJS terminates via an interrupt handler (JS_SetInterruptHandler)
    // returning non-zero, not an imperative terminate. Inert.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__IsExecutionTerminating(_isolate: *const RealIsolate) -> bool {
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__CancelTerminateExecution(_isolate: *const RealIsolate) {
    // TODO(qjs): no termination state to cancel. Inert.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__RequestInterrupt(
    _isolate: *const RealIsolate,
    _callback: crate::isolate::InterruptCallback,
    _data: *mut c_void,
) {
    // TODO(qjs): no cross-thread interrupt request mechanism wired. Inert.
}

// ===================================================================
// Isolate: exceptions / stack traces.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__ThrowException(
    _isolate: *mut RealIsolate,
    exception: *const Value,
) -> *const Value {
    // Set the value as the context's pending exception. QuickJS's JS_Throw
    // transfers one refcount into the runtime's pending-exception slot, so we
    // JS_DupValue first to keep the caller's handle valid. Returns the same
    // value re-interned, mirroring V8 (the caller passes it straight through as
    // the callback's return value).
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
    // TODO(qjs): QuickJS attaches a `.stack` to Error objects by default; no toggle.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetPrepareStackTraceCallback(
    _isolate: *mut RealIsolate,
    _callback: crate::isolate::PrepareStackTraceCallback<'static>,
) {
    // TODO(qjs): no Error.prepareStackTrace hook in the QuickJS API. Inert.
}

// ===================================================================
// Isolate: heap statistics.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetHeapStatistics(
    _this: *mut RealIsolate,
    s: *mut crate::binding::v8__HeapStatistics,
) {
    // TODO(qjs): QuickJS exposes aggregate memory usage via JS_ComputeMemoryUsage
    // but not V8's granular per-space statistics. Zero the whole struct so
    // callers read 0s rather than garbage.
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
    // TODO(qjs): no near-heap-limit callbacks in QuickJS. Inert.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetIdle(_isolate: *mut RealIsolate, _is_idle: bool) {
    // TODO(qjs): no idle notification in the QuickJS API. Inert.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__HasPendingBackgroundTasks(isolate: *const RealIsolate) -> bool {
    // QuickJS has no background threads, but it does have a pending-job queue
    // (resolved-promise continuations etc). Report whether any job is pending so
    // deno's event loop keeps spinning until they drain.
    if isolate.is_null() {
        return false;
    }
    let st = iso_state(isolate as *mut RealIsolate);
    if st.rt.is_null() {
        return false;
    }
    unsafe { JS_IsJobPending(st.rt) }
}

// ===================================================================
// Isolate: microtasks.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetMicrotasksPolicy(
    _isolate: *mut RealIsolate,
    _policy: MicrotasksPolicy,
) {
    // TODO(qjs): QuickJS drains its job queue only when the embedder calls
    // JS_ExecutePendingJob; the auto/explicit/scoped distinction isn't
    // configurable here. We always behave "explicit" (deno drives the
    // checkpoints). Inert.
}

/// Drain QuickJS's pending-job queue to completion. This runs promise
/// continuations and any other enqueued jobs; mirrors V8's microtask checkpoint.
fn drain_jobs(rt: *mut JSRuntime) {
    if rt.is_null() {
        return;
    }
    unsafe {
        let mut pctx: *mut JSContext = ptr::null_mut();
        // JS_ExecutePendingJob: >0 ran a job, 0 queue empty, <0 job threw.
        loop {
            let r = JS_ExecutePendingJob(rt, &mut pctx);
            if r == 0 {
                break;
            }
            if r < 0 {
                // A job threw; clear its pending exception so the loop can
                // continue draining the rest of the queue.
                if !pctx.is_null() {
                    let exc = JS_GetException(pctx);
                    JS_FreeValue(pctx, exc);
                }
            }
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__PerformMicrotaskCheckpoint(isolate: *mut RealIsolate) {
    if isolate.is_null() {
        return;
    }
    let st = iso_state(isolate);
    drain_jobs(st.rt);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__EnqueueMicrotask(
    _isolate: *mut RealIsolate,
    function: *const Function,
) {
    // TODO(qjs): QuickJS has no public API to push a host job into the queue. As
    // a best effort, invoke the function synchronously so it is not dropped.
    let ctx = current_ctx();
    if ctx.is_null() || function.is_null() {
        return;
    }
    let f = jsval_of(function);
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

// ===================================================================
// Isolate: host / module / wasm callbacks (no QuickJS equivalent — inert).
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetHostInitializeImportMetaObjectCallback(
    _isolate: *mut RealIsolate,
    _callback: crate::isolate::HostInitializeImportMetaObjectCallback,
) {
    // TODO(qjs): import.meta population is handled by the module family's loader
    // (module.rs); no per-isolate hook stored here. Inert.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetHostImportModuleDynamicallyCallback(
    _isolate: *mut RealIsolate,
    _callback: crate::isolate::RawHostImportModuleDynamicallyCallback,
) {
    // TODO(qjs): dynamic import() hook handled by the module family. Inert here.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetHostImportModuleWithPhaseDynamicallyCallback(
    _isolate: *mut RealIsolate,
    _callback: crate::isolate::RawHostImportModuleWithPhaseDynamicallyCallback,
) {
    // TODO(qjs): source-phase dynamic import hook unsupported. Inert.
}

#[cfg(not(target_os = "windows"))]
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetHostCreateShadowRealmContextCallback(
    _isolate: *mut RealIsolate,
    _callback: unsafe extern "C" fn(initiator_context: crate::Local<Context>) -> *mut Context,
) {
    // TODO(qjs): ShadowRealm context creation hook unsupported. Inert.
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
    // TODO(qjs): ShadowRealm context creation hook unsupported. Inert.
}

// ---- Promise reject callback: real bridge via QuickJS's rejection tracker. ----

thread_local! {
    // deno's PromiseRejectCallback (C-ABI). Stored so the QuickJS host rejection
    // tracker trampoline can forward to it.
    static PROMISE_REJECT_CB: std::cell::Cell<
        Option<crate::isolate::PromiseRejectCallback>,
    > = const { std::cell::Cell::new(None) };
}

/// QuickJS host promise-rejection tracker. Fires with `(promise, reason,
/// is_handled)`. We forward unhandled rejections (is_handled == 0) to deno's
/// `PromiseRejectCallback` as a `PromiseRejectMessage` ([promise, value, event]
/// = [usize; 3]). The handles are interned into the current scope so they stay
/// rooted for the duration of the callback.
unsafe extern "C" fn promise_rejection_tracker(
    ctx: *mut JSContext,
    promise: JSValue,
    reason: JSValue,
    is_handled: std::os::raw::c_int,
    _opaque: *mut c_void,
) {
    let cb = PROMISE_REJECT_CB.with(|c| c.get());
    let Some(cb) = cb else { return };
    // Intern borrowed handles (the tracker does NOT transfer ownership).
    let promise_h = intern_dup::<crate::Promise>(ctx, promise);
    let reason_h = intern_dup::<Value>(ctx, reason);
    // event: 0 == PromiseRejectWithNoHandler, 1 == PromiseHandlerAddedAfterReject.
    let event: usize = if is_handled != 0 { 1 } else { 0 };
    let msg: [usize; 3] = [promise_h as usize, reason_h as usize, event];
    unsafe {
        cb(std::mem::transmute::<[usize; 3], crate::promise::PromiseRejectMessage>(msg));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetPromiseRejectCallback(
    isolate: *mut RealIsolate,
    callback: crate::isolate::PromiseRejectCallback,
) {
    PROMISE_REJECT_CB.with(|c| c.set(Some(callback)));
    let iso = if isolate.is_null() { current_iso() } else { isolate };
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
    // TODO(qjs): no wasm async resolve hook (no wasm). Inert.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetWasmStreamingCallback(
    _isolate: *mut RealIsolate,
    _callback: unsafe extern "C" fn(*const crate::function::FunctionCallbackInfo),
) {
    // TODO(qjs): no wasm streaming compilation hook (no wasm). Inert.
}

// ===================================================================
// Isolate: entered / microtask context.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetEnteredOrMicrotaskContext(
    isolate: *mut RealIsolate,
) -> *const Context {
    if isolate.is_null() {
        return ptr::null();
    }
    let st = iso_state(isolate);
    let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
    super::shim_core::intern_ctx(ctx)
}

// ===================================================================
// MicrotaskQueue — QuickJS owns the real job queue; this is an opaque handle.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__MicrotaskQueue__New(
    _isolate: *mut RealIsolate,
    _policy: MicrotasksPolicy,
) -> *mut MicrotaskQueue {
    // Allocate an opaque, non-null handle. QuickJS itself owns the real job
    // queue; this just satisfies the embedder's ownership model.
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
    // Drain the runtime's job queue (the queue handle is opaque; QuickJS has a
    // single per-runtime queue).
    if isolate.is_null() {
        return;
    }
    let st = iso_state(isolate);
    drain_jobs(st.rt);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__MicrotaskQueue__IsRunningMicrotasks(_queue: *const MicrotaskQueue) -> bool {
    // TODO(qjs): no introspection of QuickJS's running-jobs state.
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__MicrotaskQueue__GetMicrotasksScopeDepth(_queue: *const MicrotaskQueue) -> int {
    // TODO(qjs): no microtask scope depth in QuickJS.
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__MicrotaskQueue__EnqueueMicrotask(
    _isolate: *mut RealIsolate,
    _queue: *const MicrotaskQueue,
    microtask: *const Function,
) {
    // Best effort: invoke synchronously so the callback is not silently lost.
    // TODO(qjs): no API to push a host job into QuickJS's job queue directly.
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

// ===================================================================
// ResourceConstraints — plain getters/setters over the #[repr(C)] struct.
//
// Field layout (from isolate_create_params.rs::raw::ResourceConstraints):
//   0: code_range_size_: usize
//   1: max_old_generation_size_: usize
//   2: max_young_generation_size_: usize
//   3: initial_old_generation_size_: usize
//   4: initial_young_generation_size_: usize
//   5: physical_memory_size_: u64
//   6: stack_limit_: *mut u32
// We access via raw word offsets since the fields are private to that module.
// ===================================================================

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
    // TODO(qjs): QuickJS sizes its own heap; just zero so defaults are benign.
    if !constraints.is_null() {
        unsafe { ptr::write_bytes(constraints as *mut u8, 0, std::mem::size_of::<RC>()) };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__ConfigureDefaultsFromHeapSize(
    constraints: *mut RC,
    _initial_heap_size_in_bytes: usize,
    _maximum_heap_size_in_bytes: usize,
) {
    // TODO(qjs): not applied to QuickJS; zero for benign defaults.
    if !constraints.is_null() {
        unsafe { ptr::write_bytes(constraints as *mut u8, 0, std::mem::size_of::<RC>()) };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__code_range_size_in_bytes(constraints: *const RC) -> usize {
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
pub extern "C" fn v8__ResourceConstraints__stack_limit(constraints: *const RC) -> *mut u32 {
    if constraints.is_null() {
        return ptr::null_mut();
    }
    // stack_limit_ is at word index 6 (after the u64 at index 5).
    unsafe { *((constraints as *const usize).add(6) as *const *mut u32) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__set_stack_limit(constraints: *mut RC, value: *mut u32) {
    if !constraints.is_null() {
        unsafe { *((constraints as *mut usize).add(6) as *mut *mut u32) = value };
    }
}

// ===================================================================
// AllowJavascriptExecutionScope — inert construct/destruct.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__AllowJavascriptExecutionScope__CONSTRUCT(
    _buf: *mut std::ffi::c_void,
    _isolate: *mut RealIsolate,
) {
    // TODO(qjs): QuickJS has no "disallow JS execution" state to override. Inert.
    // The buffer's contents are never read by QuickJS, so leave it as-is.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__AllowJavascriptExecutionScope__DESTRUCT(_this: *mut std::ffi::c_void) {
    // Inert; nothing to tear down.
}
