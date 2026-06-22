//! Family "misc": SnapshotCreator / StartupData / CppHeap / cppgc /
//! WeakCallbackInfo / Proxy / JSON / Wasm* / Task / IdleTask / Global /
//! shared_ptr<Platform>.
//!
//! JSC has no equivalent for V8 snapshots, cppgc, the WebAssembly C++ internals
//! or the C++ task abstractions, so most of these are safe inert defaults
//! (see the `TODO(v82jsc)` markers). Global handles, JSON parsing and the
//! shared_ptr<Platform> machinery are implemented for real.
#![allow(non_snake_case, unused)]

use crate::jsc_sys::*;
use crate::shim_core::{ctx_of, current_ctx, current_iso, intern, intern_ctx, iso_state, jsval};
use crate::support::{SharedPtrBase, UniquePtr, long};
use crate::{Context, Data, Object, String as V8String, Value};

use std::os::raw::{c_char, c_void};
use std::ptr;

// ---- Extra JSC C API functions we need (not declared in jsc_sys.rs) ----
unsafe extern "C" {
    fn JSValueMakeFromJSONString(ctx: JSContextRef, string: JSStringRef) -> JSValueRef;
    fn JSValueCreateJSONString(
        ctx: JSContextRef,
        value: JSValueRef,
        indent: u32,
        exception: *mut JSValueRef,
    ) -> JSStringRef;
}

// `crate::Platform` is module-private to us; for these C-ABI symbols
// we only need pointer/layout compatibility, so use an opaque marker. The
// `SharedPtrBase<T>` layout is `[usize; 2]` regardless of `T`.
type PlatformOpaque = c_void;

// ===================================================================
// cppgc — process / heap. JSC manages its own GC; these are inert.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__initialize_process(_platform: *mut c_void) {
    // TODO(v82jsc): no cppgc; JSC owns its heap. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__shutdown_process() {
    // TODO(v82jsc): no cppgc. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__heap__enable_detached_garbage_collections_for_testing(
    _heap: *mut c_void,
) {
    // TODO(v82jsc): no cppgc heap. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__heap__collect_garbage_for_testing(
    _heap: *mut c_void,
    _stack_state: u8,
) {
    // TODO(v82jsc): cannot drive cppgc GC. We could JSGarbageCollect the current
    // context, but there is no cppgc heap here, so this is a no-op.
}

// ----- cppgc Member / WeakMember -----
// A `Member<T>`/`WeakMember<T>` is, at the ABI level, a single pointer slot
// holding the managed object pointer. CONSTRUCT writes the pointer, Get reads
// it, Assign overwrites it, DESTRUCT clears it. Without a real cppgc heap we
// model them as a plain (non-owning) pointer cell — enough for Deno's
// bookkeeping to round-trip.

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
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__CppHeap__Create(
    _platform: *mut c_void,
    _marking_support: u8,
    _sweeping_support: u8,
) -> *mut c_void {
    // TODO(v82jsc): no cppgc heap available.
    ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CppHeap__Terminate(_heap: *mut c_void) {
    // TODO(v82jsc): no cppgc heap. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CppHeap__DELETE(_heap: *mut c_void) {
    // TODO(v82jsc): no cppgc heap. No-op.
}

// ===================================================================
// Global<T> — protect / unprotect a JS value so it outlives handle scopes.
//
// Globals must protect a value for the global's whole life and unprotect on
// Reset/drop, independent of handle scopes *and* of whichever context happens
// to be current at the time. Earlier code protected/unprotected against
// `current_ctx()`, which is unstable: a Global cloned while no context is
// entered would skip the protect, yet its Reset (run with a context current)
// would still unprotect — driving the JSC protect count negative and freeing a
// still-referenced cell (heap corruption surfacing during GC).
//
// To make this robust we keep a process-wide refcount per JSValueRef, capturing
// a stable context the first time a value is protected and reusing it for every
// protect/unprotect of that value. (JSC protection is per-VM; any live context
// of the value's group is equivalent, so a captured stable context is correct.)
// ===================================================================

thread_local! {
    static GLOBAL_PROTECT: std::cell::RefCell<
        std::collections::HashMap<usize, (JSContextRef, usize)>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Best-effort stable context for protecting `v`. Prefers the current context;
/// falls back to any live context of the current isolate.
fn stable_protect_ctx() -> JSContextRef {
    let ctx = current_ctx();
    if !ctx.is_null() {
        return ctx;
    }
    let iso = current_iso();
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

fn global_protect(v: JSValueRef) {
    if v.is_null() {
        return;
    }
    GLOBAL_PROTECT.with(|m| {
        let mut map = m.borrow_mut();
        match map.get_mut(&(v as usize)) {
            Some((ctx, count)) => {
                // Already protected once; bump the refcount and re-protect so the
                // JSC protect count matches our refcount exactly.
                unsafe { JSValueProtect(*ctx, v) };
                *count += 1;
            }
            None => {
                let ctx = stable_protect_ctx();
                if ctx.is_null() {
                    return;
                }
                unsafe { JSValueProtect(ctx, v) };
                map.insert(v as usize, (ctx, 1));
            }
        }
    });
}

fn global_unprotect(v: JSValueRef) {
    if v.is_null() {
        return;
    }
    GLOBAL_PROTECT.with(|m| {
        let mut map = m.borrow_mut();
        if let Some((ctx, count)) = map.get_mut(&(v as usize)) {
            let ctx = *ctx;
            unsafe { JSValueUnprotect(ctx, v) };
            *count -= 1;
            if *count == 0 {
                map.remove(&(v as usize));
            }
        }
        // If we never recorded a protect for this value (e.g. it was created
        // with no context available), do nothing — never unprotect blindly.
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Global__New(
    _isolate: *mut c_void,
    data: *const Data,
) -> *const Data {
    if data.is_null() {
        return ptr::null();
    }
    global_protect(jsval(data));
    // The Global keeps its own protection independent of the handle scope, so
    // return the same pointer (it *is* the JSValueRef) without scope-recording.
    data
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Global__NewWeak(
    _isolate: *mut c_void,
    data: *const Data,
    _parameter: *const c_void,
    _callback: unsafe extern "C" fn(*const c_void),
) -> *const Data {
    // TODO(v82jsc): JSC has no weak-handle finalizer callback in the C API.
    // Model a weak global as a plain (non-protected) reference; the finalizer
    // callback will never fire.
    data
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Global__Reset(data: *const Data) {
    if data.is_null() {
        return;
    }
    global_unprotect(jsval(data));
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
    let ctx = ctx_of(context) as JSContextRef;
    unsafe {
        // Turn the JS string value into a JSStringRef, then parse.
        let mut exc: JSValueRef = ptr::null();
        let s = JSValueToStringCopy(ctx, jsval(json_string), &mut exc);
        if s.is_null() {
            return ptr::null();
        }
        let parsed = JSValueMakeFromJSONString(ctx, s);
        JSStringRelease(s);
        if parsed.is_null() {
            return ptr::null();
        }
        intern_ctx::<Value>(ctx, parsed)
    }
}

// ===================================================================
// Proxy — JSC's C API exposes no Proxy target/handler introspection.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Proxy__GetHandler(_this: *const c_void) -> *const Value {
    // TODO(v82jsc): JSC C API cannot retrieve a Proxy's handler.
    ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Proxy__GetTarget(_this: *const c_void) -> *const Value {
    // TODO(v82jsc): JSC C API cannot retrieve a Proxy's target.
    ptr::null()
}

// ===================================================================
// SnapshotCreator / StartupData — JSC has no snapshotting.
// ===================================================================

// Mirror `snapshot::RawStartupData` exactly: { *const c_char, c_int }.
#[repr(C)]
#[derive(Clone, Copy)]
struct RawStartupDataAbi {
    data: *const c_char,
    raw_size: std::os::raw::c_int,
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__CONSTRUCT(
    _buf: *mut c_void,
    _params: *const c_void,
) {
    // TODO(v82jsc): no snapshot support. No-op (buffer left as-is).
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__DESTRUCT(_this: *mut c_void) {
    // TODO(v82jsc): no snapshot support. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__GetIsolate(_this: *const c_void) -> *mut c_void {
    // TODO(v82jsc): no snapshot creator isolate; fall back to the current one.
    current_iso() as *mut c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__CreateBlob(
    _this: *mut c_void,
    _function_code_handling: u32,
) -> RawStartupDataAbi {
    // TODO(v82jsc): cannot produce a snapshot blob.
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
    // TODO(v82jsc): no snapshot support. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__AddContext(
    _this: *mut c_void,
    _context: *const Context,
) -> usize {
    // TODO(v82jsc): no snapshot support.
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__AddData_to_context(
    _this: *mut c_void,
    _context: *const Context,
    _data: *const Data,
) -> usize {
    // TODO(v82jsc): no snapshot support.
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StartupData__CanBeRehashed(_this: *const c_void) -> bool {
    // TODO(v82jsc): no snapshot support.
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StartupData__IsValid(_this: *const c_void) -> bool {
    // TODO(v82jsc): no snapshot support; a snapshot is never valid here.
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StartupData__data__DELETE(_this: *const c_char) {
    // TODO(v82jsc): we never allocate snapshot data, so nothing to free.
}

// ===================================================================
// Task / IdleTask — opaque C++ task objects, cannot run them in JSC land.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Task__Run(_task: *mut c_void) {
    // TODO(v82jsc): opaque C++ task; cannot invoke. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Task__DELETE(_task: *mut c_void) {
    // TODO(v82jsc): opaque C++ task; cannot delete. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__IdleTask__Run(_task: *mut c_void, _deadline_in_seconds: f64) {
    // TODO(v82jsc): opaque C++ idle task; cannot invoke. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__IdleTask__DELETE(_task: *mut c_void) {
    // TODO(v82jsc): opaque C++ idle task; cannot delete. No-op.
}

// ===================================================================
// WeakCallbackInfo
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__WeakCallbackInfo__GetIsolate(_this: *const c_void) -> *mut c_void {
    // TODO(v82jsc): no real weak callbacks; surface the current isolate.
    current_iso() as *mut c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WeakCallbackInfo__GetParameter(_this: *const c_void) -> *mut c_void {
    // TODO(v82jsc): no real weak callbacks.
    ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WeakCallbackInfo__SetSecondPassCallback(
    _this: *const c_void,
    _callback: unsafe extern "C" fn(*const c_void),
) {
    // TODO(v82jsc): no second-pass weak callbacks. No-op.
}

// ===================================================================
// Wasm — JSC's C API exposes no WebAssembly compilation internals.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__Unpack(
    _isolate: *mut c_void,
    _value: *const Value,
    _that: *mut c_void,
) {
    // TODO(v82jsc): no Wasm streaming. Leaves the out param untouched.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__shared_ptr_DESTRUCT(_this: *mut c_void) {
    // TODO(v82jsc): no Wasm streaming shared_ptr to destruct. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__OnBytesReceived(
    _this: *mut c_void,
    _data: *const u8,
    _len: usize,
) {
    // TODO(v82jsc): no Wasm streaming. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__Finish(
    _this: *mut c_void,
    _callback: Option<unsafe extern "C" fn(*mut c_void)>,
) {
    // TODO(v82jsc): no Wasm streaming. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__Abort(_this: *mut c_void, _exception: *const Value) {
    // TODO(v82jsc): no Wasm streaming. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__SetUrl(
    _this: *mut c_void,
    _url: *const c_char,
    _len: usize,
) {
    // TODO(v82jsc): no Wasm streaming. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleObject__FromCompiledModule(
    _isolate: *mut c_void,
    _compiled_module: *const c_void,
) -> *const c_void {
    // TODO(v82jsc): no Wasm module support.
    ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleObject__GetCompiledModule(_this: *const c_void) -> *mut c_void {
    // TODO(v82jsc): no Wasm module support.
    ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleObject__Compile(
    _isolate: *mut c_void,
    _wire_bytes_data: *const u8,
    _length: usize,
) -> *mut c_void {
    // TODO(v82jsc): no Wasm compilation support.
    ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CompiledWasmModule__DELETE(_this: *mut c_void) {
    // TODO(v82jsc): no Wasm module support. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__NEW() -> *mut c_void {
    // TODO(v82jsc): no Wasm compilation support.
    ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__DELETE(_this: *mut c_void) {
    // TODO(v82jsc): no Wasm compilation support. No-op.
}

// ===================================================================
// std::shared_ptr<v8::Platform>
//
// `Platform` here is owned by the Rust side (see platform.rs). We back the
// shared_ptr with a tiny manually-refcounted box so use_count / copy / reset
// behave. Layout: SharedPtrBase<T> is `[usize; 2]` — we use slot 0 for the
// Platform pointer and slot 1 for the refcount box pointer.
// ===================================================================

#[repr(C)]
struct PlatformSharedRepr {
    platform: *mut c_void,
    refcount: *mut usize,
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__Platform__CONVERT__std__unique_ptr(
    unique_ptr: UniquePtr<crate::Platform>,
) -> SharedPtrBase<crate::Platform> {
    let raw = unique_ptr.into_raw() as *mut c_void;
    let repr = if raw.is_null() {
        PlatformSharedRepr {
            platform: ptr::null_mut(),
            refcount: ptr::null_mut(),
        }
    } else {
        PlatformSharedRepr {
            platform: raw,
            refcount: Box::into_raw(Box::new(1usize)),
        }
    };
    unsafe { std::mem::transmute::<PlatformSharedRepr, SharedPtrBase<crate::Platform>>(repr) }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__Platform__get(
    ptr: *const SharedPtrBase<crate::Platform>,
) -> *mut crate::Platform {
    if ptr.is_null() {
        return ptr::null_mut();
    }
    let repr = ptr as *const PlatformSharedRepr;
    unsafe { (*repr).platform as *mut crate::Platform }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__Platform__COPY(
    ptr: *const SharedPtrBase<crate::Platform>,
) -> SharedPtrBase<crate::Platform> {
    if ptr.is_null() {
        return SharedPtrBase::default();
    }
    let repr = ptr as *const PlatformSharedRepr;
    let (platform, refcount) = unsafe { ((*repr).platform, (*repr).refcount) };
    if !refcount.is_null() {
        unsafe { *refcount += 1 };
    }
    let copy = PlatformSharedRepr { platform, refcount };
    unsafe { std::mem::transmute::<PlatformSharedRepr, SharedPtrBase<crate::Platform>>(copy) }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__Platform__reset(
    ptr: *mut SharedPtrBase<crate::Platform>,
) {
    if ptr.is_null() {
        return;
    }
    let repr = ptr as *mut PlatformSharedRepr;
    unsafe {
        let refcount = (*repr).refcount;
        if !refcount.is_null() {
            *refcount -= 1;
            if *refcount == 0 {
                drop(Box::from_raw(refcount));
                let p = (*repr).platform as *mut crate::Platform;
                if !p.is_null() {
                    drop(Box::from_raw(p));
                }
            }
        }
        (*repr).platform = ptr::null_mut();
        (*repr).refcount = ptr::null_mut();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__Platform__use_count(
    ptr: *const SharedPtrBase<crate::Platform>,
) -> long {
    if ptr.is_null() {
        return 0;
    }
    let repr = ptr as *const PlatformSharedRepr;
    let refcount = unsafe { (*repr).refcount };
    if refcount.is_null() {
        0
    } else {
        unsafe { *refcount as long }
    }
}
