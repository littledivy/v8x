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

use crate::jsc_sys::*;
use crate::shim_core::{
    ctx_of, current_ctx, current_iso, intern, intern_ctx, iso_state, jsval,
};
use crate::{
    Context, Data, Function, MicrotaskQueue, MicrotasksPolicy, Object, RealIsolate, Value,
};
use std::mem::MaybeUninit;
use std::os::raw::c_void;
use std::ptr;

use crate::support::int;

// Most JSC C API functions (JSObjectIsFunction, JSObjectCallAsFunction,
// JSObjectMakeFunctionWithCallback, ...) come from `crate::jsc_sys` (bindgen).
//
// `JSGlobalContextSetUnhandledRejectionCallback` is WebKit SPI and is NOT in
// the public `<JavaScriptCore/JavaScript.h>` umbrella header, so bindgen does
// not emit it. Declare it by hand. JSC reports unhandled promise rejections by
// invoking `callback(reason, promise)`; this is the bridge we use to drive
// deno's `PromiseRejectCallback`.
unsafe extern "C" {
    fn JSGlobalContextSetUnhandledRejectionCallback(
        ctx: JSGlobalContextRef,
        function: JSObjectRef,
        exception: *mut JSValueRef,
    );
}

// The deno-registered PromiseRejectCallback, plus a flag tracking whether we've
// installed the JSC unhandled-rejection bridge for the current context.
thread_local! {
    static PROMISE_REJECT_CB: std::cell::Cell<
        Option<crate::isolate::PromiseRejectCallback>,
    > = const { std::cell::Cell::new(None) };
}

/// JSC unhandled-rejection trampoline: receives `(reason, promise)` and forwards
/// to deno's PromiseRejectCallback as a `PromiseRejectMessage` ([promise, value,
/// event] = [usize; 3]).
unsafe extern "C" fn unhandled_rejection_trampoline(
    _ctx: JSContextRef,
    _function: JSObjectRef,
    _this: JSObjectRef,
    argc: usize,
    argv: *const JSValueRef,
    _exception: *mut JSValueRef,
) -> JSValueRef {
    if std::env::var("V82JSC_DEBUG").is_ok() {
        eprintln!("[v82jsc] unhandled_rejection_trampoline fired argc={}", argc);
    }
    let cb = PROMISE_REJECT_CB.with(|c| c.get());
    if let Some(cb) = cb {
        let promise = if argc >= 2 { unsafe { *argv.add(1) } } else { ptr::null() };
        let reason = if argc >= 1 { unsafe { *argv } } else { ptr::null() };
        // event 0 == PromiseRejectWithNoHandler. PromiseRejectMessage has the
        // layout `[usize; 3]` (promise, value, event) and is passed by value.
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

// ===================================================================
// Context extras
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__AllowCodeGenerationFromStrings(_this: *const Context, _allow: bool) {
    // TODO(v82jsc): JSC always allows code generation from strings (eval/new
    // Function); there is no toggle in the public C API. Inert no-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context_IsCodeGenerationFromStringsAllowed(_this: *const Context) -> bool {
    // JSC always permits eval / Function constructor.
    true
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__FromSnapshot(
    _isolate: *mut RealIsolate,
    _context_snapshot_index: usize,
    _global_object: *const Value,
    _microtask_queue: *mut MicrotaskQueue,
) -> *const Context {
    // TODO(v82jsc): JSC has no snapshot support; cannot restore a context.
    ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetExtrasBindingObject(this: *const Context) -> *const Object {
    // V8 exposes an internal "extras binding" object. deno reads `console` off
    // it, and destructures `getContinuationPreservedEmbedderData` /
    // `setContinuationPreservedEmbedderData` (the async-context get/set). JSC has
    // none, so synthesize an object that provides these. The continuation data
    // is the async-context "snapshot"; we back it with a per-context closure
    // variable (correct single-threaded get/set semantics, which is all deno
    // needs for AsyncContext bookkeeping).
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
        let js = JSStringCreateWithUTF8CString(SRC.as_ptr() as *const std::os::raw::c_char);
        let mut exc: JSValueRef = ptr::null();
        let obj = JSEvaluateScript(ctx, js, ptr::null_mut(), ptr::null_mut(), 1, &mut exc);
        JSStringRelease(js);
        if obj.is_null() {
            // Fallback: at least provide `console`.
            let o = JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut());
            let console = JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut());
            let name = JSStringCreateWithUTF8CString(c"console".as_ptr());
            JSObjectSetProperty(ctx, o, name, console as JSValueRef, 0, ptr::null_mut());
            JSStringRelease(name);
            return intern_ctx::<Object>(ctx, o as JSValueRef);
        }
        intern_ctx::<Object>(ctx, obj)
    }
}

// Per-context embedder data: V8 exposes a single growable array of slots that
// can hold either aligned pointers or `Value` handles, indexed the same way by
// both the aligned-pointer and Value accessors. We back it with a side table
// keyed by the context pointer.
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
    // TODO(v82jsc): no snapshot support.
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
    // TODO(v82jsc): JSC exposes no per-context promise lifecycle hooks. Inert.
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
    // Async-context storage. Return the stored value, or `undefined` — NEVER
    // null: the vendored wrapper unwraps this, so a null handle panics deno on
    // every unhandled promise rejection.
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

// ===================================================================
// Isolate: termination / interrupts
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__TerminateExecution(_isolate: *const RealIsolate) {
    // TODO(v82jsc): JSC offers JSContextGroup termination via a watchdog
    // callback set at group creation, not an imperative terminate. Inert.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__IsExecutionTerminating(_isolate: *const RealIsolate) -> bool {
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__CancelTerminateExecution(_isolate: *const RealIsolate) {
    // TODO(v82jsc): no termination state to cancel. Inert.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__RequestInterrupt(
    _isolate: *const RealIsolate,
    _callback: crate::isolate::InterruptCallback,
    _data: *mut c_void,
) {
    // TODO(v82jsc): no cross-thread interrupt mechanism in the JSC C API. Inert.
}

// ===================================================================
// Isolate: exceptions / stack traces
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__ThrowException(
    _isolate: *mut RealIsolate,
    exception: *const Value,
) -> *const Value {
    // Record the value as the isolate's pending exception. Our function
    // trampolines (shim_function.rs) read this after a v8 callback returns and
    // forward it to JSC via the callback's `exception` out-param, so a throw
    // from inside a native callback propagates as a real JS exception. A
    // surrounding TryCatch also observes it.
    let ctx = current_ctx();
    if ctx.is_null() || exception.is_null() {
        return exception;
    }
    crate::shim_core::record_pending_exception(ctx, jsval(exception));
    intern_ctx::<Value>(ctx, jsval(exception))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetCaptureStackTraceForUncaughtExceptions(
    _this: *mut RealIsolate,
    _capture: bool,
    _frame_limit: i32,
) {
    // TODO(v82jsc): JSC always captures a stack on Error objects; no toggle.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetPrepareStackTraceCallback(
    _isolate: *mut RealIsolate,
    _callback: crate::isolate::PrepareStackTraceCallback,
) {
    // TODO(v82jsc): no Error.prepareStackTrace hook in the JSC C API. Inert.
}

// ===================================================================
// Isolate: heap statistics
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetHeapStatistics(
    _this: *mut RealIsolate,
    s: *mut crate::binding::v8__HeapStatistics,
) {
    // TODO(v82jsc): JSC does not expose granular heap statistics through its C
    // API. Zero the whole struct (15 fields, 120 bytes) so callers read 0s.
    if !s.is_null() {
        unsafe {
            ptr::write_bytes(s as *mut u8, 0, std::mem::size_of::<crate::binding::v8__HeapStatistics>());
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__RemoveNearHeapLimitCallback(
    _isolate: *mut RealIsolate,
    _callback: crate::isolate::NearHeapLimitCallback,
    _heap_limit: usize,
) {
    // TODO(v82jsc): no near-heap-limit callbacks in JSC. Inert.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetIdle(_isolate: *mut RealIsolate, _is_idle: bool) {
    // TODO(v82jsc): no idle notification in the JSC C API. Inert.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__HasPendingBackgroundTasks(_isolate: *const RealIsolate) -> bool {
    // JSC has no exposed background task queue (e.g. wasm compilation jobs).
    false
}

// ===================================================================
// Isolate: microtasks
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetMicrotasksPolicy(
    _isolate: *mut RealIsolate,
    _policy: MicrotasksPolicy,
) {
    // TODO(v82jsc): JSC drains its job queue automatically after each top-level
    // evaluation; the policy is not configurable. Inert.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__PerformMicrotaskCheckpoint(isolate: *mut RealIsolate) {
    // Drain JSC's microtask/job queue. This also lets JSC surface unhandled
    // promise rejections (firing the unhandled-rejection bridge -> deno's
    // PromiseRejectCallback), which is how module-evaluation throws and other
    // uncaught async errors get reported.
    if isolate.is_null() {
        return;
    }
    let ctx = current_ctx();
    if !ctx.is_null() {
        // Drain JSC's microtask/job queue by making one outermost JS API call: a
        // cached no-op function. JSC runs the job queue to completion when the
        // OUTERMOST API entry scope unwinds, and — crucially — that fresh entry
        // scope gives the drained continuations a proper top-level execution
        // context. (Calling `JSC::VM::drainMicrotasks()` directly drains the queue
        // but WITHOUT an entry boundary, so a continuation that schedules another
        // op — e.g. `await someOp(); setTimeout(...)` — runs in a broken
        // re-entrant state and silently aborts after its first statement.)
        // Reusing a cached function avoids the parse/compile cost of the old
        // `eval("0")` trick while keeping the entry-scope semantics it relied on.
        unsafe {
            let f = checkpoint_noop_fn(ctx);
            if !f.is_null() {
                let mut exc: JSValueRef = ptr::null();
                JSObjectCallAsFunction(ctx, f, ptr::null_mut(), 0, ptr::null(), &mut exc);
            }
        }
    }
}

thread_local! {
    // (global ctx, cached no-op function) — protected for the ctx's lifetime.
    static CHECKPOINT_FN: std::cell::Cell<(JSGlobalContextRef, JSObjectRef)> =
        const { std::cell::Cell::new((ptr::null_mut(), ptr::null_mut())) };
}

/// A cached `function(){}` for `ctx`'s global context, used purely as an
/// outermost API-entry boundary that drains the microtask queue.
unsafe fn checkpoint_noop_fn(ctx: JSContextRef) -> JSObjectRef {
    let gctx = unsafe { JSContextGetGlobalContext(ctx) };
    let cached = CHECKPOINT_FN.with(|c| c.get());
    if cached.0 == gctx && !cached.1.is_null() {
        return cached.1;
    }
    unsafe {
        let name = JSStringCreateWithUTF8CString(c"".as_ptr());
        let f = JSObjectMakeFunctionWithCallback(ctx, name, Some(checkpoint_noop_cb));
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
    // TODO(v82jsc): JSC has no public API to enqueue a microtask directly. As a
    // best effort, invoke the function synchronously so it is not dropped.
    let ctx = current_ctx();
    if ctx.is_null() || function.is_null() {
        return;
    }
    unsafe {
        let f = jsval(function) as JSObjectRef;
        if JSValueIsObject(ctx, jsval(function)) && JSObjectIsFunction(ctx, f) {
            let mut exc: JSValueRef = ptr::null();
            JSObjectCallAsFunction(ctx, f, ptr::null_mut(), 0, ptr::null(), &mut exc);
        }
    }
}

// ===================================================================
// Isolate: host / module / wasm callbacks (all inert — no JSC equivalent)
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetHostInitializeImportMetaObjectCallback(
    isolate: *mut RealIsolate,
    callback: crate::isolate::HostInitializeImportMetaObjectCallback,
) {
    // JSC has no module-loader hook, but our module rewriter synthesizes an
    // `import.meta` object per module and invokes this callback to populate it.
    if !isolate.is_null() {
        crate::shim_core::iso_state(isolate).import_meta_cb = Some(callback);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetHostImportModuleDynamicallyCallback(
    _isolate: *mut RealIsolate,
    _callback: crate::isolate::RawHostImportModuleDynamicallyCallback,
) {
    // TODO(v82jsc): dynamic import() hook not exposed by JSC C API. Inert.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetHostImportModuleWithPhaseDynamicallyCallback(
    _isolate: *mut RealIsolate,
    _callback: crate::isolate::RawHostImportModuleWithPhaseDynamicallyCallback,
) {
    // TODO(v82jsc): source-phase dynamic import hook not exposed by JSC. Inert.
}

#[cfg(not(target_os = "windows"))]
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetHostCreateShadowRealmContextCallback(
    _isolate: *mut RealIsolate,
    _callback: unsafe extern "C" fn(initiator_context: crate::Local<Context>) -> *mut Context,
) {
    // TODO(v82jsc): ShadowRealm context creation hook unsupported. Inert.
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
    // TODO(v82jsc): ShadowRealm context creation hook unsupported. Inert.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetPromiseRejectCallback(
    _isolate: *mut RealIsolate,
    callback: crate::isolate::PromiseRejectCallback,
) {
    // Store deno's callback and install the JSC unhandled-rejection bridge on the
    // isolate's context(s). JSC calls the bridge with `(reason, promise)` when a
    // promise rejects without a handler; the bridge forwards to `callback`.
    PROMISE_REJECT_CB.with(|c| c.set(Some(callback)));
    let iso = if _isolate.is_null() { current_iso() } else { _isolate };
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

/// Install (idempotently) the JSC unhandled-rejection callback on `gctx`.
pub(crate) unsafe fn install_unhandled_rejection_bridge(gctx: JSGlobalContextRef) {
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
    // TODO(v82jsc): no wasm async resolve hook. Inert.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetWasmStreamingCallback(
    _isolate: *mut RealIsolate,
    _callback: unsafe extern "C" fn(*const crate::function::FunctionCallbackInfo),
) {
    // TODO(v82jsc): no wasm streaming compilation hook. Inert.
}

// ===================================================================
// Isolate: entered / microtask context
// ===================================================================

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

// ===================================================================
// CreateParams
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__CreateParams__CONSTRUCT(
    buf: *mut MaybeUninit<crate::isolate_create_params::raw::CreateParams>,
) {
    // Zero-initialize the struct; the high-level Rust wrapper overwrites the
    // fields it cares about. allow_atomics_wait default in V8 is true, but the
    // Rust CreateParams::default() sets it explicitly, so zeroing is safe here.
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

// ===================================================================
// MicrotaskQueue — backed by a tiny heap box; JSC drives the real queue.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__MicrotaskQueue__New(
    _isolate: *mut RealIsolate,
    _policy: MicrotasksPolicy,
) -> *mut MicrotaskQueue {
    // Allocate an opaque, non-null handle. JSC itself owns the real job queue;
    // this just satisfies the embedder's ownership model.
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
    // JSC drains its own job queue automatically. TODO(v82jsc): no manual drain.
    let ctx = current_ctx();
    if !ctx.is_null() {
        unsafe { JSGarbageCollect(ctx) };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__MicrotaskQueue__IsRunningMicrotasks(_queue: *const MicrotaskQueue) -> bool {
    // TODO(v82jsc): no introspection of JSC's running-jobs state.
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__MicrotaskQueue__GetMicrotasksScopeDepth(_queue: *const MicrotaskQueue) -> int {
    // TODO(v82jsc): no microtask scope depth in JSC.
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__MicrotaskQueue__EnqueueMicrotask(
    _isolate: *mut RealIsolate,
    _queue: *const MicrotaskQueue,
    microtask: *const Function,
) {
    // Best effort: invoke synchronously so the callback is not silently lost.
    // TODO(v82jsc): no API to enqueue into JSC's job queue directly.
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

// ===================================================================
// ResourceConstraints — plain getters/setters over the #[repr(C)] struct.
// ===================================================================

type RC = crate::isolate_create_params::raw::ResourceConstraints;

#[unsafe(no_mangle)]
pub extern "C" fn v8__ResourceConstraints__ConfigureDefaults(
    constraints: *mut RC,
    _physical_memory: u64,
    _virtual_memory_limit: u64,
) {
    // TODO(v82jsc): JSC sizes its own heap; just zero so defaults are benign.
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
    // TODO(v82jsc): not applied to JSC; zero for benign defaults.
    if !constraints.is_null() {
        unsafe { ptr::write_bytes(constraints as *mut u8, 0, std::mem::size_of::<RC>()) };
    }
}

// The struct field layout (from isolate_create_params.rs):
//   0: code_range_size_: usize
//   1: max_old_generation_size_: usize
//   2: max_young_generation_size_: usize
//   3: initial_old_generation_size_: usize
//   4: initial_young_generation_size_: usize
//   5: physical_memory_size_: u64
//   6: stack_limit_: *mut u32
// We access via raw usize offsets since the fields are private to that module.
#[inline(always)]
unsafe fn rc_word(c: *const RC, idx: usize) -> usize {
    unsafe { *(c as *const usize).add(idx) }
}
#[inline(always)]
unsafe fn rc_set_word(c: *mut RC, idx: usize, v: usize) {
    unsafe { *(c as *mut usize).add(idx) = v };
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
    // TODO(v82jsc): JSC has no "disallow JS execution" state to override. Inert.
    // The buffer's contents are never read by JSC, so leave it as-is.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__AllowJavascriptExecutionScope__DESTRUCT(
    _this: *mut std::ffi::c_void,
) {
    // Inert; nothing to tear down.
}

// ===================================================================
// Isolate — GC / heap / wasm / memory hooks. JSC has no public API for these,
// so they are safe inert defaults.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__AddGCPrologueCallback(
    _isolate: *mut RealIsolate,
    _callback: *const c_void,
    _data: *mut c_void,
    _gc_type_filter: i32,
) {
    // No-op: JSC fires no V8-style GC prologue callbacks.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__AddGCEpilogueCallback(
    _isolate: *mut RealIsolate,
    _callback: *const c_void,
    _data: *mut c_void,
    _gc_type_filter: i32,
) {
    // No-op: JSC fires no V8-style GC epilogue callbacks.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__AdjustAmountOfExternalAllocatedMemory(
    _isolate: *mut RealIsolate,
    _change_in_bytes: i64,
) -> i64 {
    // We don't track external memory pressure; report zero total.
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__DateTimeConfigurationChangeNotification(
    _isolate: *mut RealIsolate,
    _time_zone_detection: i32,
) {
    // No-op: JSC re-reads the tz environment on demand.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__LowMemoryNotification(_isolate: *mut RealIsolate) {
    // No-op: no hook to force a JSC collection from the C API.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__NumberOfHeapSpaces(_isolate: *mut RealIsolate) -> usize {
    // We expose no enumerable heap spaces.
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetHeapSpaceStatistics(
    _isolate: *mut RealIsolate,
    space_statistics: *mut c_void,
    _index: usize,
) -> bool {
    // Zero the out struct (v8::HeapSpaceStatistics is 40 bytes on this target).
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
    // Zero the out struct (v8::HeapCodeStatistics is 32 bytes on this target).
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
    // No-op: wasm code generation is unconditionally permitted under JSC.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__HeapProfiler__TakeHeapSnapshot(
    _isolate: *mut RealIsolate,
    _callback: *const c_void,
    _arg: *mut c_void,
) {
    // No-op: no heap-snapshot serializer under JSC.
}

// ===================================================================
// Context security token — V8 cross-context access guard. JSC has a single
// security domain, so the token is informational: store/return a value.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetSecurityToken(_this: *const Context) -> *const Value {
    // No per-context token tracked; report none (undefined-equivalent null).
    ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__SetSecurityToken(
    _this: *const Context,
    _value: *const Value,
) {
    // No-op: single security domain.
}
