// Family: "value" — v8::Value predicates (Is*), conversions (To*, *Value),
// and comparisons (StrictEquals). JSC-backed definitions.
#![allow(non_snake_case, unused)]

use crate::jsc_sys::*;
use crate::support::Maybe;
use crate::{
    BigInt, Boolean, Context, Int32, Integer, Number, Object, RealIsolate, String as V8String,
    Uint32, Value,
};
use crate::shim_core::{ctx_of, current_ctx, intern, intern_ctx, jsval};
use std::os::raw::{c_char, c_void};
use std::ptr;

// ---------------------------------------------------------------------------
// Extra JSC C API functions not declared in jsc_sys.rs.
// ---------------------------------------------------------------------------
#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JSTypedArrayType {
    Int8Array = 0,
    Int16Array = 1,
    Int32Array = 2,
    Uint8Array = 3,
    Uint8ClampedArray = 4,
    Uint16Array = 5,
    Uint32Array = 6,
    Float32Array = 7,
    Float64Array = 8,
    ArrayBuffer = 9,
    None = 10,
    BigInt64Array = 11,
    BigUint64Array = 12,
}

unsafe extern "C" {
    fn JSValueToObject(
        ctx: JSContextRef,
        value: JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSObjectRef;
    fn JSValueIsObjectOfClass(ctx: JSContextRef, value: JSValueRef, jsClass: JSClassRef) -> bool;
    fn JSValueIsInstanceOfConstructor(
        ctx: JSContextRef,
        value: JSValueRef,
        constructor: JSObjectRef,
        exception: *mut JSValueRef,
    ) -> bool;
    fn JSObjectIsFunction(ctx: JSContextRef, object: JSObjectRef) -> bool;
    fn JSValueGetTypedArrayType(
        ctx: JSContextRef,
        object: JSObjectRef,
        exception: *mut JSValueRef,
    ) -> JSTypedArrayType;
    fn JSObjectGetProperty(
        ctx: JSContextRef,
        object: JSObjectRef,
        propertyName: JSStringRef,
        exception: *mut JSValueRef,
    ) -> JSValueRef;
    fn JSObjectCallAsFunction(
        ctx: JSContextRef,
        object: JSObjectRef,
        thisObject: JSObjectRef,
        argumentCount: usize,
        arguments: *const JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSValueRef;
    fn JSValueIsStrictEqual(ctx: JSContextRef, a: JSValueRef, b: JSValueRef) -> bool;
    fn JSValueIsSymbol(ctx: JSContextRef, value: JSValueRef) -> bool;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[inline]
fn ctx() -> JSContextRef {
    current_ctx()
}

#[inline]
fn jsty(v: *const Value) -> JSType {
    let c = ctx();
    if c.is_null() {
        return JSType::Undefined;
    }
    unsafe { JSValueGetType(c, jsval(v)) }
}

#[inline]
fn is_obj(v: *const Value) -> bool {
    let c = ctx();
    !c.is_null() && unsafe { JSValueIsObject(c, jsval(v)) }
}

/// Mirror of `support::Maybe<T>` (`#[repr(C)] { has_value: bool, value: T }`)
/// so we can construct/write it without touching its private fields.
#[repr(C)]
struct MaybeMirror<T> {
    has_value: bool,
    value: T,
}

/// Write a populated Maybe<T> (has_value=true, value=val) at `out`.
#[inline]
unsafe fn maybe_set<T: Copy>(out: *mut Maybe<T>, val: T) {
    if out.is_null() {
        return;
    }
    ptr::write(
        out as *mut MaybeMirror<T>,
        MaybeMirror {
            has_value: true,
            value: val,
        },
    );
}

#[inline]
unsafe fn maybe_none<T: Copy + Default>(out: *mut Maybe<T>) {
    if out.is_null() {
        return;
    }
    ptr::write(
        out as *mut MaybeMirror<T>,
        MaybeMirror {
            has_value: false,
            value: T::default(),
        },
    );
}

/// `Object.prototype.toString.call(v)` -> tag string like "[object Date]".
/// Returns true if the resulting tag equals `tag` (e.g. "Date").
fn class_tag_is(v: *const Value, tag: &str) -> bool {
    let c = ctx();
    if c.is_null() || !is_obj(v) {
        return false;
    }
    unsafe {
        let global = JSContextGetGlobalObject(c);
        let mut exc: JSValueRef = ptr::null();
        // Object
        let name = JSStringCreateWithUTF8CString(b"Object\0".as_ptr() as *const c_char);
        let obj_ctor = JSObjectGetProperty(c, global, name, &mut exc);
        JSStringRelease(name);
        if obj_ctor.is_null() {
            return false;
        }
        let obj_ctor_o = JSValueToObject(c, obj_ctor, &mut exc);
        if obj_ctor_o.is_null() {
            return false;
        }
        // .prototype
        let pname = JSStringCreateWithUTF8CString(b"prototype\0".as_ptr() as *const c_char);
        let proto = JSObjectGetProperty(c, obj_ctor_o, pname, &mut exc);
        JSStringRelease(pname);
        let proto_o = JSValueToObject(c, proto, &mut exc);
        if proto_o.is_null() {
            return false;
        }
        // .toString
        let tsname = JSStringCreateWithUTF8CString(b"toString\0".as_ptr() as *const c_char);
        let ts = JSObjectGetProperty(c, proto_o, tsname, &mut exc);
        JSStringRelease(tsname);
        let ts_o = JSValueToObject(c, ts, &mut exc);
        if ts_o.is_null() {
            return false;
        }
        // call with this = v
        let this_o = JSValueToObject(c, jsval(v), &mut exc);
        let result = JSObjectCallAsFunction(c, ts_o, this_o, 0, ptr::null(), &mut exc);
        if result.is_null() {
            return false;
        }
        let s = JSValueToStringCopy(c, result, &mut exc);
        if s.is_null() {
            return false;
        }
        let max = JSStringGetMaximumUTF8CStringSize(s);
        let mut buf = vec![0u8; max];
        let n = JSStringGetUTF8CString(s, buf.as_mut_ptr() as *mut c_char, max);
        JSStringRelease(s);
        let got = std::ffi::CStr::from_ptr(buf.as_ptr() as *const c_char)
            .to_string_lossy()
            .into_owned();
        got == format!("[object {}]", tag)
    }
}

/// Check `v instanceof global[ctor_name]`.
fn instance_of_global(v: *const Value, ctor_name: &[u8]) -> bool {
    let c = ctx();
    if c.is_null() || !is_obj(v) {
        return false;
    }
    unsafe {
        let global = JSContextGetGlobalObject(c);
        let mut exc: JSValueRef = ptr::null();
        let name = JSStringCreateWithUTF8CString(ctor_name.as_ptr() as *const c_char);
        let ctor = JSObjectGetProperty(c, global, name, &mut exc);
        JSStringRelease(name);
        if ctor.is_null() || !JSValueIsObject(c, ctor) {
            return false;
        }
        let ctor_o = JSValueToObject(c, ctor, &mut exc);
        if ctor_o.is_null() {
            return false;
        }
        JSValueIsInstanceOfConstructor(c, jsval(v), ctor_o, &mut exc)
    }
}

#[inline]
fn typed_array_type(v: *const Value) -> JSTypedArrayType {
    let c = ctx();
    if c.is_null() || !is_obj(v) {
        return JSTypedArrayType::None;
    }
    unsafe {
        let mut exc: JSValueRef = ptr::null();
        let o = JSValueToObject(c, jsval(v), &mut exc);
        if o.is_null() {
            return JSTypedArrayType::None;
        }
        JSValueGetTypedArrayType(c, o, &mut exc)
    }
}

// ===========================================================================
// Primitive type predicates (real JSC support)
// ===========================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUndefined(this: *const Value) -> bool {
    jsty(this) == JSType::Undefined
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsNull(this: *const Value) -> bool {
    jsty(this) == JSType::Null
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsNullOrUndefined(this: *const Value) -> bool {
    let t = jsty(this);
    t == JSType::Null || t == JSType::Undefined
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsTrue(this: *const Value) -> bool {
    let c = ctx();
    if c.is_null() {
        return false;
    }
    unsafe { JSValueIsBoolean(c, jsval(this)) && JSValueToBoolean(c, jsval(this)) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsString(this: *const Value) -> bool {
    jsty(this) == JSType::String
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsSymbol(this: *const Value) -> bool {
    jsty(this) == JSType::Symbol
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsObject(this: *const Value) -> bool {
    is_obj(this)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsBigInt(this: *const Value) -> bool {
    jsty(this) == JSType::BigInt
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsBoolean(this: *const Value) -> bool {
    jsty(this) == JSType::Boolean
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsNumber(this: *const Value) -> bool {
    jsty(this) == JSType::Number
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsInt32(this: *const Value) -> bool {
    let c = ctx();
    if c.is_null() || !v8__Value__IsNumber(this) {
        return false;
    }
    let mut exc: JSValueRef = ptr::null();
    let n = unsafe { JSValueToNumber(c, jsval(this), &mut exc) };
    n.is_finite() && n.fract() == 0.0 && n >= i32::MIN as f64 && n <= i32::MAX as f64
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUint32(this: *const Value) -> bool {
    let c = ctx();
    if c.is_null() || !v8__Value__IsNumber(this) {
        return false;
    }
    let mut exc: JSValueRef = ptr::null();
    let n = unsafe { JSValueToNumber(c, jsval(this), &mut exc) };
    n.is_finite() && n.fract() == 0.0 && n >= 0.0 && n <= u32::MAX as f64
}

// ===========================================================================
// Array / Function predicates (real JSC support)
// ===========================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsArray(this: *const Value) -> bool {
    let c = ctx();
    !c.is_null() && unsafe { JSValueIsArray(c, jsval(this)) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsFunction(this: *const Value) -> bool {
    let c = ctx();
    if c.is_null() || !is_obj(this) {
        return false;
    }
    unsafe {
        let mut exc: JSValueRef = ptr::null();
        let o = JSValueToObject(c, jsval(this), &mut exc);
        !o.is_null() && JSObjectIsFunction(c, o)
    }
}

// ===========================================================================
// Object-subtype predicates via Object.prototype.toString tag / instanceof
// ===========================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsDate(this: *const Value) -> bool {
    class_tag_is(this, "Date")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsArgumentsObject(this: *const Value) -> bool {
    class_tag_is(this, "Arguments")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsBigIntObject(this: *const Value) -> bool {
    class_tag_is(this, "BigInt") && is_obj(this) && jsty(this) != JSType::BigInt
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsBooleanObject(this: *const Value) -> bool {
    class_tag_is(this, "Boolean")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsNumberObject(this: *const Value) -> bool {
    class_tag_is(this, "Number")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsStringObject(this: *const Value) -> bool {
    class_tag_is(this, "String")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsSymbolObject(this: *const Value) -> bool {
    class_tag_is(this, "Symbol")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsNativeError(this: *const Value) -> bool {
    instance_of_global(this, b"Error\0")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsRegExp(this: *const Value) -> bool {
    class_tag_is(this, "RegExp")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsAsyncFunction(this: *const Value) -> bool {
    // TODO(v82jsc): JSC has no C API to distinguish async functions; approximate
    // by AsyncFunction tag, else false.
    class_tag_is(this, "AsyncFunction")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsGeneratorFunction(this: *const Value) -> bool {
    class_tag_is(this, "GeneratorFunction")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsPromise(this: *const Value) -> bool {
    instance_of_global(this, b"Promise\0")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsMap(this: *const Value) -> bool {
    class_tag_is(this, "Map")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsSet(this: *const Value) -> bool {
    class_tag_is(this, "Set")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsMapIterator(this: *const Value) -> bool {
    class_tag_is(this, "Map Iterator")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsSetIterator(this: *const Value) -> bool {
    class_tag_is(this, "Set Iterator")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsSetGeneratorObject(this: *const Value) -> bool {
    // TODO(v82jsc): not distinguishable via JSC C API; approximate as generator.
    class_tag_is(this, "Generator")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsWeakMap(this: *const Value) -> bool {
    class_tag_is(this, "WeakMap")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsWeakSet(this: *const Value) -> bool {
    class_tag_is(this, "WeakSet")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsProxy(this: *const Value) -> bool {
    // TODO(v82jsc): JSC Proxies are transparent; no reliable C API detection.
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsModuleNamespaceObject(this: *const Value) -> bool {
    class_tag_is(this, "Module")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsExternal(this: *const Value) -> bool {
    // TODO(v82jsc): External values are represented via private classes; no
    // generic JSC C API check. Inert false.
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsWasmMemoryObject(this: *const Value) -> bool {
    instance_of_global(this, b"WebAssembly\0") && class_tag_is(this, "WebAssembly.Memory")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsWasmModuleObject(this: *const Value) -> bool {
    class_tag_is(this, "WebAssembly.Module")
}

// ===========================================================================
// ArrayBuffer / TypedArray / DataView (real JSC support via JSTypedArrayType)
// ===========================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsArrayBuffer(this: *const Value) -> bool {
    typed_array_type(this) == JSTypedArrayType::ArrayBuffer
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsArrayBufferView(this: *const Value) -> bool {
    // ArrayBufferView = TypedArray | DataView
    v8__Value__IsTypedArray(this) || v8__Value__IsDataView(this)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsTypedArray(this: *const Value) -> bool {
    let t = typed_array_type(this);
    t != JSTypedArrayType::None && t != JSTypedArrayType::ArrayBuffer
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUint8Array(this: *const Value) -> bool {
    typed_array_type(this) == JSTypedArrayType::Uint8Array
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUint8ClampedArray(this: *const Value) -> bool {
    typed_array_type(this) == JSTypedArrayType::Uint8ClampedArray
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsInt8Array(this: *const Value) -> bool {
    typed_array_type(this) == JSTypedArrayType::Int8Array
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUint16Array(this: *const Value) -> bool {
    typed_array_type(this) == JSTypedArrayType::Uint16Array
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsInt16Array(this: *const Value) -> bool {
    typed_array_type(this) == JSTypedArrayType::Int16Array
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUint32Array(this: *const Value) -> bool {
    typed_array_type(this) == JSTypedArrayType::Uint32Array
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsInt32Array(this: *const Value) -> bool {
    typed_array_type(this) == JSTypedArrayType::Int32Array
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsFloat32Array(this: *const Value) -> bool {
    typed_array_type(this) == JSTypedArrayType::Float32Array
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsFloat64Array(this: *const Value) -> bool {
    typed_array_type(this) == JSTypedArrayType::Float64Array
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsBigInt64Array(this: *const Value) -> bool {
    typed_array_type(this) == JSTypedArrayType::BigInt64Array
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsBigUint64Array(this: *const Value) -> bool {
    typed_array_type(this) == JSTypedArrayType::BigUint64Array
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsDataView(this: *const Value) -> bool {
    class_tag_is(this, "DataView")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsSharedArrayBuffer(this: *const Value) -> bool {
    class_tag_is(this, "SharedArrayBuffer")
}

// ===========================================================================
// Comparisons
// ===========================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__StrictEquals(this: *const Value, that: *const Value) -> bool {
    let c = ctx();
    !c.is_null() && unsafe { JSValueIsStrictEqual(c, jsval(this), jsval(that)) }
}

// ===========================================================================
// Conversions
// ===========================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToBigInt(
    this: *const Value,
    context: *const Context,
) -> *const BigInt {
    // TODO(v82jsc): no JSC C API to coerce to BigInt; only pass through if
    // already a BigInt.
    let c = ctx_of(context) as JSContextRef;
    if c.is_null() {
        return ptr::null();
    }
    if unsafe { JSValueGetType(c, jsval(this)) } == JSType::BigInt {
        return intern_ctx::<BigInt>(c, jsval(this));
    }
    ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToString(
    this: *const Value,
    context: *const Context,
) -> *const V8String {
    let c = ctx_of(context) as JSContextRef;
    if c.is_null() {
        return ptr::null();
    }
    unsafe {
        let mut exc: JSValueRef = ptr::null();
        let s = JSValueToStringCopy(c, jsval(this), &mut exc);
        if s.is_null() {
            return ptr::null();
        }
        let v = JSValueMakeString(c, s);
        JSStringRelease(s);
        intern_ctx::<V8String>(c, v)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToObject(
    this: *const Value,
    context: *const Context,
) -> *const Object {
    let c = ctx_of(context) as JSContextRef;
    if c.is_null() {
        return ptr::null();
    }
    unsafe {
        let mut exc: JSValueRef = ptr::null();
        let o = JSValueToObject(c, jsval(this), &mut exc);
        if o.is_null() {
            return ptr::null();
        }
        intern_ctx::<Object>(c, o as JSValueRef)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToInteger(
    this: *const Value,
    context: *const Context,
) -> *const Integer {
    let c = ctx_of(context) as JSContextRef;
    if c.is_null() {
        return ptr::null();
    }
    unsafe {
        let mut exc: JSValueRef = ptr::null();
        let n = JSValueToNumber(c, jsval(this), &mut exc);
        if !exc.is_null() {
            return ptr::null();
        }
        let truncated = if n.is_nan() { 0.0 } else { n.trunc() };
        let v = JSValueMakeNumber(c, truncated);
        intern_ctx::<Integer>(c, v)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToBoolean(
    this: *const Value,
    isolate: *mut RealIsolate,
) -> *const Boolean {
    let _ = isolate;
    let c = ctx();
    if c.is_null() {
        return ptr::null();
    }
    unsafe {
        let b = JSValueToBoolean(c, jsval(this));
        let v = JSValueMakeBoolean(c, b);
        intern_ctx::<Boolean>(c, v)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__NumberValue(
    this: *const Value,
    context: *const Context,
    out: *mut Maybe<f64>,
) {
    let c = ctx_of(context) as JSContextRef;
    if c.is_null() {
        unsafe { maybe_none(out) };
        return;
    }
    unsafe {
        let mut exc: JSValueRef = ptr::null();
        let n = JSValueToNumber(c, jsval(this), &mut exc);
        if exc.is_null() {
            maybe_set(out, n);
        } else {
            maybe_none(out);
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__Int32Value(
    this: *const Value,
    context: *const Context,
    out: *mut Maybe<i32>,
) {
    let c = ctx_of(context) as JSContextRef;
    if c.is_null() {
        unsafe { maybe_none(out) };
        return;
    }
    unsafe {
        let mut exc: JSValueRef = ptr::null();
        let n = JSValueToNumber(c, jsval(this), &mut exc);
        if exc.is_null() && n.is_finite() {
            // ECMAScript ToInt32 semantics (wrap modulo 2^32).
            let i = n.trunc().rem_euclid(4294967296.0);
            let i = if i >= 2147483648.0 { i - 4294967296.0 } else { i };
            maybe_set(out, i as i32);
        } else {
            maybe_none(out);
        }
    }
}
