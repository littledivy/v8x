//! Family "misc" (QuickJS backend): SnapshotCreator / StartupData / CppHeap /
//! cppgc / WeakCallbackInfo / Proxy / JSON / Wasm* / Task / IdleTask / Global.
//!
//! QuickJS has no equivalent for V8 snapshots, cppgc (Oilpan), the WebAssembly
//! C++ internals or the C++ task abstractions, so most of these are safe inert
//! defaults (see the `TODO(qjs)` markers). The pieces QuickJS *can* back are
//! implemented for real:
//!   * `v8__Global__New` / `v8__Global__Reset` — `JS_DupValue` / `JS_FreeValue`
//!     against a stable context, with a process-wide protect refcount so the
//!     value outlives handle scopes and survives GC.
//!   * `v8__JSON__Parse` — `JS_ParseJSON`.
//!   * `v8__SnapshotCreator__GetIsolate` / `v8__WeakCallbackInfo__GetIsolate`
//!     surface the current isolate.
//!
//! Mirrors the C-ABI shape of the JSC backend (`src/shim_misc.rs`) but routes
//! every JSValue through the QuickJS refcount helpers in `shim_core`.
#![allow(non_snake_case, unused)]

use crate::qjs::quickjs_sys::*;
use crate::qjs::shim_core::{
    ctx_of, current_ctx, current_iso, intern, iso_state, jsval_of,
};
use crate::{Context, Data, Object, RealIsolate, String as V8String, Value};

use std::os::raw::{c_char, c_void};
use std::ptr;

// ---- QuickJS C API functions we need that aren't declared in quickjs_sys ----
unsafe extern "C" {
    // `buf` must be NUL-terminated (buf[buf_len] == '\0'); JS_ToCString gives us
    // exactly that. Returns an owned (+1) JSValue, or an exception on error.
    fn JS_ParseJSON(
        ctx: *mut JSContext,
        buf: *const c_char,
        buf_len: usize,
        filename: *const c_char,
    ) -> JSValue;
}

// ===================================================================
// cppgc — process / heap. QuickJS manages its own GC; these are inert.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__initialize_process(_platform: *mut c_void) {
    // TODO(qjs): no cppgc; QuickJS owns its heap. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__shutdown_process() {
    // TODO(qjs): no cppgc. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__heap__enable_detached_garbage_collections_for_testing(
    _heap: *mut c_void,
) {
    // TODO(qjs): no cppgc heap. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__heap__collect_garbage_for_testing(
    _heap: *mut c_void,
    _stack_state: u8,
) {
    // TODO(qjs): cannot drive a cppgc GC; there is no cppgc heap. No-op.
}

// ----- cppgc Member / WeakMember -----
// A `Member<T>`/`WeakMember<T>` is, at the ABI level, a single pointer slot
// holding the managed object pointer. CONSTRUCT writes the pointer, DESTRUCT
// clears it. Without a real cppgc heap we model them as a plain (non-owning)
// pointer cell — enough for Deno's bookkeeping to round-trip.

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Member__CONSTRUCT(member: *mut *mut c_void, obj: *mut c_void) {
    if !member.is_null() {
        unsafe { *member = obj };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Member__DESTRUCT(member: *mut *mut c_void) {
    if !member.is_null() {
        unsafe { *member = ptr::null_mut() };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__WeakMember__CONSTRUCT(member: *mut *mut c_void, obj: *mut c_void) {
    if !member.is_null() {
        unsafe { *member = obj };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__WeakMember__DESTRUCT(member: *mut *mut c_void) {
    if !member.is_null() {
        unsafe { *member = ptr::null_mut() };
    }
}

// ===================================================================
// CppHeap — no cppgc backing.
//
// Deno still expects `Isolate::GetCppHeap()` to return a non-null heap so it can
// wrap native-backed objects via `make_garbage_collected`. We provide a dummy
// thread-wide heap handle. Without real GC these allocations leak; acceptable
// for running scripts (the alternative — panicking — fails the runtime).
// ===================================================================

thread_local! {
    static DUMMY_CPP_HEAP: std::cell::Cell<*mut c_void> =
        const { std::cell::Cell::new(ptr::null_mut()) };
}

/// Return (creating on first use) the thread dummy CppHeap pointer.
fn current_cpp_heap() -> *mut c_void {
    DUMMY_CPP_HEAP.with(|c| {
        let mut h = c.get();
        if h.is_null() {
            // A small leaked allocation serves as a stable, unique handle.
            h = Box::into_raw(Box::new(0u64)) as *mut c_void;
            c.set(h);
        }
        h
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CppHeap__Create(
    _platform: *mut c_void,
    _marking_support: u8,
    _sweeping_support: u8,
) -> *mut c_void {
    current_cpp_heap()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CppHeap__Terminate(_heap: *mut c_void) {
    // TODO(qjs): no cppgc heap. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CppHeap__DELETE(_heap: *mut c_void) {
    // TODO(qjs): no cppgc heap. No-op.
}

// ===================================================================
// Global<T> — a value pinned for the Global's whole life, independent of
// handle scopes.
//
// Contract from the vendored handle.rs:
//   * `v8__Global__New(iso, data)` returns a *storage cell* pointer that the
//     `Global` stores as its `data`. It must stay valid until
//     `v8__Global__Reset(cell)` is called exactly once (Drop / Clone-then-Drop).
//   * `Clone` calls `New` again passing the *previous* cell pointer, so `New`
//     must read its value with `jsval_of` (works for both a handle-scope arena
//     slot and one of our own cells) and create a fresh, independent cell.
//   * The returned cell MUST NOT live in the handle-scope arena (it has to
//     survive scope pops) — so we allocate a standalone `Box<JSValue>` that owns
//     one `JS_DupValue` refcount, and free it in `Reset`.
// ===================================================================

/// Best-effort stable context. Prefers the current context; falls back to the
/// current isolate's context stack / single context.
fn stable_ctx() -> *mut JSContext {
    let ctx = current_ctx();
    if !ctx.is_null() {
        return ctx;
    }
    let iso = current_iso();
    if iso.is_null() {
        return ptr::null_mut();
    }
    let st = iso_state(iso);
    st.contexts.last().copied().unwrap_or(st.ctx)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Global__New(
    _isolate: *mut RealIsolate,
    data: *const Data,
) -> *const Data {
    if data.is_null() {
        return ptr::null();
    }
    let ctx = stable_ctx();
    if ctx.is_null() {
        return ptr::null();
    }
    // Take an independent +1 refcount for the Global and store it in a
    // standalone heap cell (NOT the handle arena, so it outlives handle scopes).
    let v = jsval_of(data);
    let dup = unsafe { JS_DupValue(ctx, v) };
    let cell = Box::into_raw(Box::new(dup));
    cell as *const Data
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Global__NewWeak(
    _isolate: *mut RealIsolate,
    data: *const Data,
    _parameter: *const c_void,
    _callback: unsafe extern "C" fn(*const c_void),
) -> *const Data {
    // QuickJS-ng's C ABI has no weak-handle finalizer callback, so the callback
    // will never fire. We still create a real owning cell (like `New`) so the
    // value stays alive and `Reset` balances. The only divergence from V8 is
    // that the weak callback never runs — acceptable (the value simply lives
    // until the owning isolate is disposed).
    v8__Global__New(_isolate, data)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Global__Reset(data: *const Data) {
    if data.is_null() {
        return;
    }
    // Reclaim the standalone cell created by New/NewWeak: free its one refcount
    // and drop the box. Reset is called exactly once per cell.
    let ctx = stable_ctx();
    unsafe {
        let cell = data as *mut JSValue;
        let v = *cell;
        if !ctx.is_null() {
            JS_FreeValue(ctx, v);
        }
        drop(Box::from_raw(cell));
    }
}

// ===================================================================
// JSON
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__JSON__Parse(
    context: *const Context,
    json_string: *const V8String,
) -> *const Value {
    if context.is_null() || json_string.is_null() {
        return ptr::null();
    }
    let ctx = ctx_of(context);
    unsafe {
        // JS_ParseJSON needs a NUL-terminated buffer; JS_ToCStringLen returns
        // exactly that (and gives us the byte length).
        let mut len: usize = 0;
        let cstr = JS_ToCStringLen(ctx, &mut len, jsval_of(json_string));
        if cstr.is_null() {
            // Pending exception; drop it and report failure.
            let exc = JS_GetException(ctx);
            JS_FreeValue(ctx, exc);
            return ptr::null();
        }
        let fname = c"<json>";
        let parsed = JS_ParseJSON(ctx, cstr, len, fname.as_ptr());
        JS_FreeCString(ctx, cstr);
        if parsed.tag == JS_TAG_EXCEPTION {
            let exc = JS_GetException(ctx);
            JS_FreeValue(ctx, exc);
            return ptr::null();
        }
        // `parsed` is owned (+1); move it into an arena slot.
        intern::<Value>(parsed)
    }
}

// ===================================================================
// Proxy — QuickJS's public C API exposes no Proxy target/handler introspection.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Proxy__GetHandler(_this: *const c_void) -> *const Value {
    // TODO(qjs): QuickJS C API cannot retrieve a Proxy's handler.
    ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Proxy__GetTarget(_this: *const c_void) -> *const Value {
    // TODO(qjs): QuickJS C API cannot retrieve a Proxy's target.
    ptr::null()
}

// ===================================================================
// SnapshotCreator / StartupData — QuickJS has no V8-style snapshotting.
//
// The PR's snapshot.rs approximates snapshots with bytecode caching, but that is
// a higher-level Rust API; at the raw v8 C-ABI level deno's snapshot path is not
// exercised by the QuickJS runtime, so we provide inert defaults that produce an
// empty, invalid blob.
// ===================================================================

// Mirror `snapshot::RawStartupData` exactly: { *const c_char, c_int }.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct RawStartupDataAbi {
    data: *const c_char,
    raw_size: std::os::raw::c_int,
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__CONSTRUCT(
    _buf: *mut c_void,
    _params: *const c_void,
) {
    // TODO(qjs): no snapshot support. No-op (buffer left as-is).
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__DESTRUCT(_this: *mut c_void) {
    // TODO(qjs): no snapshot support. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__GetIsolate(_this: *const c_void) -> *mut c_void {
    // TODO(qjs): no snapshot creator isolate; fall back to the current one.
    current_iso() as *mut c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__CreateBlob(
    _this: *mut c_void,
    _function_code_handling: u32,
) -> RawStartupDataAbi {
    // TODO(qjs): cannot produce a snapshot blob.
    RawStartupDataAbi {
        data: ptr::null(),
        raw_size: 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__SetDefaultContext(
    _this: *mut c_void,
    _context: *const Context,
) {
    // TODO(qjs): no snapshot support. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__AddContext(
    _this: *mut c_void,
    _context: *const Context,
) -> usize {
    // TODO(qjs): no snapshot support.
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__AddData_to_context(
    _this: *mut c_void,
    _context: *const Context,
    _data: *const Data,
) -> usize {
    // TODO(qjs): no snapshot support.
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StartupData__CanBeRehashed(_this: *const c_void) -> bool {
    // TODO(qjs): no snapshot support.
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StartupData__IsValid(_this: *const c_void) -> bool {
    // TODO(qjs): no snapshot support; a snapshot is never valid here.
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StartupData__data__DELETE(_this: *const c_char) {
    // TODO(qjs): we never allocate snapshot data, so nothing to free.
}

// ===================================================================
// Task / IdleTask — opaque C++ task objects; cannot run them in QuickJS land.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Task__Run(_task: *mut c_void) {
    // TODO(qjs): opaque C++ task; cannot invoke. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Task__DELETE(_task: *mut c_void) {
    // TODO(qjs): opaque C++ task; cannot delete. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__IdleTask__Run(_task: *mut c_void, _deadline_in_seconds: f64) {
    // TODO(qjs): opaque C++ idle task; cannot invoke. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__IdleTask__DELETE(_task: *mut c_void) {
    // TODO(qjs): opaque C++ idle task; cannot delete. No-op.
}

// ===================================================================
// WeakCallbackInfo — no real weak callbacks in QuickJS's C ABI.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__WeakCallbackInfo__GetIsolate(_this: *const c_void) -> *mut c_void {
    // TODO(qjs): no real weak callbacks; surface the current isolate.
    current_iso() as *mut c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WeakCallbackInfo__GetParameter(_this: *const c_void) -> *mut c_void {
    // TODO(qjs): no real weak callbacks.
    ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WeakCallbackInfo__SetSecondPassCallback(
    _this: *const c_void,
    _callback: unsafe extern "C" fn(*const c_void),
) {
    // TODO(qjs): no second-pass weak callbacks. No-op.
}

// ===================================================================
// Wasm — QuickJS-ng has no WebAssembly support / compilation internals.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__Unpack(
    _isolate: *mut c_void,
    _value: *const Value,
    _that: *mut c_void,
) {
    // TODO(qjs): no Wasm streaming. Leaves the out param untouched.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__shared_ptr_DESTRUCT(_this: *mut c_void) {
    // TODO(qjs): no Wasm streaming shared_ptr to destruct. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__OnBytesReceived(
    _this: *mut c_void,
    _data: *const u8,
    _len: usize,
) {
    // TODO(qjs): no Wasm streaming. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__Finish(
    _this: *mut c_void,
    _callback: Option<unsafe extern "C" fn(*mut c_void)>,
) {
    // TODO(qjs): no Wasm streaming. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__Abort(_this: *mut c_void, _exception: *const Value) {
    // TODO(qjs): no Wasm streaming. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__SetUrl(
    _this: *mut c_void,
    _url: *const c_char,
    _len: usize,
) {
    // TODO(qjs): no Wasm streaming. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleObject__FromCompiledModule(
    _isolate: *mut c_void,
    _compiled_module: *const c_void,
) -> *const c_void {
    // TODO(qjs): no Wasm module support.
    ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleObject__GetCompiledModule(_this: *const c_void) -> *mut c_void {
    // TODO(qjs): no Wasm module support.
    ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleObject__Compile(
    _isolate: *mut c_void,
    _wire_bytes_data: *const u8,
    _length: usize,
) -> *mut c_void {
    // TODO(qjs): no Wasm compilation support.
    ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CompiledWasmModule__DELETE(_this: *mut c_void) {
    // TODO(qjs): no Wasm module support. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__NEW() -> *mut c_void {
    // TODO(qjs): no Wasm compilation support.
    ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__DELETE(_this: *mut c_void) {
    // TODO(qjs): no Wasm compilation support. No-op.
}
