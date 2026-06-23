//! QuickJS-ng-backed shims for the "function" family:
//! Function / FunctionCallbackInfo / ReturnValue / Template / ObjectTemplate /
//! Signature / External.
//!
//! Ported from the JSC backend (`src/shim_function.rs`) with JSC C-API calls
//! swapped for QuickJS-ng calls, and from the deno PR's QuickJS logic
//! (`reference/qjs_v8_compat/src/function.rs`, `external.rs`). The C-ABI shape
//! (the `RawFunctionCallbackInfoParts` / `RawReturnValue` layouts and the
//! `*const FunctionCallbackInfo` contract) is identical to the JSC backend — the
//! vendored `src/function.rs` only cares about layout, not which engine backs it.
//!
//! ## Host functions
//! QuickJS-ng's `JS_NewCFunctionData` is documented to crash in this build (see
//! the PR), so — exactly like the PR's `build_op_function` — we create callable
//! functions with `JS_NewCFunction2(..., JS_CFUNC_generic_magic, magic)` and
//! recover the (v8 callback, data) pair from a per-thread dispatch table keyed by
//! the integer `magic`. Constructor (`new F()`) dispatch reuses the same table
//! via a `JS_CFUNC_constructor_magic` trampoline.
//!
//! ## Refcount discipline
//! Every shim that RETURNS a v8 handle routes its `JSValue` through
//! `intern`/`intern_dup` so the arena owns exactly one refcount. The dispatch
//! table holds an owned (`JS_DupValue`'d) copy of each callback's `data`
//! JSValue; it is intentionally never freed (lives for the isolate's lifetime,
//! like a v8 FunctionTemplate's data).

#![allow(non_snake_case, unused)]

use crate::qjs::quickjs_sys::*;
use crate::qjs::shim_core::{
    ctx_of, current_ctx, current_iso, intern, intern_dup, iso_state, jsval_of,
};
use crate::{
    Context, Data, External, Function, FunctionCallback, FunctionCallbackInfo,
    FunctionTemplate, Name, Object, ObjectTemplate, PropertyAttribute, RealIsolate,
    Signature, String, Value,
};
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

// ===================================================================
// Extra QuickJS-ng C API we need (declared locally; quickjs_sys.rs is owned by
// others — only add forward decls here, never duplicate existing ones).
// ===================================================================

unsafe extern "C" {
    /// Allocate a fresh class id (the in/out pointer is seeded to 0 by caller).
    fn JS_NewClassID(rt: *mut JSRuntime, pclass_id: *mut JSClassID) -> JSClassID;
    fn JS_NewClass(
        rt: *mut JSRuntime,
        class_id: JSClassID,
        class_def: *const JSClassDef,
    ) -> c_int;
    fn JS_NewObjectClass(ctx: *mut JSContext, class_id: c_int) -> JSValue;
    fn JS_SetOpaque(obj: JSValue, opaque: *mut c_void);
    fn JS_GetOpaque(obj: JSValue, class_id: JSClassID) -> *mut c_void;
    fn JS_SetPrototype(ctx: *mut JSContext, obj: JSValue, proto: JSValue) -> c_int;
    /// `JS_SetPropertyStr` consumes the value's refcount on success; declared in
    /// quickjs_sys already — reused via the `use *` import.
    fn JS_DefinePropertyValueStr(
        ctx: *mut JSContext,
        this_obj: JSValue,
        prop: *const c_char,
        val: JSValue,
        flags: c_int,
    ) -> c_int;
}

/// QuickJS class finalizer type.
type JSClassFinalizer = unsafe extern "C" fn(rt: *mut JSRuntime, val: JSValue);

#[repr(C)]
struct JSClassDef {
    class_name: *const c_char,
    finalizer: Option<JSClassFinalizer>,
    gc_mark: *const c_void,
    call: *const c_void,
    exotic: *const c_void,
}

// ===================================================================
// Private layouts mirrored from the vendored function.rs — the C ABI only cares
// about layout, so we replicate `RawReturnValue` / `RawFunctionCallbackInfoParts`
// exactly. (`return_value` is a usize that is a pointer to our JSValue slot.)
// ===================================================================

#[repr(C)]
struct RawReturnValue(usize);

#[repr(C)]
struct RawFunctionCallbackInfoParts {
    isolate: *mut RealIsolate,
    return_value: usize,
    data: *const Value,
    length: crate::support::int,
}

// ===================================================================
// Our own opaque layouts. v8/Deno never dereferences these; only our shims do.
//
// `FunctionCallbackInfo` -> *const CbInfo
// `FunctionTemplate`     -> *const FnTemplate
// `ObjectTemplate`       -> *const ObjTemplate
// `Signature`            -> *const FnTemplate (the receiver template)
// ===================================================================

/// Built fresh per JS call; a `*const FunctionCallbackInfo` points at this.
#[repr(C)]
struct CbInfo {
    isolate: *mut RealIsolate,
    ctx: *mut JSContext,
    this: JSValue,
    data: JSValue,
    new_target: JSValue,
    is_construct: bool,
    args: Vec<JSValue>,
    /// The slot the v8 `ReturnValue` writes into. The ABI hands its address out
    /// as a `usize` (see `GetReturnValue` / `GetParts`). Tag-undefined = unset.
    return_slot: Box<JSValue>,
}

/// FunctionTemplate config object.
struct FnTemplate {
    callback: FunctionCallback,
    /// Owned (+1) data JSValue, protected for the template's lifetime.
    data: JSValue,
    length: i32,
    class_name: Option<std::string::String>,
    proto: *mut ObjTemplate,
    instance: *mut ObjTemplate,
    parent: *const FnTemplate,
    /// Template properties (Template::Set): (key, value, attr) — JSValues borrowed
    /// from arena slots at Set time; we dup on materialization.
    props: Vec<(JSValue, JSValue, u32)>,
    /// Materialized `.prototype`, created once and cached (owned/+1) so
    /// `GetFunction` and `NewInstance` share the SAME object (see the JSC shim's
    /// note: without sharing, instances miss methods attached to `Class.prototype`).
    cached_proto: JSValue,
}

/// An accessor declared on a template via SetAccessorProperty.
struct TemplAccessor {
    key: JSValue,
    getter: *const FnTemplate,
    setter: *const FnTemplate,
    attr: u32,
}

/// ObjectTemplate config object.
struct ObjTemplate {
    internal_field_count: i32,
    props: Vec<(JSValue, JSValue, u32)>,
    accessors: Vec<TemplAccessor>,
    /// Back-pointer to the FunctionTemplate this is the *instance* template of
    /// (null for standalone ObjectTemplates). Lets NewInstance wire the created
    /// object's `__proto__` to the function's `.prototype`.
    parent_fn: *const FnTemplate,
}

// ===================================================================
// Per-thread dispatch table for host functions. JS_NewCFunctionData crashes in
// this build (per the PR), so we key on the JS_CFUNC_*_magic `magic` int.
// ===================================================================

struct DispatchEntry {
    callback: FunctionCallback,
    /// Owned (+1) data JSValue (or undefined). Lives for the isolate lifetime.
    data: JSValue,
}

thread_local! {
    static DISPATCH: std::cell::RefCell<Vec<DispatchEntry>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn register_dispatch(callback: FunctionCallback, data: JSValue) -> c_int {
    DISPATCH.with(|t| {
        let mut t = t.borrow_mut();
        let idx = t.len() as c_int;
        t.push(DispatchEntry { callback, data });
        idx
    })
}

fn lookup_dispatch(idx: c_int) -> Option<(FunctionCallback, JSValue)> {
    DISPATCH.with(|t| {
        t.borrow()
            .get(idx as usize)
            .map(|e| (e.callback, e.data))
    })
}

/// Invoke a stored v8 callback, building a `CbInfo` for it. Returns the owned
/// (+1) JSValue the callback wrote into its return slot (or `undefined`). If the
/// callback left a QuickJS pending exception (via the exception family's
/// `JS_Throw`), returns `JS_EXCEPTION` so QuickJS propagates it.
unsafe fn dispatch(
    ctx: *mut JSContext,
    callback: FunctionCallback,
    data: JSValue,
    this: JSValue,
    new_target: JSValue,
    is_construct: bool,
    argc: c_int,
    argv: *mut JSValue,
) -> JSValue {
    let n = argc.max(0) as usize;
    let mut args = Vec::with_capacity(n);
    for i in 0..n {
        args.push(unsafe { *argv.add(i) });
    }
    let iso = current_iso();
    let info = Box::new(CbInfo {
        isolate: iso,
        ctx,
        this,
        data,
        new_target,
        is_construct,
        args,
        return_slot: Box::new(jsv_undefined()),
    });
    let info_ptr = Box::into_raw(info) as *const FunctionCallbackInfo;
    unsafe { (callback)(info_ptr) };
    // Recover the return value and reclaim the CbInfo box.
    let info = unsafe { Box::from_raw(info_ptr as *mut CbInfo) };
    let ret = *info.return_slot;
    // If the callback raised a QuickJS exception, propagate it.
    if unsafe { JS_HasException(ctx) } != 0 {
        return jsv_exception();
    }
    if jsv_is_undefined(&ret) {
        return jsv_undefined();
    }
    // The return slot holds a value owned by some Rust arena slot that may be
    // freed when its handle scope pops; dup so the JS-held copy survives.
    unsafe { JS_DupValue(ctx, ret) }
}

/// Regular-call trampoline (`JS_CFUNC_generic_magic`).
unsafe extern "C" fn fn_trampoline(
    ctx: *mut JSContext,
    this_val: JSValue,
    argc: c_int,
    argv: *mut JSValue,
    magic: c_int,
) -> JSValue {
    let Some((callback, data)) = lookup_dispatch(magic) else {
        return jsv_undefined();
    };
    unsafe {
        dispatch(
            ctx,
            callback,
            data,
            this_val,
            jsv_undefined(),
            false,
            argc,
            argv,
        )
    }
}

/// Constructor trampoline (`JS_CFUNC_constructor_magic`). `this_val` is the
/// new.target (the constructor function); QuickJS expects us to return the new
/// object.
unsafe extern "C" fn fn_construct_trampoline(
    ctx: *mut JSContext,
    new_target: JSValue,
    argc: c_int,
    argv: *mut JSValue,
    magic: c_int,
) -> JSValue {
    let Some((callback, data)) = lookup_dispatch(magic) else {
        return unsafe { JS_NewObject(ctx) };
    };
    // Fresh `this` whose prototype is the constructor's `.prototype`, so instance
    // methods are reachable (matches `new F()`).
    let this = unsafe { JS_NewObject(ctx) };
    unsafe {
        let proto = JS_GetPropertyStr(ctx, new_target, c"prototype".as_ptr());
        if jsv_is_object(&proto) {
            JS_SetPrototype(ctx, this, proto);
        }
        JS_FreeValue(ctx, proto);
    }
    let r = unsafe {
        dispatch(ctx, callback, data, this, new_target, true, argc, argv)
    };
    if jsv_is_exception(&r) {
        unsafe { JS_FreeValue(ctx, this) };
        return r;
    }
    // If the callback returned an object, use it; else use `this`.
    if jsv_is_object(&r) {
        unsafe { JS_FreeValue(ctx, this) };
        r
    } else {
        unsafe { JS_FreeValue(ctx, r) };
        this
    }
}

/// Transmute our magic-trampoline into the `JSCFunction` pointer type
/// `JS_NewCFunction2` wants. This is the documented quickjs pattern (the
/// `JS_NewCFunctionMagic` inline wrapper does the same cast).
unsafe fn make_cfunc_magic(
    ctx: *mut JSContext,
    trampoline: unsafe extern "C" fn(
        *mut JSContext,
        JSValue,
        c_int,
        *mut JSValue,
        c_int,
    ) -> JSValue,
    name: *const c_char,
    length: c_int,
    cproto: c_int,
    magic: c_int,
) -> JSValue {
    unsafe {
        let f: JSCFunction = std::mem::transmute::<
            unsafe extern "C" fn(
                *mut JSContext,
                JSValue,
                c_int,
                *mut JSValue,
                c_int,
            ) -> JSValue,
            JSCFunction,
        >(trampoline);
        JS_NewCFunction2(ctx, f, name, length, cproto, magic)
    }
}

/// Create a callable JS function carrying `callback`/`data`. `data` is borrowed;
/// we dup it (the dispatch table owns its copy for the isolate lifetime). The
/// returned JSValue is owned (+1).
unsafe fn make_function_len(
    ctx: *mut JSContext,
    callback: FunctionCallback,
    data: JSValue,
    length: i32,
    construct: bool,
) -> JSValue {
    let data_owned = if jsv_is_undefined(&data) {
        jsv_undefined()
    } else {
        unsafe { JS_DupValue(ctx, data) }
    };
    let magic = register_dispatch(callback, data_owned);
    let cproto = if construct {
        JS_CFUNC_CONSTRUCTOR_MAGIC
    } else {
        JS_CFUNC_GENERIC_MAGIC
    };
    let tramp = if construct {
        fn_construct_trampoline
    } else {
        fn_trampoline
    };
    unsafe {
        make_cfunc_magic(ctx, tramp, ptr::null(), length.max(0), cproto, magic)
    }
}

// `JS_CFUNC_constructor_magic` lives between constructor (2) and
// constructor_or_func (4): quickjs enum order is generic=0, generic_magic=1,
// constructor=2, constructor_magic=3, constructor_or_func=4, ...
const JS_CFUNC_CONSTRUCTOR_MAGIC: c_int = 3;

#[inline]
fn cbinfo<'a>(this: *const FunctionCallbackInfo) -> &'a mut CbInfo {
    unsafe { &mut *(this as *mut CbInfo) }
}

// ===================================================================
// External — a plain object of our `ext_class` carrying the raw pointer as
// opaque private data, so `External::Value` can recover it and the value family
// can identify it by class.
// ===================================================================

thread_local! {
    static EXT_CLASS_ID: std::cell::Cell<JSClassID> = const { std::cell::Cell::new(0) };
}

unsafe extern "C" fn ext_finalize(_rt: *mut JSRuntime, _val: JSValue) {
    // The opaque is a borrowed raw embedder pointer; nothing to free.
}

fn ext_class_id(rt: *mut JSRuntime) -> JSClassID {
    EXT_CLASS_ID.with(|c| {
        let existing = c.get();
        if existing != 0 {
            return existing;
        }
        let mut id: JSClassID = 0;
        let id = unsafe { JS_NewClassID(rt, &mut id) };
        let def = JSClassDef {
            class_name: c"v8jsc_external".as_ptr(),
            finalizer: Some(ext_finalize),
            gc_mark: ptr::null(),
            call: ptr::null(),
            exotic: ptr::null(),
        };
        unsafe { JS_NewClass(rt, id, &def) };
        c.set(id);
        id
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__External__New(
    isolate: *mut RealIsolate,
    value: *mut c_void,
) -> *const External {
    if isolate.is_null() {
        return ptr::null();
    }
    let st = iso_state(isolate);
    let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
    if ctx.is_null() {
        return ptr::null();
    }
    let cid = ext_class_id(st.rt);
    let obj = unsafe { JS_NewObjectClass(ctx, cid as c_int) };
    if jsv_is_exception(&obj) {
        return ptr::null();
    }
    unsafe { JS_SetOpaque(obj, value) };
    // `obj` is owned (+1); move it into the arena.
    intern::<External>(obj)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__External__Value(this: *const External) -> *mut c_void {
    if this.is_null() {
        return ptr::null_mut();
    }
    let iso = current_iso();
    if iso.is_null() {
        return ptr::null_mut();
    }
    let cid = EXT_CLASS_ID.with(|c| c.get());
    if cid == 0 {
        return ptr::null_mut();
    }
    unsafe { JS_GetOpaque(jsval_of(this), cid) }
}

/// Whether `v` is one of our `External` objects (used by the value family's
/// `v8__Value__IsExternal`). Reports false for non-objects / when no External
/// has been created yet.
pub(crate) fn value_is_external(v: JSValue) -> bool {
    if !jsv_is_object(&v) {
        return false;
    }
    let cid = EXT_CLASS_ID.with(|c| c.get());
    if cid == 0 {
        return false;
    }
    !unsafe { JS_GetOpaque(v, cid) }.is_null()
}

// ===================================================================
// Function
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__New(
    context: *const Context,
    callback: FunctionCallback,
    data_or_null: *const Value,
    length: i32,
    constructor_behavior: crate::ConstructorBehavior,
    side_effect_type: crate::SideEffectType,
) -> *const Function {
    let _ = side_effect_type;
    let ctx = ctx_of(context);
    if ctx.is_null() {
        return ptr::null();
    }
    let construct = matches!(
        constructor_behavior,
        crate::ConstructorBehavior::Allow
    );
    let data = jsval_of(data_or_null);
    let f = unsafe { make_function_len(ctx, callback, data, length, construct) };
    intern::<Function>(f)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__Call(
    this: *const Function,
    context: *const Context,
    recv: *const Value,
    argc: crate::support::int,
    argv: *const *const Value,
) -> *const Value {
    let ctx = ctx_of(context);
    if ctx.is_null() || this.is_null() {
        return ptr::null();
    }
    let func = jsval_of(this);
    let recv_v = if recv.is_null() {
        jsv_undefined()
    } else {
        jsval_of(recv)
    };
    let n = argc.max(0) as usize;
    let mut args: Vec<JSValue> = Vec::with_capacity(n);
    for i in 0..n {
        args.push(jsval_of(unsafe { *argv.add(i) }));
    }
    let r = unsafe {
        JS_Call(ctx, func, recv_v, n as c_int, args.as_mut_ptr())
    };
    if jsv_is_exception(&r) {
        // Leave the QuickJS exception pending so a surrounding TryCatch (the
        // exception family) can observe it via JS_GetException.
        return ptr::null();
    }
    // `r` is owned (+1).
    intern::<Value>(r)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__NewInstance(
    this: *const Function,
    context: *const Context,
    argc: crate::support::int,
    argv: *const *const Value,
) -> *const Object {
    let ctx = ctx_of(context);
    if ctx.is_null() || this.is_null() {
        return ptr::null();
    }
    let func = jsval_of(this);
    let n = argc.max(0) as usize;
    let mut args: Vec<JSValue> = Vec::with_capacity(n);
    for i in 0..n {
        args.push(jsval_of(unsafe { *argv.add(i) }));
    }
    let r = unsafe {
        JS_CallConstructor(ctx, func, n as c_int, args.as_mut_ptr())
    };
    if jsv_is_exception(&r) {
        return ptr::null();
    }
    intern::<Object>(r)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__SetName(this: *const Function, name: *const String) {
    if this.is_null() || name.is_null() {
        return;
    }
    let ctx = current_ctx();
    if ctx.is_null() {
        return;
    }
    // `JS_SetPropertyStr` consumes the value's refcount; dup so the caller's
    // arena slot keeps its own.
    let v = unsafe { JS_DupValue(ctx, jsval_of(name)) };
    unsafe {
        JS_SetPropertyStr(ctx, jsval_of(this), c"name".as_ptr(), v);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__CreateCodeCache(
    script: *const Function,
) -> *mut crate::CachedData<'static> {
    // TODO(qjs): QuickJS has no per-function code-cache serialization exposed.
    let _ = script;
    ptr::null_mut()
}

// ===================================================================
// FunctionCallbackInfo
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__GetIsolate(
    this: *const FunctionCallbackInfo,
) -> *mut RealIsolate {
    if this.is_null() {
        return current_iso();
    }
    cbinfo(this).isolate
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__GetParts(
    this: *const FunctionCallbackInfo,
) -> RawFunctionCallbackInfoParts {
    if this.is_null() {
        return RawFunctionCallbackInfoParts {
            isolate: current_iso(),
            return_value: 0,
            data: ptr::null(),
            length: 0,
        };
    }
    let info = cbinfo(this);
    let slot = &mut *info.return_slot as *mut JSValue;
    // `data` must be a v8 handle (arena slot). Dup the borrowed data JSValue
    // into the current scope so the returned `*const Value` is valid.
    let data = intern_dup::<Value>(info.ctx, info.data);
    RawFunctionCallbackInfoParts {
        isolate: info.isolate,
        return_value: slot as usize,
        data,
        length: info.args.len() as crate::support::int,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__Data(
    this: *const FunctionCallbackInfo,
) -> *const Value {
    if this.is_null() {
        return ptr::null();
    }
    let info = cbinfo(this);
    intern_dup::<Value>(info.ctx, info.data)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__This(
    this: *const FunctionCallbackInfo,
) -> *const Object {
    if this.is_null() {
        return ptr::null();
    }
    let info = cbinfo(this);
    intern_dup::<Object>(info.ctx, info.this)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__Get(
    this: *const FunctionCallbackInfo,
    index: crate::support::int,
) -> *const Value {
    if this.is_null() {
        return ptr::null();
    }
    let info = cbinfo(this);
    if index < 0 {
        return intern_dup::<Value>(info.ctx, jsv_undefined());
    }
    match info.args.get(index as usize) {
        Some(&v) => intern_dup::<Value>(info.ctx, v),
        None => intern_dup::<Value>(info.ctx, jsv_undefined()),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__Length(
    this: *const FunctionCallbackInfo,
) -> crate::support::int {
    if this.is_null() {
        return 0;
    }
    cbinfo(this).args.len() as crate::support::int
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__GetReturnValue(
    this: *const FunctionCallbackInfo,
) -> usize {
    if this.is_null() {
        return 0;
    }
    let info = cbinfo(this);
    (&mut *info.return_slot as *mut JSValue) as usize
}

// ===================================================================
// ReturnValue — `this` is `*mut RawReturnValue`, holding a usize that is a
// pointer to our JSValue return slot.
// ===================================================================

#[inline]
unsafe fn rv_slot(this: *mut RawReturnValue) -> *mut JSValue {
    if this.is_null() {
        return ptr::null_mut();
    }
    unsafe { (*this).0 as *mut JSValue }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Set(
    this: *mut RawReturnValue,
    value: *const Value,
) {
    let slot = unsafe { rv_slot(this) };
    if !slot.is_null() {
        // The slot holds a borrowed reference (owned by an arena slot). `dispatch`
        // dups it before handing it to JS, so we just record the value here.
        unsafe { *slot = jsval_of(value) };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Set__Bool(
    this: *mut RawReturnValue,
    value: bool,
) {
    let slot = unsafe { rv_slot(this) };
    if !slot.is_null() {
        unsafe { *slot = jsv_bool(value) };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Set__Int32(
    this: *mut RawReturnValue,
    value: i32,
) {
    let slot = unsafe { rv_slot(this) };
    if !slot.is_null() {
        unsafe { *slot = jsv_int32(value) };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Set__Uint32(
    this: *mut RawReturnValue,
    value: u32,
) {
    let slot = unsafe { rv_slot(this) };
    if !slot.is_null() {
        let v = if value <= i32::MAX as u32 {
            jsv_int32(value as i32)
        } else {
            jsv_float64(value as f64)
        };
        unsafe { *slot = v };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Set__Double(
    this: *mut RawReturnValue,
    value: f64,
) {
    let slot = unsafe { rv_slot(this) };
    if !slot.is_null() {
        unsafe { *slot = jsv_float64(value) };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__SetNull(this: *mut RawReturnValue) {
    let slot = unsafe { rv_slot(this) };
    if !slot.is_null() {
        unsafe { *slot = jsv_null() };
    }
}

// ===================================================================
// Template (base) — Set a property on a template's property list. `*const
// Template` aliases either a FnTemplate or ObjTemplate; we tag each template
// pointer in a registry so we know which `props` vec to push to.
// ===================================================================

trait AttrU32 {
    fn as_u32_lenient(&self) -> u32;
}
impl AttrU32 for PropertyAttribute {
    fn as_u32_lenient(&self) -> u32 {
        // SAFETY: PropertyAttribute is #[repr(C)] over a u32.
        unsafe { *(self as *const PropertyAttribute as *const u32) }
    }
}

thread_local! {
    static TEMPLATES: std::cell::RefCell<std::collections::HashMap<usize, TemplKind>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

#[derive(Clone, Copy)]
enum TemplKind {
    Func,
    Obj,
}

fn register_template(p: usize, kind: TemplKind) {
    TEMPLATES.with(|t| {
        t.borrow_mut().insert(p, kind);
    });
}

/// Whether `p` is one of our FunctionTemplate / ObjectTemplate handles. These
/// are Rust box pointers, NOT JSValues — callers that would otherwise treat a
/// `*const Data` as a value (e.g. `Global::new`) must special-case them.
pub(crate) fn is_template_ptr(p: *const c_void) -> bool {
    if p.is_null() {
        return false;
    }
    TEMPLATES.with(|t| t.borrow().contains_key(&(p as usize)))
}

fn with_template_props(p: usize, f: impl FnOnce(&mut Vec<(JSValue, JSValue, u32)>)) {
    let kind = TEMPLATES.with(|t| t.borrow().get(&p).copied());
    match kind {
        Some(TemplKind::Func) => {
            let t = unsafe { &mut *(p as *mut FnTemplate) };
            f(&mut t.props);
        }
        Some(TemplKind::Obj) => {
            let t = unsafe { &mut *(p as *mut ObjTemplate) };
            f(&mut t.props);
        }
        None => {}
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Template__Set(
    this: *const crate::Template,
    key: *const Name,
    value: *const Data,
    attr: PropertyAttribute,
) {
    if this.is_null() {
        return;
    }
    let raw = this as *const c_void as usize;
    with_template_props(raw, |props| {
        props.push((jsval_of(key), jsval_of(value), attr.as_u32_lenient()));
    });
}

// ===================================================================
// FunctionTemplate
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__New(
    isolate: *mut RealIsolate,
    callback: FunctionCallback,
    data_or_null: *const Value,
    signature_or_null: *const Signature,
    length: i32,
    constructor_behavior: crate::ConstructorBehavior,
    side_effect_type: crate::SideEffectType,
    c_functions: *const crate::fast_api::CFunction,
    c_functions_len: usize,
) -> *const FunctionTemplate {
    let _ = (
        isolate,
        signature_or_null,
        constructor_behavior,
        side_effect_type,
        c_functions,
        c_functions_len,
    );
    // Dup+protect the data JSValue for the template's lifetime.
    let data = {
        let ctx = current_ctx();
        let d = jsval_of(data_or_null);
        if !ctx.is_null() && !jsv_is_undefined(&d) {
            unsafe { JS_DupValue(ctx, d) }
        } else {
            jsv_undefined()
        }
    };
    let proto = Box::into_raw(Box::new(ObjTemplate {
        internal_field_count: 0,
        props: Vec::new(),
        accessors: Vec::new(),
        parent_fn: ptr::null(),
    }));
    let instance = Box::into_raw(Box::new(ObjTemplate {
        internal_field_count: 0,
        props: Vec::new(),
        accessors: Vec::new(),
        parent_fn: ptr::null(),
    }));
    register_template(proto as usize, TemplKind::Obj);
    register_template(instance as usize, TemplKind::Obj);
    let t = Box::into_raw(Box::new(FnTemplate {
        callback,
        data,
        length,
        class_name: None,
        proto,
        instance,
        parent: ptr::null(),
        props: Vec::new(),
        cached_proto: jsv_undefined(),
    }));
    unsafe { (*instance).parent_fn = t };
    register_template(t as usize, TemplKind::Func);
    t as *const FunctionTemplate
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__GetFunction(
    this: *const FunctionTemplate,
    context: *const Context,
) -> *const Function {
    if this.is_null() {
        return ptr::null();
    }
    let ctx = ctx_of(context);
    if ctx.is_null() {
        return ptr::null();
    }
    let t = unsafe { &*(this as *const FnTemplate) };
    // FunctionTemplates can be `new`'d, so make a constructor-capable function.
    let f = unsafe { make_function_len(ctx, t.callback, t.data, t.length, true) };

    // Class name -> function `name`.
    if let Some(name) = &t.class_name {
        if let Ok(cname) = std::ffi::CString::new(name.as_str()) {
            let nameval = unsafe { JS_NewString(ctx, cname.as_ptr()) };
            unsafe {
                JS_SetPropertyStr(ctx, f, c"name".as_ptr(), nameval);
            }
        }
    }

    // Static template props go directly on the function.
    apply_props(ctx, f, &t.props);

    // Install the shared `.prototype` object (carries prototype-template methods).
    let proto_obj = unsafe { build_prototype_object(ctx, this as *const FnTemplate) };
    let proto_dup = unsafe { JS_DupValue(ctx, proto_obj) };
    unsafe {
        JS_SetPropertyStr(ctx, f, c"prototype".as_ptr(), proto_dup);
    }

    intern::<Function>(f)
}

/// Return the `.prototype` object for a FunctionTemplate, creating it once and
/// caching it (owned/+1) on the template. Splices a parent template's prototype
/// in as `__proto__` for `Inherit` chains.
unsafe fn build_prototype_object(ctx: *mut JSContext, tp: *const FnTemplate) -> JSValue {
    let t = unsafe { &mut *(tp as *mut FnTemplate) };
    if jsv_is_object(&t.cached_proto) {
        return t.cached_proto;
    }
    let proto_obj = unsafe { JS_NewObject(ctx) };
    let proto = unsafe { &*t.proto };
    if !proto.props.is_empty() {
        apply_props(ctx, proto_obj, &proto.props);
    }
    apply_accessors(ctx, proto_obj, &proto.accessors);
    if !t.parent.is_null() {
        let parent_proto = unsafe { build_prototype_object(ctx, t.parent) };
        let dup = unsafe { JS_DupValue(ctx, parent_proto) };
        unsafe { JS_SetPrototype(ctx, proto_obj, dup) };
        unsafe { JS_FreeValue(ctx, dup) };
    }
    // Cache the owned (+1) object; it lives for the template's lifetime.
    t.cached_proto = proto_obj;
    proto_obj
}

fn apply_props(
    ctx: *mut JSContext,
    obj: JSValue,
    props: &[(JSValue, JSValue, u32)],
) {
    for &(key, value, attr) in props {
        if jsv_is_undefined(&key) {
            continue;
        }
        // Key -> C string.
        let mut len: usize = 0;
        let keystr = unsafe { JS_ToCStringLen(ctx, &mut len, key) };
        if keystr.is_null() {
            continue;
        }
        // A template property value may itself be a nested FunctionTemplate /
        // ObjectTemplate (a Rust box pointer, NOT a JSValue) — materialize it.
        let value = materialize_template_value(ctx, value);
        // `JS_SetPropertyStr` consumes the value refcount; dup so we don't
        // disturb the source arena slot's count.
        let v = unsafe { JS_DupValue(ctx, value) };
        unsafe {
            JS_SetPropertyStr(ctx, obj, keystr, v);
            JS_FreeCString(ctx, keystr);
        }
    }
}

/// If `value` is one of our template handles (a Rust box ptr smuggled where a
/// JSValue is expected), instantiate it into a real JS value; else return it.
fn materialize_template_value(ctx: *mut JSContext, value: JSValue) -> JSValue {
    // Template handles are box pointers, not JSValues; they were stored via
    // Template::Set as `jsval_of(value)` of a `*const Data` that is actually a
    // template pointer. Such a "JSValue" would have a bogus tag; only treat it
    // as a template if the raw pointer is registered.
    let raw = unsafe { value.u.ptr } as usize;
    let kind = TEMPLATES.with(|t| t.borrow().get(&raw).copied());
    match kind {
        Some(TemplKind::Func) => {
            let f = v8__FunctionTemplate__GetFunction(
                raw as *const FunctionTemplate,
                ctx as *const Context,
            );
            if f.is_null() { value } else { jsval_of(f) }
        }
        Some(TemplKind::Obj) => {
            let o = v8__ObjectTemplate__NewInstance(
                raw as *const ObjectTemplate,
                ctx as *const Context,
            );
            if o.is_null() { value } else { jsval_of(o) }
        }
        None => value,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__Inherit(
    this: *const FunctionTemplate,
    parent: *const FunctionTemplate,
) {
    if this.is_null() {
        return;
    }
    let t = unsafe { &mut *(this as *mut FnTemplate) };
    t.parent = parent as *const FnTemplate;
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__PrototypeTemplate(
    this: *const FunctionTemplate,
) -> *const ObjectTemplate {
    if this.is_null() {
        return ptr::null();
    }
    let t = unsafe { &*(this as *const FnTemplate) };
    t.proto as *const ObjectTemplate
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__InstanceTemplate(
    this: *const FunctionTemplate,
) -> *const ObjectTemplate {
    if this.is_null() {
        return ptr::null();
    }
    let t = unsafe { &*(this as *const FnTemplate) };
    t.instance as *const ObjectTemplate
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__SetClassName(
    this: *const FunctionTemplate,
    name: *const String,
) {
    if this.is_null() || name.is_null() {
        return;
    }
    let t = unsafe { &mut *(this as *mut FnTemplate) };
    let ctx = current_ctx();
    if ctx.is_null() {
        return;
    }
    let mut len: usize = 0;
    let s = unsafe { JS_ToCStringLen(ctx, &mut len, jsval_of(name)) };
    if s.is_null() {
        return;
    }
    let bytes = unsafe { std::slice::from_raw_parts(s as *const u8, len) };
    if let Ok(name) = std::str::from_utf8(bytes) {
        t.class_name = Some(name.to_owned());
    }
    unsafe { JS_FreeCString(ctx, s) };
}

// ===================================================================
// ObjectTemplate
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__New(
    isolate: *mut RealIsolate,
    templ: *const FunctionTemplate,
) -> *const ObjectTemplate {
    let _ = (isolate, templ);
    let t = Box::into_raw(Box::new(ObjTemplate {
        internal_field_count: 0,
        props: Vec::new(),
        accessors: Vec::new(),
        parent_fn: ptr::null(),
    }));
    register_template(t as usize, TemplKind::Obj);
    t as *const ObjectTemplate
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__NewInstance(
    this: *const ObjectTemplate,
    context: *const Context,
) -> *const Object {
    let ctx = ctx_of(context);
    if ctx.is_null() {
        return ptr::null();
    }
    let obj = unsafe { JS_NewObject(ctx) };
    if !this.is_null() {
        let t = unsafe { &*(this as *const ObjTemplate) };
        if !t.parent_fn.is_null() {
            let proto_obj = unsafe { build_prototype_object(ctx, t.parent_fn) };
            let dup = unsafe { JS_DupValue(ctx, proto_obj) };
            unsafe { JS_SetPrototype(ctx, obj, dup) };
            unsafe { JS_FreeValue(ctx, dup) };
        }
        apply_props(ctx, obj, &t.props);
        apply_accessors(ctx, obj, &t.accessors);
    }
    intern::<Object>(obj)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__SetAccessorProperty(
    this: *const ObjectTemplate,
    key: *const Name,
    getter: *const FunctionTemplate,
    setter: *const FunctionTemplate,
    attr: PropertyAttribute,
) {
    if this.is_null() {
        return;
    }
    let t = unsafe { &mut *(this as *mut ObjTemplate) };
    t.accessors.push(TemplAccessor {
        key: jsval_of(key),
        getter: getter as *const FnTemplate,
        setter: setter as *const FnTemplate,
        attr: attr.as_u32_lenient(),
    });
}

/// Install template accessors via `Object.defineProperty(obj, key, {get,set,..})`.
fn apply_accessors(ctx: *mut JSContext, obj: JSValue, accessors: &[TemplAccessor]) {
    if accessors.is_empty() {
        return;
    }
    unsafe {
        let global = JS_GetGlobalObject(ctx);
        let object_ctor = JS_GetPropertyStr(ctx, global, c"Object".as_ptr());
        JS_FreeValue(ctx, global);
        if !jsv_is_object(&object_ctor) {
            JS_FreeValue(ctx, object_ctor);
            return;
        }
        let define = JS_GetPropertyStr(ctx, object_ctor, c"defineProperty".as_ptr());
        if !jsv_is_object(&define) {
            JS_FreeValue(ctx, define);
            JS_FreeValue(ctx, object_ctor);
            return;
        }

        for acc in accessors {
            if jsv_is_undefined(&acc.key) {
                continue;
            }
            let desc = JS_NewObject(ctx);
            if !acc.getter.is_null() {
                let gf = v8__FunctionTemplate__GetFunction(
                    acc.getter as *const FunctionTemplate,
                    ctx as *const Context,
                );
                if !gf.is_null() {
                    let v = JS_DupValue(ctx, jsval_of(gf));
                    JS_SetPropertyStr(ctx, desc, c"get".as_ptr(), v);
                }
            }
            if !acc.setter.is_null() {
                let sf = v8__FunctionTemplate__GetFunction(
                    acc.setter as *const FunctionTemplate,
                    ctx as *const Context,
                );
                if !sf.is_null() {
                    let v = JS_DupValue(ctx, jsval_of(sf));
                    JS_SetPropertyStr(ctx, desc, c"set".as_ptr(), v);
                }
            }
            // v8 PropertyAttribute: DontEnum=2, DontDelete=4.
            let enumerable = (acc.attr & 2) == 0;
            let configurable = (acc.attr & 4) == 0;
            JS_SetPropertyStr(ctx, desc, c"enumerable".as_ptr(), jsv_bool(enumerable));
            JS_SetPropertyStr(
                ctx,
                desc,
                c"configurable".as_ptr(),
                jsv_bool(configurable),
            );

            let mut args = [
                JS_DupValue(ctx, obj),
                JS_DupValue(ctx, acc.key),
                desc, // owned; consumed below
            ];
            let r = JS_Call(ctx, define, object_ctor, 3, args.as_mut_ptr());
            JS_FreeValue(ctx, args[0]);
            JS_FreeValue(ctx, args[1]);
            JS_FreeValue(ctx, args[2]);
            if !jsv_is_exception(&r) {
                JS_FreeValue(ctx, r);
            }
        }

        JS_FreeValue(ctx, define);
        JS_FreeValue(ctx, object_ctor);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__SetIndexedPropertyHandler(
    this: *const ObjectTemplate,
    getter: Option<crate::IndexedPropertyGetterCallback>,
    setter: Option<crate::IndexedPropertySetterCallback>,
    query: Option<crate::IndexedPropertyQueryCallback>,
    deleter: Option<crate::IndexedPropertyDeleterCallback>,
    enumerator: Option<crate::IndexedPropertyEnumeratorCallback>,
    definer: Option<crate::IndexedPropertyDefinerCallback>,
    descriptor: Option<crate::IndexedPropertyDescriptorCallback>,
    data_or_null: *const Value,
    flags: crate::PropertyHandlerFlags,
) {
    // TODO(qjs): indexed interceptors need a custom exotic class with
    // get/set/has handlers bridged to v8 PropertyCallbackInfo; not yet wired.
    let _ = (
        this, getter, setter, query, deleter, enumerator, definer, descriptor,
        data_or_null, flags,
    );
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__SetInternalFieldCount(
    this: *const ObjectTemplate,
    value: crate::support::int,
) {
    if this.is_null() {
        return;
    }
    let t = unsafe { &mut *(this as *mut ObjTemplate) };
    t.internal_field_count = value;
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__SetNamedPropertyHandler(
    this: *const ObjectTemplate,
    getter: Option<crate::NamedPropertyGetterCallback>,
    setter: Option<crate::NamedPropertySetterCallback>,
    query: Option<crate::NamedPropertyQueryCallback>,
    deleter: Option<crate::NamedPropertyDeleterCallback>,
    enumerator: Option<crate::NamedPropertyEnumeratorCallback>,
    definer: Option<crate::NamedPropertyDefinerCallback>,
    descriptor: Option<crate::NamedPropertyDescriptorCallback>,
    data_or_null: *const Value,
    flags: crate::PropertyHandlerFlags,
) {
    // TODO(qjs): named interceptors need a custom exotic class with property
    // hooks bridged to v8 PropertyCallbackInfo; not yet wired.
    let _ = (
        this, getter, setter, query, deleter, enumerator, definer, descriptor,
        data_or_null, flags,
    );
}
