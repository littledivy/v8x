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
// Extra JSC C API we need (declared locally; jsc_sys.rs is owned by others).
// ===================================================================

#[repr(C)]
struct JSClassDefinition {
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

type JSObjectCallAsFunctionCallback = unsafe extern "C" fn(
    ctx: JSContextRef,
    function: JSObjectRef,
    thisObject: JSObjectRef,
    argumentCount: usize,
    arguments: *const JSValueRef,
    exception: *mut JSValueRef,
) -> JSValueRef;

unsafe extern "C" {
    fn JSClassCreate(definition: *const JSClassDefinition) -> JSClassRef;
    fn JSObjectMake(
        ctx: JSContextRef,
        jsClass: JSClassRef,
        data: *mut c_void,
    ) -> JSObjectRef;
    fn JSObjectGetPrivate(object: JSObjectRef) -> *mut c_void;
    fn JSObjectSetPrivate(object: JSObjectRef, data: *mut c_void) -> bool;
    fn JSObjectIsFunction(ctx: JSContextRef, object: JSObjectRef) -> bool;
    fn JSObjectCallAsFunction(
        ctx: JSContextRef,
        object: JSObjectRef,
        thisObject: JSObjectRef,
        argumentCount: usize,
        arguments: *const JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSValueRef;
    fn JSObjectCallAsConstructor(
        ctx: JSContextRef,
        object: JSObjectRef,
        argumentCount: usize,
        arguments: *const JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSObjectRef;
    fn JSObjectMakeConstructor(
        ctx: JSContextRef,
        jsClass: JSClassRef,
        callAsConstructor: *const c_void,
    ) -> JSObjectRef;
    fn JSObjectSetProperty(
        ctx: JSContextRef,
        object: JSObjectRef,
        propertyName: JSStringRef,
        value: JSValueRef,
        attributes: u32,
        exception: *mut JSValueRef,
    );
    fn JSObjectGetProperty(
        ctx: JSContextRef,
        object: JSObjectRef,
        propertyName: JSStringRef,
        exception: *mut JSValueRef,
    ) -> JSValueRef;
    fn JSObjectMakeFunctionWithCallback(
        ctx: JSContextRef,
        name: JSStringRef,
        callAsFunction: JSObjectCallAsFunctionCallback,
    ) -> JSObjectRef;
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
}

/// ObjectTemplate config object.
struct ObjTemplate {
    internal_field_count: i32,
    props: Vec<(JSValueRef, JSValueRef, u32)>,
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
        let def = JSClassDefinition {
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
        let cls = unsafe { JSClassCreate(&def) };
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
unsafe fn dispatch(
    ctx: JSContextRef,
    bridge: &FnBridge,
    this: JSValueRef,
    new_target: JSValueRef,
    is_construct: bool,
    argc: usize,
    argv: *const JSValueRef,
) -> JSValueRef {
    let mut args = Vec::with_capacity(argc);
    for i in 0..argc {
        args.push(unsafe { *argv.add(i) });
    }
    let info = Box::new(CbInfo {
        isolate: current_iso(),
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
    _exception: *mut JSValueRef,
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
        )
    }
}

unsafe extern "C" fn fn_construct_trampoline(
    ctx: JSContextRef,
    function: JSObjectRef,
    argc: usize,
    argv: *const JSValueRef,
    _exception: *mut JSValueRef,
) -> JSObjectRef {
    let bridge = unsafe { JSObjectGetPrivate(function) } as *const FnBridge;
    if bridge.is_null() {
        return unsafe { JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut()) };
    }
    // Fresh `this` object for the constructor.
    let this = unsafe { JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut()) };
    let r = unsafe {
        dispatch(
            ctx,
            &*bridge,
            this as JSValueRef,
            function as JSValueRef,
            true,
            argc,
            argv,
        )
    };
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
    // Install `length` (arity). ReadOnly|DontEnum|DontDelete == 2|4|8.
    let key = unsafe { JSStringCreateWithUTF8CString(c"length".as_ptr()) };
    let lenval = unsafe { JSValueMakeNumber(ctx, length.max(0) as f64) };
    let mut exc: JSValueRef = ptr::null();
    unsafe {
        JSObjectSetProperty(ctx, obj, key, lenval, 2 | 4 | 8, &mut exc);
        JSStringRelease(key);
    }
    obj
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
        let def = JSClassDefinition {
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
        let cls = unsafe { JSClassCreate(&def) };
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

unsafe extern "C" {
    fn JSValueIsObjectOfClass(
        ctx: JSContextRef,
        value: JSValueRef,
        js_class: JSClassRef,
    ) -> bool;
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
    }));
    let instance = Box::into_raw(Box::new(ObjTemplate {
        internal_field_count: 0,
        props: Vec::new(),
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
    }));
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
    let proto = unsafe { &*t.proto };
    if !proto.props.is_empty() {
        let proto_obj = unsafe { JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut()) };
        apply_props(ctx, proto_obj, &proto.props);
        let key =
            unsafe { JSStringCreateWithUTF8CString(c"prototype".as_ptr()) };
        let mut exc: JSValueRef = ptr::null();
        unsafe {
            JSObjectSetProperty(ctx, f, key, proto_obj as JSValueRef, 0, &mut exc);
            JSStringRelease(key);
        }
    }

    intern_ctx::<Function>(ctx, f as JSValueRef)
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
        unsafe {
            JSObjectSetProperty(ctx, obj, keystr, value, attr, &mut exc);
            JSStringRelease(keystr);
        }
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
        apply_props(ctx, obj, &t.props);
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
    // TODO(v82jsc): accessor properties on object templates not yet bridged
    // (would require defineProperty with getter/setter at instantiation).
    let _ = (this, key, getter, setter, attr);
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
