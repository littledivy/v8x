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
    ctx_of, current_ctx, current_iso, intern, intern_dup, iso_state, jsval_of,
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

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetCppHeap(_isolate: *mut RealIsolate) -> *mut c_void {
    // deno wraps native-backed objects (CryptoKey, etc.) through the CppHeap.
    // Return the dummy thread heap so the wrapping path doesn't see null.
    current_cpp_heap()
}

/// Allocate a cppgc-managed object of `8 + additional_bytes` bytes (the 8 leading
/// bytes mirror real V8's vtable slot, which our shim never invokes), aligned to
/// `align`. Without real cppgc we use the system allocator and leak the object —
/// there is no GC to sweep it, which is acceptable for running scripts.
#[unsafe(no_mangle)]
pub extern "C" fn cppgc__make_garbage_collectable(
    _heap: *mut c_void,
    additional_bytes: usize,
    align: usize,
) -> *mut c_void {
    const RUST_OBJ_SIZE: usize = 8;
    let size = RUST_OBJ_SIZE + additional_bytes;
    let align = align.max(8);
    let Ok(layout) = std::alloc::Layout::from_size_align(size, align) else {
        return ptr::null_mut();
    };
    unsafe { std::alloc::alloc_zeroed(layout) as *mut c_void }
}

// cppgc Persistent<T> — a strong cross-scope reference. We model it like a
// Member: a single pointer slot holding the raw object pointer (no GC, so no
// tracing needed). Layout: one pointer.
#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Persistent__CONSTRUCT(
    persistent: *mut *mut c_void,
    obj: *mut c_void,
) {
    if !persistent.is_null() {
        unsafe { *persistent = obj };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Persistent__DESTRUCT(persistent: *mut *mut c_void) {
    if !persistent.is_null() {
        unsafe { *persistent = ptr::null_mut() };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Persistent__Get(persistent: *const *mut c_void) -> *mut c_void {
    if persistent.is_null() {
        return ptr::null_mut();
    }
    unsafe { *persistent }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Visitor__Trace__TracedReference(
    _visitor: *mut c_void,
    _ref_: *const c_void,
) {
    // No tracing GC; nothing to trace.
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
    // Template handles are raw box pointers, not JSValues; a Global of a
    // template must preserve pointer identity (and never be refcounted/freed as
    // a value). Hand it back unchanged — `Global::Reset` no-ops on it below.
    if super::shim_core::is_non_value_handle(data) {
        return data;
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
    // Template handles were handed back by identity in `New`; there is no cell
    // to reclaim and the template box is owned for the whole run.
    if super::shim_core::is_non_value_handle(data) {
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
// TracedReference — a GC-traced handle. We model it exactly like our Global: a
// standalone heap cell (`Box<JSValue>`) owning one independent refcount, freed
// on DESTRUCT/Reset. The vendored buffer is a single usize slot holding the
// cell pointer (0 == empty).
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__TracedReference__CONSTRUCT(this: *mut usize) {
    if !this.is_null() {
        // `TracedReference<T>` storage is `[u8; SIZE]` (align 1) and may be embedded
        // at a misaligned address, so the inline payload must use unaligned access.
        unsafe { this.write_unaligned(0) };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TracedReference__DESTRUCT(this: *mut usize) {
    if this.is_null() {
        return;
    }
    unsafe {
        let cell = this.read_unaligned() as *mut JSValue;
        if !cell.is_null() {
            let v = *cell;
            let ctx = stable_ctx();
            if !ctx.is_null() {
                JS_FreeValue(ctx, v);
            }
            drop(Box::from_raw(cell));
            this.write_unaligned(0);
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TracedReference__Reset(
    this: *mut usize,
    _isolate: *mut RealIsolate,
    data: *const Data,
) {
    if this.is_null() {
        return;
    }
    let ctx = stable_ctx();
    unsafe {
        // Release any previously held cell.
        let old = this.read_unaligned() as *mut JSValue;
        if !old.is_null() {
            if !ctx.is_null() {
                JS_FreeValue(ctx, *old);
            }
            drop(Box::from_raw(old));
            this.write_unaligned(0);
        }
        if data.is_null() || ctx.is_null() {
            return;
        }
        // Template handles aren't JSValues and have no refcount; TracedReference
        // is only ever used for ordinary values in deno, so we don't store them
        // (leaving the slot empty is safe — Get returns null).
        if super::shim_core::is_non_value_handle(data) {
            return;
        }
        let dup = JS_DupValue(ctx, jsval_of(data));
        if std::env::var_os("QJS_DEBUG_TR").is_some() {
            eprintln!("[QJS TracedRef::Reset] store tag={} ptr={:?}", dup.tag, dup.u.ptr);
        }
        let cell = Box::into_raw(Box::new(dup));
        this.write_unaligned(cell as usize);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TracedReference__Get(
    this: *const usize,
    _isolate: *mut RealIsolate,
) -> *const Data {
    if this.is_null() {
        return ptr::null();
    }
    let slot = unsafe { this.read_unaligned() };
    if slot == 0 {
        if std::env::var_os("QJS_DEBUG_TR").is_some() {
            eprintln!("[QJS TracedRef::Get] EMPTY");
        }
        return ptr::null();
    }
    if std::env::var_os("QJS_DEBUG_TR").is_some() {
        let v = unsafe { *(slot as *const JSValue) };
        eprintln!("[QJS TracedRef::Get] tag={} ptr={:?}", v.tag, unsafe { v.u.ptr });
    }
    // The cell is a `Box<JSValue>`; its address is itself a valid v8 Data handle
    // (Local::new reads it via jsval_of, dups into the current scope).
    slot as *const Data
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
    buf: *mut c_void,
    _params: *const c_void,
) {
    // QuickJS can't serialize a snapshot, but deno's snapshot *build* still
    // drives a real isolate through this creator (set default context, run the
    // extension JS, then CreateBlob). So we must own a live, entered isolate:
    // create one and stash its pointer in the creator buffer (`[usize; 1]`).
    let iso = crate::qjs::shim_core::v8__Isolate__New(ptr::null());
    crate::qjs::shim_core::v8__Isolate__Enter(iso);
    if !buf.is_null() {
        unsafe { *(buf as *mut *mut RealIsolate) = iso };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__DESTRUCT(_this: *mut c_void) {
    // The isolate is owned by the `OwnedIsolate` wrapper deno builds around us
    // and is freed via `v8__Isolate__Dispose`; nothing to tear down here.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__GetIsolate(this: *const c_void) -> *mut c_void {
    // Return the isolate created+stashed by CONSTRUCT; fall back to the current
    // one if called on a creator we didn't populate.
    if !this.is_null() {
        let iso = unsafe { *(this as *const *mut RealIsolate) };
        if !iso.is_null() {
            return iso as *mut c_void;
        }
    }
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
    isolate: *mut RealIsolate,
    wire_bytes_data: *const u8,
    length: usize,
) -> *const Object {
    // WASM-ESM integration (`import source wasmMod from "./x.wasm"`): deno calls
    // this to compile the wire bytes into a WasmModuleObject, then the generated
    // wrapper does `new WebAssembly.Instance(wasmMod, imports)`. Compile via the
    // WAMR-backed module store and hand back the same `__wasm_module_id` wrapper
    // `new WebAssembly.Module` produces, which `obj_module_id` recognizes.
    if isolate.is_null() || wire_bytes_data.is_null() {
        return ptr::null();
    }
    let st = iso_state(isolate);
    let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
    if ctx.is_null() {
        return ptr::null();
    }
    let bytes = unsafe { std::slice::from_raw_parts(wire_bytes_data, length) };
    let v = unsafe { super::fam_wasm::compile_module_object(ctx, bytes) };
    if v.tag == JS_TAG_EXCEPTION {
        // Drain the pending exception; deno only checks for a null return.
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return ptr::null();
    }
    intern::<Object>(v)
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

// ===================================================================
// JSON::Stringify — QuickJS JS_JSONStringify(obj, replacer, space).
// ===================================================================

unsafe extern "C" {
    fn JS_JSONStringify(
        ctx: *mut JSContext,
        obj: JSValue,
        replacer: JSValue,
        space0: JSValue,
    ) -> JSValue;
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__JSON__Stringify(
    context: *const Context,
    json_object: *const Value,
) -> *const V8String {
    let ctx = ctx_of(context);
    if ctx.is_null() || json_object.is_null() {
        return ptr::null();
    }
    unsafe {
        // replacer = undefined, space = "" (no indentation), matching v8.
        let s = JS_JSONStringify(
            ctx,
            jsval_of(json_object),
            jsv_undefined(),
            jsv_undefined(),
        );
        if s.tag == JS_TAG_EXCEPTION {
            let exc = JS_GetException(ctx);
            JS_FreeValue(ctx, exc);
            return ptr::null();
        }
        intern::<V8String>(s)
    }
}

// ===================================================================
// Map / Set — built on the JS-visible Map/Set objects via small helpers.
// ===================================================================

/// Run `(function(x){ ... })(obj)` and return the owned (+1) result.
unsafe fn call_unary_closure(ctx: *mut JSContext, src: &[u8], obj: JSValue) -> JSValue {
    unsafe {
        let f = JS_Eval(
            ctx,
            src.as_ptr() as *const c_char,
            src.len() - 1,
            c"<map-set>".as_ptr(),
            JS_EVAL_TYPE_GLOBAL,
        );
        if f.tag == JS_TAG_EXCEPTION {
            let exc = JS_GetException(ctx);
            JS_FreeValue(ctx, exc);
            return jsv_exception();
        }
        let mut args = [JS_DupValue(ctx, obj)];
        let r = JS_Call(ctx, f, jsv_undefined(), 1, args.as_mut_ptr());
        JS_FreeValue(ctx, f);
        JS_FreeValue(ctx, args[0]);
        r
    }
}

fn map_set_size(v: *const Object) -> usize {
    let ctx = current_ctx();
    if ctx.is_null() || v.is_null() {
        return 0;
    }
    unsafe {
        let sz = JS_GetPropertyStr(ctx, jsval_of(v), c"size".as_ptr());
        if sz.tag == JS_TAG_EXCEPTION {
            let exc = JS_GetException(ctx);
            JS_FreeValue(ctx, exc);
            return 0;
        }
        let mut n: i64 = 0;
        let rc = JS_ToInt64(ctx, &mut n, sz);
        JS_FreeValue(ctx, sz);
        if rc < 0 {
            let exc = JS_GetException(ctx);
            JS_FreeValue(ctx, exc);
            return 0;
        }
        n.max(0) as usize
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Size(map: *const crate::Map) -> usize {
    map_set_size(map as *const Object)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__Size(set: *const crate::Set) -> usize {
    map_set_size(set as *const Object)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__As__Array(this: *const crate::Map) -> *const crate::Array {
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return ptr::null();
    }
    // Flatten map entries to [k0,v0,k1,v1,...] (matches V8 Map::AsArray).
    const SRC: &[u8] =
        b"(function(m){var r=[];m.forEach(function(v,k){r.push(k);r.push(v);});return r;})\0";
    let arr = unsafe { call_unary_closure(ctx, SRC, jsval_of(this)) };
    if arr.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return ptr::null();
    }
    intern::<crate::Array>(arr)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__As__Array(this: *const crate::Set) -> *const crate::Array {
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return ptr::null();
    }
    const SRC: &[u8] = b"(function(s){var r=[];s.forEach(function(v){r.push(v);});return r;})\0";
    let arr = unsafe { call_unary_closure(ctx, SRC, jsval_of(this)) };
    if arr.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return ptr::null();
    }
    intern::<crate::Array>(arr)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__New(isolate: *mut RealIsolate) -> *const crate::Set {
    if isolate.is_null() {
        return ptr::null();
    }
    let st = iso_state(isolate);
    let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
    if ctx.is_null() {
        return ptr::null();
    }
    unsafe {
        let global = JS_GetGlobalObject(ctx);
        let ctor = JS_GetPropertyStr(ctx, global, c"Set".as_ptr());
        JS_FreeValue(ctx, global);
        if JS_IsConstructor(ctx, ctor) == 0 {
            JS_FreeValue(ctx, ctor);
            return ptr::null();
        }
        let v = JS_CallConstructor(ctx, ctor, 0, ptr::null_mut());
        JS_FreeValue(ctx, ctor);
        if v.tag == JS_TAG_EXCEPTION {
            let exc = JS_GetException(ctx);
            JS_FreeValue(ctx, exc);
            return ptr::null();
        }
        intern::<crate::Set>(v)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__Add(
    this: *const crate::Set,
    context: *const Context,
    key: *const Value,
) -> *const crate::Set {
    let ctx = ctx_of(context);
    if ctx.is_null() || this.is_null() {
        return ptr::null();
    }
    unsafe {
        let add = JS_GetPropertyStr(ctx, jsval_of(this), c"add".as_ptr());
        if JS_IsFunction(ctx, add) == 0 {
            JS_FreeValue(ctx, add);
            return ptr::null();
        }
        let mut args = [JS_DupValue(ctx, jsval_of(key))];
        let r = JS_Call(ctx, add, jsval_of(this), 1, args.as_mut_ptr());
        JS_FreeValue(ctx, add);
        JS_FreeValue(ctx, args[0]);
        if r.tag == JS_TAG_EXCEPTION {
            let exc = JS_GetException(ctx);
            JS_FreeValue(ctx, exc);
            return ptr::null();
        }
        JS_FreeValue(ctx, r);
        // `Set::Add` returns the set itself; dup it into a fresh slot.
        intern::<crate::Set>(JS_DupValue(ctx, jsval_of(this)))
    }
}

// ===================================================================
// Date
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Date__New(context: *const Context, value: f64) -> *const crate::Date {
    let ctx = ctx_of(context);
    if ctx.is_null() {
        return ptr::null();
    }
    unsafe {
        let global = JS_GetGlobalObject(ctx);
        let ctor = JS_GetPropertyStr(ctx, global, c"Date".as_ptr());
        JS_FreeValue(ctx, global);
        if JS_IsConstructor(ctx, ctor) == 0 {
            JS_FreeValue(ctx, ctor);
            return ptr::null();
        }
        let mut args = [JS_NewFloat64(ctx, value)];
        let v = JS_CallConstructor(ctx, ctor, 1, args.as_mut_ptr());
        JS_FreeValue(ctx, ctor);
        if v.tag == JS_TAG_EXCEPTION {
            let exc = JS_GetException(ctx);
            JS_FreeValue(ctx, exc);
            return ptr::null();
        }
        intern::<crate::Date>(v)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Date__ValueOf(this: *const crate::Date) -> f64 {
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return 0.0;
    }
    unsafe {
        let vo = JS_GetPropertyStr(ctx, jsval_of(this), c"valueOf".as_ptr());
        if JS_IsFunction(ctx, vo) == 0 {
            JS_FreeValue(ctx, vo);
            return 0.0;
        }
        let r = JS_Call(ctx, vo, jsval_of(this), 0, ptr::null_mut());
        JS_FreeValue(ctx, vo);
        if r.tag == JS_TAG_EXCEPTION {
            let exc = JS_GetException(ctx);
            JS_FreeValue(ctx, exc);
            return 0.0;
        }
        let mut n: f64 = 0.0;
        JS_ToFloat64(ctx, &mut n, r);
        JS_FreeValue(ctx, r);
        n
    }
}

// ===================================================================
// Context embedder data / security token.
//
// Embedder data: store a small Vec of owned (+1) JSValues per context, keyed by
// index, in a process-wide map keyed by the JSContext pointer.
// ===================================================================

use std::collections::HashMap;

// JSValues are not Send (raw pointers); isolates are single-threaded here, so
// keep the per-context stores in thread-local maps keyed by the JSContext ptr.
thread_local! {
    static EMBEDDER_DATA: std::cell::RefCell<HashMap<usize, Vec<JSValue>>> =
        std::cell::RefCell::new(HashMap::new());
    static SECURITY_TOKEN: std::cell::RefCell<HashMap<usize, JSValue>> =
        std::cell::RefCell::new(HashMap::new());
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__SetEmbedderData(
    this: *const Context,
    index: std::os::raw::c_int,
    value: *const Value,
) {
    let ctx = ctx_of(this);
    if ctx.is_null() || index < 0 {
        return;
    }
    let owned = unsafe { JS_DupValue(ctx, jsval_of(value)) };
    EMBEDDER_DATA.with(|m| {
        let mut map = m.borrow_mut();
        let slots = map.entry(ctx as usize).or_default();
        let idx = index as usize;
        while slots.len() <= idx {
            slots.push(jsv_undefined());
        }
        let old = slots[idx];
        if old.tag < 0 {
            unsafe { JS_FreeValue(ctx, old) };
        }
        slots[idx] = owned;
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetEmbedderData(
    this: *const Context,
    index: std::os::raw::c_int,
) -> *const Value {
    let ctx = ctx_of(this);
    if ctx.is_null() || index < 0 {
        return ptr::null();
    }
    let found = EMBEDDER_DATA.with(|m| {
        m.borrow()
            .get(&(ctx as usize))
            .and_then(|slots| slots.get(index as usize).copied())
    });
    match found {
        Some(v) => intern_dup::<Value>(ctx, v),
        // No data set: v8 returns an empty/undefined Local.
        None => intern::<Value>(jsv_undefined()),
    }
}

// Security token: QuickJS has a single security domain, so store/return the
// token verbatim (keyed by ctx) without enforcing any check.

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__SetSecurityToken(this: *const Context, value: *const Value) {
    let ctx = ctx_of(this);
    if ctx.is_null() {
        return;
    }
    let owned = unsafe { JS_DupValue(ctx, jsval_of(value)) };
    SECURITY_TOKEN.with(|m| {
        if let Some(old) = m.borrow_mut().insert(ctx as usize, owned) {
            if old.tag < 0 {
                unsafe { JS_FreeValue(ctx, old) };
            }
        }
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetSecurityToken(this: *const Context) -> *const Value {
    let ctx = ctx_of(this);
    if ctx.is_null() {
        return ptr::null();
    }
    let found = SECURITY_TOKEN.with(|m| m.borrow().get(&(ctx as usize)).copied());
    match found {
        Some(v) => intern_dup::<Value>(ctx, v),
        None => intern::<Value>(jsv_undefined()),
    }
}

// ===================================================================
// Eternal<Data> — V8 eternals never die. We back each with a heap-leaked,
// process-wide protected JSValue, stored inline in the Eternal's pointer-sized
// raw slot (a `*const Data` handle pointing at a Box<JSValue> we never free).
// ===================================================================

// NOTE on alignment: the C++ `v8::Eternal<T>` is mapped to a Rust struct whose
// storage is `data: [u8; v8__Eternal_SIZE]` (see `handle.rs`), which has
// **alignment 1**. Callers embed it inline — e.g. deno_core's webidl sequence
// converter keeps `thread_local! { static NEXT_ETERNAL: v8::Eternal<v8::String> }`
// — so the `Eternal` (and hence the `this` pointer handed to these shims) can sit
// at an arbitrary, pointer-misaligned address. We therefore MUST access the
// pointer-sized payload with `read_unaligned`/`write_unaligned`; a plain `*this`
// is a misaligned-pointer dereference (UB; panics under debug) whenever the
// embedding happens to land on an odd address.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__CONSTRUCT(this: *mut *const Data) {
    if !this.is_null() {
        unsafe { this.write_unaligned(ptr::null()) };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__DESTRUCT(_this: *mut *const Data) {
    // Eternals are never destroyed in V8 semantics; leak intentionally.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__Get(this: *const *const Data, _isolate: *mut RealIsolate) -> *const Data {
    if this.is_null() {
        return ptr::null();
    }
    let stored = unsafe { this.read_unaligned() };
    if stored.is_null() {
        return ptr::null();
    }
    // `stored` points at a process-leaked Box<JSValue>; re-intern a dup into the
    // current scope so the caller gets a scope-managed handle.
    let ctx = current_ctx();
    if ctx.is_null() {
        return stored;
    }
    intern_dup::<Data>(ctx, jsval_of(stored))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__Set(
    this: *mut *const Data,
    _isolate: *mut RealIsolate,
    data: *mut Data,
) {
    if this.is_null() {
        return;
    }
    let ctx = current_ctx();
    if ctx.is_null() || data.is_null() {
        unsafe { this.write_unaligned(ptr::null()) };
        return;
    }
    // Leak an owned (+1) copy so it outlives all handle scopes (eternal).
    let owned = unsafe { JS_DupValue(ctx, jsval_of(data)) };
    let boxed = Box::into_raw(Box::new(owned));
    unsafe { this.write_unaligned(boxed as *const Data) };
}

// ===================================================================
// GC prologue/epilogue/near-heap-limit callbacks — QuickJS exposes no GC
// callback hooks, so these are inert (the callbacks simply never fire).
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__AddGCPrologueCallback(
    _isolate: *mut RealIsolate,
    _callback: crate::isolate::GcCallbackWithData,
    _data: *mut c_void,
    _gc_type_filter: crate::gc::GCType,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__AddGCEpilogueCallback(
    _isolate: *mut RealIsolate,
    _callback: crate::isolate::GcCallbackWithData,
    _data: *mut c_void,
    _gc_type_filter: crate::gc::GCType,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__AddNearHeapLimitCallback(
    _isolate: *mut RealIsolate,
    _callback: crate::isolate::NearHeapLimitCallback,
    _data: *mut c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__AdjustAmountOfExternalAllocatedMemory(
    _isolate: *mut RealIsolate,
    change_in_bytes: i64,
) -> i64 {
    // V8 returns the running total of externally-allocated bytes. We have no GC
    // pressure model; track a process-wide running sum so callers see monotone,
    // self-consistent values.
    use std::sync::atomic::{AtomicI64, Ordering};
    static TOTAL: AtomicI64 = AtomicI64::new(0);
    TOTAL.fetch_add(change_in_bytes, Ordering::SeqCst) + change_in_bytes
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__LowMemoryNotification(isolate: *mut RealIsolate) {
    // Best-effort: run a GC cycle.
    if isolate.is_null() {
        return;
    }
    let st = iso_state(isolate);
    if !st.rt.is_null() {
        unsafe { JS_RunGC(st.rt) };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__NumberOfHeapSpaces(_isolate: *mut RealIsolate) -> usize {
    // QuickJS has no V8-style heap spaces.
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetHeapSpaceStatistics(
    _isolate: *mut RealIsolate,
    space_statistics: *mut crate::binding::v8__HeapSpaceStatistics,
    _index: usize,
) -> bool {
    if !space_statistics.is_null() {
        unsafe {
            ptr::write_bytes(
                space_statistics as *mut u8,
                0,
                std::mem::size_of::<crate::binding::v8__HeapSpaceStatistics>(),
            );
        }
    }
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetHeapCodeAndMetadataStatistics(
    _isolate: *mut RealIsolate,
    code_statistics: *mut crate::binding::v8__HeapCodeStatistics,
) -> bool {
    if !code_statistics.is_null() {
        unsafe {
            ptr::write_bytes(
                code_statistics as *mut u8,
                0,
                std::mem::size_of::<crate::binding::v8__HeapCodeStatistics>(),
            );
        }
    }
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__DateTimeConfigurationChangeNotification(
    _isolate: *mut RealIsolate,
    _time_zone_detection: crate::isolate::TimeZoneDetection,
) {
    // QuickJS reads the host timezone on demand; nothing to invalidate. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetAllowWasmCodeGenerationCallback(
    _isolate: *mut RealIsolate,
    _callback: crate::isolate::AllowWasmCodeGenerationCallback,
) {
    // No Wasm in QuickJS-ng. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__HeapProfiler__TakeHeapSnapshot(
    _isolate: *mut RealIsolate,
    _callback: unsafe extern "C" fn(*mut c_void, *const u8, usize) -> bool,
    _arg: *mut c_void,
) {
    // QuickJS has no heap-snapshot serializer. Emit nothing.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CachedDataVersionTag() -> u32 {
    // A constant tag: any code cache we produce is keyed by it. Stable per build.
    0x5145_4a53 // "QJS\x?" placeholder constant
}
