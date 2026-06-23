//! JSC-backed shims for the "function" family:
//! Function / FunctionCallbackInfo / ReturnValue / Template / ObjectTemplate /
//! Signature / External.
#![allow(non_snake_case, unused)]

use crate::jsc_sys::*;
use crate::shim_core::{
    ctx_of, current_ctx, current_iso, intern, intern_ctx, iso_state, jsval,
};
use crate::{
    Context, Data, External, Function, FunctionCallback, FunctionCallbackInfo,
    FunctionTemplate, Name, Object, ObjectTemplate, PropertyAttribute, RealIsolate,
    Signature, String, Value,
};
use std::convert::TryFrom;
use std::os::raw::{c_char, c_void};
use std::ptr;

// ===================================================================
// JSC C API functions come from `crate::jsc_sys` (bindgen) via the glob import.
//
// `FnJSClassDefinition` is a layout-compatible mirror of bindgen's
// `JSClassDefinition` with the callback fields type-erased to `*const c_void`
// (the callbacks are set with raw fn-pointer casts). It is passed to
// `JSClassCreate` via a cast.
// ===================================================================

#[repr(C)]
struct FnJSClassDefinition {
    version: std::os::raw::c_int,
    attributes: u32,
    className: *const c_char,
    parentClass: JSClassRef,
    staticValues: *const c_void,
    staticFunctions: *const c_void,
    initialize: *const c_void,
    finalize: *const c_void,
    hasProperty: *const c_void,
    getProperty: *const c_void,
    setProperty: *const c_void,
    deleteProperty: *const c_void,
    getPropertyNames: *const c_void,
    callAsFunction: *const c_void,
    callAsConstructor: *const c_void,
    hasInstance: *const c_void,
    convertToType: *const c_void,
}

// ===================================================================
// Private layouts mirrored from function.rs (those types are not `pub`,
// but the C ABI only cares about layout, so we replicate them exactly).
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
// Our own opaque layouts. v8/Deno never dereferences these; only our shims
// do, so we are free to choose the representation.
//
// `FunctionCallbackInfo` -> *const CbInfo
// `FunctionTemplate`     -> *const FnTemplate
// `ObjectTemplate`       -> *const ObjTemplate
// `Signature`            -> *const FnTemplate (the receiver template)
// ===================================================================

/// The boxed bridge state attached (as JSC private data) to every function
/// object we create, so the JSC trampoline can recover the v8 callback.
struct FnBridge {
    callback: FunctionCallback,
    data: JSValueRef,
    ctx: JSGlobalContextRef,
}

/// Built fresh per JS call; a `*const FunctionCallbackInfo` points at this.
#[repr(C)]
struct CbInfo {
    isolate: *mut RealIsolate,
    ctx: JSContextRef,
    this: JSValueRef,
    data: JSValueRef,
    new_target: JSValueRef,
    is_construct: bool,
    args: Vec<JSValueRef>,
    /// The slot the v8 ReturnValue writes into. `0` means "unset".
    return_slot: Box<JSValueRef>,
}

/// FunctionTemplate config object.
struct FnTemplate {
    callback: FunctionCallback,
    data: JSValueRef,
    length: i32,
    class_name: Option<std::string::String>,
    proto: *mut ObjTemplate,
    instance: *mut ObjTemplate,
    parent: *const FnTemplate,
    // properties set via Template::Set: (name, value, attr)
    props: Vec<(JSValueRef, JSValueRef, u32)>,
    /// The materialized `.prototype` object, created on first use and cached so
    /// that `GetFunction` (which installs methods onto it) and `NewInstance`
    /// (which wires it as the instance's `__proto__`) share the SAME object.
    /// Without sharing, instances would get a fresh empty prototype and miss the
    /// methods that JS later attaches to `Class.prototype`. JSValueProtected for
    /// the template's lifetime.
    cached_proto: JSValueRef,
}

/// An accessor (getter/setter) declared on a template via SetAccessorProperty.
/// `getter`/`setter` are FunctionTemplate handles (Rust box pointers) to be
/// materialized into JS functions when the template is instantiated.
struct TemplAccessor {
    key: JSValueRef,
    getter: *const FnTemplate,
    setter: *const FnTemplate,
    attr: u32,
}

/// ObjectTemplate config object.
struct ObjTemplate {
    internal_field_count: i32,
    props: Vec<(JSValueRef, JSValueRef, u32)>,
    accessors: Vec<TemplAccessor>,
    /// If this ObjectTemplate is the *instance* template of a FunctionTemplate,
    /// a back-pointer to that FunctionTemplate. Used by `NewInstance` to wire the
    /// created object's prototype chain to the function's `.prototype` (which
    /// carries the prototype-template methods), matching V8 semantics where
    /// `instance_template.new_instance()` yields an object whose `__proto__` is
    /// the function's prototype. Null for standalone ObjectTemplates.
    parent_fn: *const FnTemplate,
}

// Class used to make callable function objects carrying an FnBridge.
thread_local! {
    static FN_CLASS: std::cell::Cell<JSClassRef> = const { std::cell::Cell::new(ptr::null_mut()) };
}

fn fn_class() -> JSClassRef {
    FN_CLASS.with(|c| {
        let existing = c.get();
        if !existing.is_null() {
            return existing;
        }
        let def = FnJSClassDefinition {
            version: 0,
            attributes: 0,
            className: c"v8jsc_fn".as_ptr(),
            parentClass: ptr::null_mut(),
            staticValues: ptr::null(),
            staticFunctions: ptr::null(),
            initialize: ptr::null(),
            finalize: fn_finalize as *const c_void,
            hasProperty: ptr::null(),
            getProperty: ptr::null(),
            setProperty: ptr::null(),
            deleteProperty: ptr::null(),
            getPropertyNames: ptr::null(),
            callAsFunction: fn_trampoline as *const c_void,
            callAsConstructor: fn_construct_trampoline as *const c_void,
            hasInstance: ptr::null(),
            convertToType: ptr::null(),
        };
        let cls = unsafe { JSClassCreate(&def as *const _ as *const JSClassDefinition) };
        c.set(cls);
        cls
    })
}

unsafe extern "C" fn fn_finalize(object: JSObjectRef) {
    let p = unsafe { JSObjectGetPrivate(object) } as *mut FnBridge;
    if !p.is_null() {
        drop(unsafe { Box::from_raw(p) });
    }
}

/// Invoke a stored v8 callback, building a `CbInfo` for it.
///
/// If the callback throws (via `Isolate::ThrowException` / a recorded pending
/// exception), the thrown value is written to `*out_exc` and the JSC runtime is
/// signalled to propagate it (by the caller returning while `*out_exc` is set).
unsafe fn dispatch(
    ctx: JSContextRef,
    bridge: &FnBridge,
    this: JSValueRef,
    new_target: JSValueRef,
    is_construct: bool,
    argc: usize,
    argv: *const JSValueRef,
    out_exc: *mut JSValueRef,
) -> JSValueRef {
    let mut args = Vec::with_capacity(argc);
    for i in 0..argc {
        args.push(unsafe { *argv.add(i) });
    }
    // Clear any stale pending exception so we only observe one thrown by THIS
    // callback invocation.
    let iso = current_iso();
    crate::shim_core::clear_pending_exception(iso);
    let info = Box::new(CbInfo {
        isolate: iso,
        ctx,
        this,
        data: bridge.data,
        new_target,
        is_construct,
        args,
        return_slot: Box::new(ptr::null()),
    });
    let info_ptr =
        Box::into_raw(info) as *const FunctionCallbackInfo;
    unsafe { (bridge.callback)(info_ptr) };
    // Recover return value.
    let info = unsafe { Box::from_raw(info_ptr as *mut CbInfo) };
    let ret = *info.return_slot;
    // If the callback recorded a pending exception, forward it to JSC.
    let pending = crate::shim_core::peek_pending_exception(iso);
    if !pending.is_null() {
        if !out_exc.is_null() {
            unsafe { *out_exc = pending };
        }
        crate::shim_core::clear_pending_exception(iso);
        return unsafe { JSValueMakeUndefined(ctx) };
    }
    if ret.is_null() {
        unsafe { JSValueMakeUndefined(ctx) }
    } else {
        ret
    }
}

unsafe extern "C" fn fn_trampoline(
    ctx: JSContextRef,
    function: JSObjectRef,
    this_object: JSObjectRef,
    argc: usize,
    argv: *const JSValueRef,
    exception: *mut JSValueRef,
) -> JSValueRef {
    let bridge = unsafe { JSObjectGetPrivate(function) } as *const FnBridge;
    if bridge.is_null() {
        return unsafe { JSValueMakeUndefined(ctx) };
    }
    unsafe {
        dispatch(
            ctx,
            &*bridge,
            this_object as JSValueRef,
            JSValueMakeUndefined(ctx),
            false,
            argc,
            argv,
            exception,
        )
    }
}

unsafe extern "C" fn fn_construct_trampoline(
    ctx: JSContextRef,
    function: JSObjectRef,
    argc: usize,
    argv: *const JSValueRef,
    exception: *mut JSValueRef,
) -> JSObjectRef {
    let bridge = unsafe { JSObjectGetPrivate(function) } as *const FnBridge;
    if bridge.is_null() {
        return unsafe { JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut()) };
    }
    // Fresh `this` object for the constructor, with its prototype set to the
    // constructor function's `.prototype` so instance methods (installed on the
    // prototype template) are reachable, matching `new F()` semantics.
    let this = unsafe { JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut()) };
    unsafe {
        let proto_key = JSStringCreateWithUTF8CString(c"prototype".as_ptr());
        let mut exc: JSValueRef = ptr::null();
        let proto = JSObjectGetProperty(ctx, function, proto_key, &mut exc);
        JSStringRelease(proto_key);
        if !proto.is_null() && JSValueIsObject(ctx, proto) {
            JSObjectSetPrototype(ctx, this, proto);
        }
    }
    let r = unsafe {
        dispatch(
            ctx,
            &*bridge,
            this as JSValueRef,
            function as JSValueRef,
            true,
            argc,
            argv,
            exception,
        )
    };
    // If a constructor callback threw, signal JSC via the exception out-param.
    if !exception.is_null() && !unsafe { *exception }.is_null() {
        return unsafe { JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut()) };
    }
    // If callback returned an object, use it; else use `this`.
    if !r.is_null() && unsafe { JSValueIsObject(ctx, r) } {
        r as JSObjectRef
    } else {
        this
    }
}

/// Create a callable JSC function object carrying the given v8 callback/data.
unsafe fn make_function(
    ctx: JSContextRef,
    callback: FunctionCallback,
    data: JSValueRef,
) -> JSObjectRef {
    unsafe { make_function_len(ctx, callback, data, 0) }
}

/// Like `make_function` but also installs the function's `.length` (arity)
/// property, which JS bootstrap code (e.g. `setUpAsyncStub`) relies on.
unsafe fn make_function_len(
    ctx: JSContextRef,
    callback: FunctionCallback,
    data: JSValueRef,
    length: i32,
) -> JSObjectRef {
    let gctx = unsafe { JSContextGetGlobalContext(ctx) };
    if !data.is_null() {
        unsafe { JSValueProtect(gctx, data) };
    }
    let bridge = Box::new(FnBridge {
        callback,
        data,
        ctx: gctx,
    });
    let obj = unsafe {
        JSObjectMake(ctx, fn_class(), Box::into_raw(bridge) as *mut c_void)
    };
    // Install `length` (arity). Per ES spec a function's `length` is
    // { writable:false, enumerable:false, configurable:true } — i.e. ReadOnly|
    // DontEnum (2|4) but NOT DontDelete, so embedder code (e.g. deno's
    // applyWebIdlInterfaceShape) can redefine it without a JSC "unconfigurable
    // property" TypeError.
    let key = unsafe { JSStringCreateWithUTF8CString(c"length".as_ptr()) };
    let lenval = unsafe { JSValueMakeNumber(ctx, length.max(0) as f64) };
    let mut exc: JSValueRef = ptr::null();
    unsafe {
        JSObjectSetProperty(ctx, obj, key, lenval, 2 | 4, &mut exc);
        JSStringRelease(key);
    }
    // Objects made with a custom JSClass do NOT inherit `Function.prototype`, so
    // they lack `call`/`apply`/`bind`. deno's async-op stubs do
    // `originalOp.call(this, id, ...)`, so splice `Function.prototype` into the
    // function object's prototype chain.
    unsafe {
        let fp = function_prototype(ctx);
        if !fp.is_null() {
            JSObjectSetPrototype(ctx, obj, fp);
        }
    }
    obj
}

/// Return `Function.prototype` for `ctx` (cached per call is cheap; JSC interns
/// the global). Used to give our custom-class function objects `call`/`apply`/
/// `bind`.
unsafe fn function_prototype(ctx: JSContextRef) -> JSValueRef {
    unsafe {
        let global = JSContextGetGlobalObject(ctx);
        let fkey = JSStringCreateWithUTF8CString(c"Function".as_ptr());
        let mut exc: JSValueRef = ptr::null();
        let func_ctor = JSObjectGetProperty(ctx, global, fkey, &mut exc);
        JSStringRelease(fkey);
        if func_ctor.is_null() || !JSValueIsObject(ctx, func_ctor) {
            return ptr::null();
        }
        let pkey = JSStringCreateWithUTF8CString(c"prototype".as_ptr());
        let fp = JSObjectGetProperty(ctx, func_ctor as JSObjectRef, pkey, &mut exc);
        JSStringRelease(pkey);
        fp
    }
}

#[inline]
fn cbinfo<'a>(this: *const FunctionCallbackInfo) -> &'a mut CbInfo {
    unsafe { &mut *(this as *mut CbInfo) }
}

// ===================================================================
// External
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__External__New(
    isolate: *mut RealIsolate,
    value: *mut c_void,
) -> *const External {
    let st = iso_state(isolate);
    let ctx = st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
    if ctx.is_null() {
        return ptr::null();
    }
    // Wrap the raw pointer as a JS number is lossy on 64-bit; instead store it
    // as private data on a plain object so External::Value can recover it.
    let obj = unsafe { JSObjectMake(ctx, ext_class(), value) };
    intern_ctx::<External>(ctx, obj as JSValueRef)
}

thread_local! {
    static EXT_CLASS: std::cell::Cell<JSClassRef> = const { std::cell::Cell::new(ptr::null_mut()) };
}

fn ext_class() -> JSClassRef {
    EXT_CLASS.with(|c| {
        let existing = c.get();
        if !existing.is_null() {
            return existing;
        }
        let def = FnJSClassDefinition {
            version: 0,
            attributes: 0,
            className: c"v8jsc_external".as_ptr(),
            parentClass: ptr::null_mut(),
            staticValues: ptr::null(),
            staticFunctions: ptr::null(),
            initialize: ptr::null(),
            finalize: ptr::null(),
            hasProperty: ptr::null(),
            getProperty: ptr::null(),
            setProperty: ptr::null(),
            deleteProperty: ptr::null(),
            getPropertyNames: ptr::null(),
            callAsFunction: ptr::null(),
            callAsConstructor: ptr::null(),
            hasInstance: ptr::null(),
            convertToType: ptr::null(),
        };
        let cls = unsafe { JSClassCreate(&def as *const _ as *const JSClassDefinition) };
        c.set(cls);
        cls
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__External__Value(this: *const External) -> *mut c_void {
    if this.is_null() {
        return ptr::null_mut();
    }
    unsafe { JSObjectGetPrivate(jsval(this) as JSObjectRef) }
}

/// Whether `v` is one of our `External` objects (used by `v8__Value__IsExternal`
/// and the `Object` introspection shims). Reports false for null/non-objects.
pub(crate) fn value_is_external(v: JSValueRef) -> bool {
    if v.is_null() {
        return false;
    }
    let ctx = current_ctx();
    if ctx.is_null() {
        return false;
    }
    unsafe { JSValueIsObjectOfClass(ctx, v, ext_class()) }
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
    let _ = (constructor_behavior, side_effect_type);
    let ctx = ctx_of(context) as JSContextRef;
    if ctx.is_null() {
        return ptr::null();
    }
    let f =
        unsafe { make_function_len(ctx, callback, jsval(data_or_null), length) };
    intern_ctx::<Function>(ctx, f as JSValueRef)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__Call(
    this: *const Function,
    context: *const Context,
    recv: *const Value,
    argc: crate::support::int,
    argv: *const *const Value,
) -> *const Value {
    let ctx = ctx_of(context) as JSContextRef;
    if ctx.is_null() || this.is_null() {
        return ptr::null();
    }
    let func = jsval(this) as JSObjectRef;
    let recv_obj = if recv.is_null() {
        ptr::null_mut()
    } else {
        jsval(recv) as JSObjectRef
    };
    let n = argc.max(0) as usize;
    let mut args: Vec<JSValueRef> = Vec::with_capacity(n);
    for i in 0..n {
        let p = unsafe { *argv.add(i) };
        args.push(jsval(p));
    }
    let mut exc: JSValueRef = ptr::null();
    let r = unsafe {
        JSObjectCallAsFunction(
            ctx,
            func,
            recv_obj,
            n,
            args.as_ptr(),
            &mut exc,
        )
    };
    if r.is_null() {
        if !exc.is_null() && std::env::var("V82JSC_DEBUG").is_ok() {
            unsafe {
                let s = JSValueToStringCopy(ctx, exc, ptr::null_mut());
                if !s.is_null() {
                    let max = JSStringGetMaximumUTF8CStringSize(s);
                    let mut buf = vec![0u8; max];
                    let n = JSStringGetUTF8CString(s, buf.as_mut_ptr() as *mut _, max);
                    JSStringRelease(s);
                    buf.truncate(n.saturating_sub(1));
                    eprintln!("[v82jsc] Function__Call threw: {}", std::string::String::from_utf8_lossy(&buf));
                }
            }
        }
        // Record the thrown value as the isolate's pending exception so a
        // surrounding TryCatch (and deno's error reporting) can observe it.
        if !exc.is_null() {
            crate::shim_core::record_pending_exception(ctx, exc);
        }
        return ptr::null();
    }
    intern_ctx::<Value>(ctx, r)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__NewInstance(
    this: *const Function,
    context: *const Context,
    argc: crate::support::int,
    argv: *const *const Value,
) -> *const Object {
    let ctx = ctx_of(context) as JSContextRef;
    if ctx.is_null() || this.is_null() {
        return ptr::null();
    }
    let func = jsval(this) as JSObjectRef;
    let n = argc.max(0) as usize;
    let mut args: Vec<JSValueRef> = Vec::with_capacity(n);
    for i in 0..n {
        args.push(jsval(unsafe { *argv.add(i) }));
    }
    let mut exc: JSValueRef = ptr::null();
    let r = unsafe {
        JSObjectCallAsConstructor(ctx, func, n, args.as_ptr(), &mut exc)
    };
    if r.is_null() {
        return ptr::null();
    }
    intern_ctx::<Object>(ctx, r as JSValueRef)
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
    let obj = jsval(this) as JSObjectRef;
    let key = unsafe { JSStringCreateWithUTF8CString(c"name".as_ptr()) };
    let mut exc: JSValueRef = ptr::null();
    // Read-only/dontenum (1|4) per JSPropertyAttributes; use 0 to be lenient.
    unsafe {
        JSObjectSetProperty(ctx, obj, key, jsval(name), 0, &mut exc);
        JSStringRelease(key);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__CreateCodeCache(
    script: *const Function,
) -> *mut crate::CachedData<'static> {
    // JSC has no code-cache serialization exposed via the C API.
    // TODO(v82jsc): no JSC equivalent for code cache creation.
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
    let slot = &mut *info.return_slot as *mut JSValueRef;
    RawFunctionCallbackInfoParts {
        isolate: info.isolate,
        return_value: slot as usize,
        data: info.data as *const Value,
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
    if info.data.is_null() {
        return (unsafe { JSValueMakeUndefined(info.ctx) }) as *const Value;
    }
    info.data as *const Value
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__This(
    this: *const FunctionCallbackInfo,
) -> *const Object {
    if this.is_null() {
        return ptr::null();
    }
    let info = cbinfo(this);
    info.this as *const Object
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
        return (unsafe { JSValueMakeUndefined(info.ctx) }) as *const Value;
    }
    match info.args.get(index as usize) {
        Some(&v) => v as *const Value,
        None => (unsafe { JSValueMakeUndefined(info.ctx) }) as *const Value,
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
    (&mut *info.return_slot as *mut JSValueRef) as usize
}

// ===================================================================
// ReturnValue — `this` is `*mut RawReturnValue`, holding a usize that is a
// pointer to our JSValueRef return slot.
// ===================================================================

#[inline]
unsafe fn rv_slot(this: *mut RawReturnValue) -> *mut JSValueRef {
    if this.is_null() {
        return ptr::null_mut();
    }
    unsafe { (*this).0 as *mut JSValueRef }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Set(
    this: *mut RawReturnValue,
    value: *const Value,
) {
    let slot = unsafe { rv_slot(this) };
    if !slot.is_null() {
        unsafe { *slot = jsval(value) };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Set__Bool(
    this: *mut RawReturnValue,
    value: bool,
) {
    let slot = unsafe { rv_slot(this) };
    if !slot.is_null() {
        let v = unsafe { JSValueMakeBoolean(current_ctx(), value) };
        unsafe { *slot = v };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Set__Int32(
    this: *mut RawReturnValue,
    value: i32,
) {
    let slot = unsafe { rv_slot(this) };
    if !slot.is_null() {
        let v = unsafe { JSValueMakeNumber(current_ctx(), value as f64) };
        unsafe { *slot = v };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Set__Uint32(
    this: *mut RawReturnValue,
    value: u32,
) {
    let slot = unsafe { rv_slot(this) };
    if !slot.is_null() {
        let v = unsafe { JSValueMakeNumber(current_ctx(), value as f64) };
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
        let v = unsafe { JSValueMakeNumber(current_ctx(), value) };
        unsafe { *slot = v };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__SetNull(this: *mut RawReturnValue) {
    let slot = unsafe { rv_slot(this) };
    if !slot.is_null() {
        let v = unsafe { JSValueMakeNull(current_ctx()) };
        unsafe { *slot = v };
    }
}

// ===================================================================
// Template (base) — Set a property on a template's property list.
// `*const Template` aliases either a FnTemplate or ObjTemplate; both start
// with their own data, so we can't blindly cast. We store template props in a
// side table keyed by template pointer. Simpler: both template structs keep a
// `props` Vec at a known place? They don't share layout, so use a registry.
// ===================================================================

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
    // Both FnTemplate and ObjTemplate are registered so we can find their
    // props vec. We try function-template first, then object-template.
    let raw = this as *const c_void as usize;
    with_template_props(raw, |props| {
        props.push((jsval(key), jsval(value), attr.as_u32_lenient()));
    });
}

// Helper trait to read PropertyAttribute as u32 without depending on private
// accessors. PropertyAttribute is repr(transparent) over u32.
trait AttrU32 {
    fn as_u32_lenient(&self) -> u32;
}
impl AttrU32 for PropertyAttribute {
    fn as_u32_lenient(&self) -> u32 {
        // SAFETY: PropertyAttribute is #[repr(C)] struct PropertyAttribute(u32).
        unsafe { *(self as *const PropertyAttribute as *const u32) }
    }
}

/// Registry tagging each template pointer as Fn or Obj so Template::Set can
/// find the right `props` vec.
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

/// Whether `p` is one of our `FunctionTemplate` / `ObjectTemplate` handles.
///
/// These are *not* JSC values — they are pointers to Rust-owned `FnTemplate` /
/// `ObjTemplate` boxes. Callers that would otherwise treat a `*const Data` as a
/// `JSValueRef` (e.g. `Global::new`, which `JSValueProtect`s its argument) must
/// special-case them: protecting a Rust box pointer poisons JSC's
/// protected-values root set and crashes GC ("INVALID HANDLE", blockVM=0x8).
pub(crate) fn is_template_ptr(p: *const c_void) -> bool {
    if p.is_null() {
        return false;
    }
    TEMPLATES.with(|t| t.borrow().contains_key(&(p as usize)))
}

fn with_template_props(p: usize, f: impl FnOnce(&mut Vec<(JSValueRef, JSValueRef, u32)>)) {
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
        data: jsval(data_or_null),
        length,
        class_name: None,
        proto,
        instance,
        parent: ptr::null(),
        props: Vec::new(),
        cached_proto: ptr::null(),
    }));
    // Wire the instance template back to its FunctionTemplate so NewInstance can
    // install the function's prototype (carrying prototype-template methods) as
    // the created object's `__proto__`.
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
    let ctx = ctx_of(context) as JSContextRef;
    if ctx.is_null() {
        return ptr::null();
    }
    let t = unsafe { &*(this as *const FnTemplate) };
    let f = unsafe { make_function_len(ctx, t.callback, t.data, t.length) };

    // Apply class name as the function's `name`.
    if let Some(name) = &t.class_name {
        if let Ok(cname) = std::ffi::CString::new(name.as_str()) {
            let key = unsafe { JSStringCreateWithUTF8CString(c"name".as_ptr()) };
            let nameval = unsafe {
                let s = JSStringCreateWithUTF8CString(cname.as_ptr());
                let v = JSValueMakeString(ctx, s);
                JSStringRelease(s);
                v
            };
            let mut exc: JSValueRef = ptr::null();
            unsafe {
                JSObjectSetProperty(ctx, f, key, nameval, 0, &mut exc);
                JSStringRelease(key);
            }
        }
    }

    // Apply template properties (static props) directly onto the function.
    apply_props(ctx, f, &t.props);

    // Build a prototype object carrying prototype-template props and install it.
    // v8 functions ALWAYS have a `.prototype` object (deno's setUpAsyncStub does
    // `tmplFn.prototype[opName] = fn`), so install one even when the prototype
    // template has no props.
    let proto_obj = unsafe { build_prototype_object(ctx, this as *const FnTemplate) };
    let key = unsafe { JSStringCreateWithUTF8CString(c"prototype".as_ptr()) };
    let mut exc: JSValueRef = ptr::null();
    unsafe {
        JSObjectSetProperty(ctx, f, key, proto_obj as JSValueRef, 0, &mut exc);
        JSStringRelease(key);
    }

    intern_ctx::<Function>(ctx, f as JSValueRef)
}

/// Return the `.prototype` object for a FunctionTemplate, creating it once and
/// caching it on the template. The object carries the prototype-template's props
/// (methods/accessors). If the FunctionTemplate `Inherit`s a parent, the parent's
/// prototype object is spliced in as this prototype's `__proto__`, so inherited
/// methods are reachable up the chain (matching V8's prototype inheritance).
///
/// Caching is essential: `GetFunction` installs this object as the function's
/// `.prototype` (and JS code then attaches methods to it), while `NewInstance`
/// wires it as a created instance's `__proto__`. Both must reference the SAME
/// object or instances would miss methods.
unsafe fn build_prototype_object(ctx: JSContextRef, tp: *const FnTemplate) -> JSObjectRef {
    let t = unsafe { &mut *(tp as *mut FnTemplate) };
    if !t.cached_proto.is_null() {
        return t.cached_proto as JSObjectRef;
    }
    let proto_obj = unsafe { JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut()) };
    let proto = unsafe { &*t.proto };
    if !proto.props.is_empty() {
        apply_props(ctx, proto_obj, &proto.props);
    }
    apply_accessors(ctx, proto_obj, &proto.accessors);
    // Splice the parent template's prototype into this one's chain.
    if !t.parent.is_null() {
        let parent_proto = unsafe { build_prototype_object(ctx, t.parent) };
        unsafe { JSObjectSetPrototype(ctx, proto_obj, parent_proto as JSValueRef) };
    }
    // Protect for the template's lifetime and cache.
    let gctx = unsafe { JSContextGetGlobalContext(ctx) };
    unsafe { JSValueProtect(gctx, proto_obj as JSValueRef) };
    t.cached_proto = proto_obj as JSValueRef;
    proto_obj
}

fn apply_props(
    ctx: JSContextRef,
    obj: JSObjectRef,
    props: &[(JSValueRef, JSValueRef, u32)],
) {
    for &(key, value, attr) in props {
        if key.is_null() {
            continue;
        }
        let mut exc: JSValueRef = ptr::null();
        let keystr = unsafe { JSValueToStringCopy(ctx, key, &mut exc) };
        if keystr.is_null() {
            continue;
        }
        // A template property value may itself be a nested FunctionTemplate /
        // ObjectTemplate (e.g. a method template set on a prototype template).
        // Those are Rust box pointers, NOT JS values — storing one as a property
        // would put a non-cell pointer into the JSC heap and crash the GC.
        // Materialize templates into real JS objects before installing them.
        let value = materialize_template_value(ctx, value);
        unsafe {
            JSObjectSetProperty(ctx, obj, keystr, value, attr, &mut exc);
            JSStringRelease(keystr);
        }
    }
}

/// If `value` is one of our template handles, instantiate it into a real JS
/// value (a function for a FunctionTemplate, an object for an ObjectTemplate);
/// otherwise return it unchanged.
fn materialize_template_value(ctx: JSContextRef, value: JSValueRef) -> JSValueRef {
    if value.is_null() {
        return value;
    }
    let raw = value as *const c_void as usize;
    let kind = TEMPLATES.with(|t| t.borrow().get(&raw).copied());
    match kind {
        Some(TemplKind::Func) => {
            let f = v8__FunctionTemplate__GetFunction(
                value as *const FunctionTemplate,
                ctx as *const Context,
            );
            if f.is_null() {
                value
            } else {
                jsval(f)
            }
        }
        Some(TemplKind::Obj) => {
            let o = v8__ObjectTemplate__NewInstance(
                value as *const ObjectTemplate,
                ctx as *const Context,
            );
            if o.is_null() {
                value
            } else {
                jsval(o)
            }
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
    let mut exc: JSValueRef = ptr::null();
    let s = unsafe { JSValueToStringCopy(ctx, jsval(name), &mut exc) };
    if s.is_null() {
        return;
    }
    let max = unsafe { JSStringGetMaximumUTF8CStringSize(s) };
    let mut buf = vec![0u8; max];
    let n = unsafe {
        JSStringGetUTF8CString(s, buf.as_mut_ptr() as *mut c_char, max)
    };
    unsafe { JSStringRelease(s) };
    if n > 0 {
        // n includes the trailing NUL.
        buf.truncate(n - 1);
        if let Ok(name) = std::string::String::from_utf8(buf) {
            t.class_name = Some(name);
        }
    }
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
    let ctx = ctx_of(context) as JSContextRef;
    if ctx.is_null() {
        return ptr::null();
    }
    let obj = unsafe { JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut()) };
    if !this.is_null() {
        let t = unsafe { &*(this as *const ObjTemplate) };
        // If this is the instance template of a FunctionTemplate, set the created
        // object's prototype to the function's `.prototype` object so the
        // prototype-template methods (e.g. crypto.getRandomValues) are reachable.
        // This mirrors V8, where `instance_template.new_instance()` yields an
        // object whose `__proto__` is the FunctionTemplate's prototype.
        if !t.parent_fn.is_null() {
            let proto_obj = unsafe { build_prototype_object(ctx, t.parent_fn) };
            unsafe { JSObjectSetPrototype(ctx, obj, proto_obj as JSValueRef) };
        }
        // Instance-template props go directly on the instance.
        apply_props(ctx, obj, &t.props);
        apply_accessors(ctx, obj, &t.accessors);
    }
    intern_ctx::<Object>(ctx, obj as JSValueRef)
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
        key: jsval(key),
        getter: getter as *const FnTemplate,
        setter: setter as *const FnTemplate,
        attr: attr.as_u32_lenient(),
    });
}

/// Install template-declared accessors onto a live object via
/// `Object.defineProperty(obj, key, { get, set, enumerable, configurable })`.
/// Getters/setters are FunctionTemplate handles materialized into JS functions.
fn apply_accessors(ctx: JSContextRef, obj: JSObjectRef, accessors: &[TemplAccessor]) {
    if accessors.is_empty() {
        return;
    }
    unsafe {
        // Resolve Object.defineProperty once.
        let global = JSContextGetGlobalObject(ctx);
        let okey = JSStringCreateWithUTF8CString(c"Object".as_ptr());
        let mut exc: JSValueRef = ptr::null();
        let object_ctor = JSObjectGetProperty(ctx, global, okey, &mut exc);
        JSStringRelease(okey);
        if object_ctor.is_null() || !JSValueIsObject(ctx, object_ctor) {
            return;
        }
        let dpkey = JSStringCreateWithUTF8CString(c"defineProperty".as_ptr());
        let define = JSObjectGetProperty(ctx, object_ctor as JSObjectRef, dpkey, &mut exc);
        JSStringRelease(dpkey);
        if define.is_null() || !JSValueIsObject(ctx, define) {
            return;
        }

        let get_str = JSStringCreateWithUTF8CString(c"get".as_ptr());
        let set_str = JSStringCreateWithUTF8CString(c"set".as_ptr());
        let enum_str = JSStringCreateWithUTF8CString(c"enumerable".as_ptr());
        let conf_str = JSStringCreateWithUTF8CString(c"configurable".as_ptr());

        for acc in accessors {
            if acc.key.is_null() {
                continue;
            }
            let desc = JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut());
            if !acc.getter.is_null() {
                let gf = v8__FunctionTemplate__GetFunction(
                    acc.getter as *const FunctionTemplate,
                    ctx as *const Context,
                );
                if !gf.is_null() {
                    JSObjectSetProperty(ctx, desc, get_str, jsval(gf), 0, &mut exc);
                }
            }
            if !acc.setter.is_null() {
                let sf = v8__FunctionTemplate__GetFunction(
                    acc.setter as *const FunctionTemplate,
                    ctx as *const Context,
                );
                if !sf.is_null() {
                    JSObjectSetProperty(ctx, desc, set_str, jsval(sf), 0, &mut exc);
                }
            }
            // PropertyAttribute bits: DontEnum=2, DontDelete=4 (v8). Map to
            // enumerable/configurable for the descriptor.
            let enumerable = (acc.attr & 2) == 0;
            let configurable = (acc.attr & 4) == 0;
            JSObjectSetProperty(
                ctx, desc, enum_str,
                JSValueMakeBoolean(ctx, enumerable), 0, &mut exc,
            );
            JSObjectSetProperty(
                ctx, desc, conf_str,
                JSValueMakeBoolean(ctx, configurable), 0, &mut exc,
            );

            let args = [obj as JSValueRef, acc.key, desc as JSValueRef];
            JSObjectCallAsFunction(
                ctx,
                define as JSObjectRef,
                object_ctor as JSObjectRef,
                3,
                args.as_ptr(),
                &mut exc,
            );
        }

        JSStringRelease(get_str);
        JSStringRelease(set_str);
        JSStringRelease(enum_str);
        JSStringRelease(conf_str);
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
    // TODO(v82jsc): indexed interceptors require a custom JSClass with
    // getProperty/setProperty hooks decoding integer keys; not yet bridged.
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
    // TODO(v82jsc): named interceptors require a custom JSClass with property
    // hooks bridged to v8 PropertyCallbackInfo; not yet bridged.
    let _ = (
        this, getter, setter, query, deleter, enumerator, definer, descriptor,
        data_or_null, flags,
    );
}
