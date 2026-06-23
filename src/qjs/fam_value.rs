//! QuickJS-ng-backed `v8::Value` shims — family "value".
//!
//! Implements the v8 Value predicates (`Is*`), conversions (`To*`,
//! `NumberValue`, `Int32Value`), and `StrictEquals` against the QuickJS-ng
//! runtime. Ported from the deno PR's QuickJS logic
//! (reference/qjs_v8_compat/src/value.rs) and structured to mirror the JSC
//! backend's `src/shim_value.rs` C-ABI shape — but every JSC call is replaced
//! by the equivalent QuickJS-ng call.
//!
//! Refcount discipline:
//!  * Predicates (`Is*`, `StrictEquals`) only *read* a borrowed JSValue (via
//!    `jsval_of`) and never mint a new handle, so no dup/free is required.
//!  * Conversions that return a NEW handle route the OWNED (+1) JSValue
//!    produced by a QuickJS `JS_*` call through `intern`, transferring
//!    ownership into the current handle scope. For values that are merely a
//!    borrowed pass-through of `this`, we `intern_dup` (JS_DupValue) so the
//!    arena owns its own refcount.
//!  * Any temporary JSValue we create and do NOT keep (globals, ctor lookups,
//!    `Object.prototype.toString` machinery, call results) is released with
//!    `JS_FreeValue`.

#![allow(non_snake_case)]

use crate::qjs::quickjs_sys::*;
use crate::qjs::shim_core::{ctx_of, current_ctx, intern, intern_dup, jsval_of};
use crate::support::Maybe;
use crate::{BigInt, Boolean, Context, Integer, Object, RealIsolate, String as V8String, Value};
use std::os::raw::c_char;
use std::ptr;

// ---------------------------------------------------------------------------
// Extra QuickJS-ng C API functions not declared in quickjs_sys.rs.
// (Verified against vendor/quickjs-ng/quickjs.h.)
// ---------------------------------------------------------------------------
unsafe extern "C" {
    /// `bool JS_IsStrictEqual(JSContext*, JSValueConst, JSValueConst)`.
    fn JS_IsStrictEqual(ctx: *mut JSContext, op1: JSValue, op2: JSValue) -> bool;
    /// `JSValue JS_ToObject(JSContext*, JSValueConst)` — returns owned (+1).
    fn JS_ToObject(ctx: *mut JSContext, val: JSValue) -> JSValue;
    /// `int JS_IsInstanceOf(JSContext*, JSValueConst val, JSValueConst obj)`.
    /// Returns 1 / 0 / -1(exception).
    fn JS_IsInstanceOf(ctx: *mut JSContext, val: JSValue, obj: JSValue) -> std::os::raw::c_int;
    /// `int JS_GetTypedArrayType(JSValueConst)` — returns -1 if not a typed
    /// array, else a `JSTypedArrayEnum` value.
    fn JS_GetTypedArrayType(obj: JSValue) -> std::os::raw::c_int;
}

// JSTypedArrayEnum (vendor/quickjs-ng/quickjs.h).
const JS_TYPED_ARRAY_UINT8C: i32 = 0;
const JS_TYPED_ARRAY_INT8: i32 = 1;
const JS_TYPED_ARRAY_UINT8: i32 = 2;
const JS_TYPED_ARRAY_INT16: i32 = 3;
const JS_TYPED_ARRAY_UINT16: i32 = 4;
const JS_TYPED_ARRAY_INT32: i32 = 5;
const JS_TYPED_ARRAY_UINT32: i32 = 6;
const JS_TYPED_ARRAY_BIG_INT64: i32 = 7;
const JS_TYPED_ARRAY_BIG_UINT64: i32 = 8;
const JS_TYPED_ARRAY_FLOAT16: i32 = 9;
const JS_TYPED_ARRAY_FLOAT32: i32 = 10;
const JS_TYPED_ARRAY_FLOAT64: i32 = 11;

// ---------------------------------------------------------------------------
// Maybe<T> mirror — Maybe's fields are private, so reproduce its #[repr(C)]
// layout (matches src/support.rs: { has_value: bool, value: T }).
// ---------------------------------------------------------------------------
#[repr(C)]
struct MaybeMirror<T> {
    has_value: bool,
    value: T,
}

#[inline]
unsafe fn maybe_set<T: Copy>(out: *mut Maybe<T>, val: T) {
    if out.is_null() {
        return;
    }
    unsafe {
        ptr::write(
            out as *mut MaybeMirror<T>,
            MaybeMirror {
                has_value: true,
                value: val,
            },
        );
    }
}

#[inline]
unsafe fn maybe_none<T: Copy + Default>(out: *mut Maybe<T>) {
    if out.is_null() {
        return;
    }
    unsafe {
        ptr::write(
            out as *mut MaybeMirror<T>,
            MaybeMirror {
                has_value: false,
                value: T::default(),
            },
        );
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[inline]
fn ctx() -> *mut JSContext {
    current_ctx()
}

#[inline]
fn is_obj(this: *const Value) -> bool {
    !this.is_null() && jsv_is_object(&jsval_of(this))
}

/// `Object.prototype.toString.call(v)` -> tag string like "[object Date]".
/// Returns true if the resulting tag equals `tag` (e.g. "Date"). Mirrors the
/// JSC backend's `class_tag_is` but uses QuickJS-ng calls. Every temporary
/// JSValue minted here is freed before returning.
fn class_tag_is(this: *const Value, tag: &str) -> bool {
    let c = ctx();
    if c.is_null() || !is_obj(this) {
        return false;
    }
    let v = jsval_of(this);
    unsafe {
        let global = JS_GetGlobalObject(c);
        // global.Object
        let obj_ctor = JS_GetPropertyStr(c, global, b"Object\0".as_ptr() as *const c_char);
        // Object.prototype
        let proto = JS_GetPropertyStr(c, obj_ctor, b"prototype\0".as_ptr() as *const c_char);
        // Object.prototype.toString
        let ts = JS_GetPropertyStr(c, proto, b"toString\0".as_ptr() as *const c_char);

        let mut result = false;
        if jsv_is_object(&ts) {
            // toString.call(v): JS_Call(ctx, func, this=v, 0, null)
            let r = JS_Call(c, ts, v, 0, ptr::null_mut());
            if !jsv_is_exception(&r) {
                let cstr = JS_ToCString(c, r);
                if !cstr.is_null() {
                    let got = std::ffi::CStr::from_ptr(cstr).to_string_lossy().into_owned();
                    JS_FreeCString(c, cstr);
                    result = got == format!("[object {}]", tag);
                }
            } else {
                // Clear any pending exception from the call.
                let exc = JS_GetException(c);
                JS_FreeValue(c, exc);
            }
            JS_FreeValue(c, r);
        }

        JS_FreeValue(c, ts);
        JS_FreeValue(c, proto);
        JS_FreeValue(c, obj_ctor);
        JS_FreeValue(c, global);
        result
    }
}

/// Check `v instanceof global[ctor_name]` via QuickJS `JS_IsInstanceOf`.
/// `ctor_name` must be NUL-terminated. Frees all temporaries.
fn instance_of_global(this: *const Value, ctor_name: &[u8]) -> bool {
    let c = ctx();
    if c.is_null() || !is_obj(this) {
        return false;
    }
    let v = jsval_of(this);
    unsafe {
        let global = JS_GetGlobalObject(c);
        let ctor = JS_GetPropertyStr(c, global, ctor_name.as_ptr() as *const c_char);
        let mut result = false;
        if jsv_is_object(&ctor) {
            let r = JS_IsInstanceOf(c, v, ctor);
            // r == 1 true, 0 false, -1 exception.
            if r < 0 {
                let exc = JS_GetException(c);
                JS_FreeValue(c, exc);
            } else {
                result = r == 1;
            }
        }
        JS_FreeValue(c, ctor);
        JS_FreeValue(c, global);
        result
    }
}

/// Typed-array enum for `this`, or -1 if not a typed array.
#[inline]
fn typed_array_type(this: *const Value) -> i32 {
    if !is_obj(this) {
        return -1;
    }
    unsafe { JS_GetTypedArrayType(jsval_of(this)) }
}

// ===========================================================================
// Primitive type predicates (real — pure tag inspection)
// ===========================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsBoolean(this: *const Value) -> bool {
    !this.is_null() && jsv_is_bool(&jsval_of(this))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsTrue(this: *const Value) -> bool {
    if this.is_null() {
        return false;
    }
    let v = jsval_of(this);
    jsv_is_bool(&v) && unsafe { v.u.int32 != 0 }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsFalse(this: *const Value) -> bool {
    if this.is_null() {
        return false;
    }
    let v = jsval_of(this);
    jsv_is_bool(&v) && unsafe { v.u.int32 == 0 }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsNull(this: *const Value) -> bool {
    !this.is_null() && jsv_is_null(&jsval_of(this))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUndefined(this: *const Value) -> bool {
    !this.is_null() && jsv_is_undefined(&jsval_of(this))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsNullOrUndefined(this: *const Value) -> bool {
    if this.is_null() {
        return false;
    }
    let v = jsval_of(this);
    jsv_is_null(&v) || jsv_is_undefined(&v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsString(this: *const Value) -> bool {
    !this.is_null() && jsv_is_string(&jsval_of(this))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsSymbol(this: *const Value) -> bool {
    !this.is_null() && jsv_is_symbol(&jsval_of(this))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsNumber(this: *const Value) -> bool {
    !this.is_null() && jsv_is_number(&jsval_of(this))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsBigInt(this: *const Value) -> bool {
    !this.is_null() && jsv_is_bigint(&jsval_of(this))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsObject(this: *const Value) -> bool {
    is_obj(this)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsInt32(this: *const Value) -> bool {
    if this.is_null() {
        return false;
    }
    let v = jsval_of(this);
    // Fast path: an exact int tag is always Int32-representable in QuickJS.
    if jsv_is_int(&v) {
        return true;
    }
    // A float64 that holds an integral value within i32 range.
    if jsv_is_float64(&v) {
        let n = unsafe { v.u.float64 };
        return n.is_finite()
            && n.fract() == 0.0
            && n >= i32::MIN as f64
            && n <= i32::MAX as f64;
    }
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUint32(this: *const Value) -> bool {
    if this.is_null() {
        return false;
    }
    let v = jsval_of(this);
    if jsv_is_int(&v) {
        return unsafe { v.u.int32 >= 0 };
    }
    if jsv_is_float64(&v) {
        let n = unsafe { v.u.float64 };
        return n.is_finite() && n.fract() == 0.0 && n >= 0.0 && n <= u32::MAX as f64;
    }
    false
}

// ===========================================================================
// Array / Function predicates (real)
// ===========================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsArray(this: *const Value) -> bool {
    let c = ctx();
    !c.is_null() && !this.is_null() && unsafe { JS_IsArray(c, jsval_of(this)) != 0 }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsFunction(this: *const Value) -> bool {
    let c = ctx();
    !c.is_null() && !this.is_null() && unsafe { JS_IsFunction(c, jsval_of(this)) != 0 }
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
    // A boxed BigInt is an Object (not a primitive bigint) whose tag is BigInt.
    is_obj(this) && class_tag_is(this, "BigInt")
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
    // TODO(qjs): QuickJS has no C API to distinguish async functions; approximate
    // via the AsyncFunction tag, else false.
    class_tag_is(this, "AsyncFunction")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsGeneratorFunction(this: *const Value) -> bool {
    class_tag_is(this, "GeneratorFunction")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsPromise(this: *const Value) -> bool {
    // QuickJS exposes a direct predicate.
    !this.is_null() && unsafe { JS_IsPromise(jsval_of(this)) != 0 }
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
    // TODO(qjs): not distinguishable via QuickJS C API; approximate as generator.
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
    // TODO(qjs): QuickJS Proxies are transparent; no reliable C API detection.
    let _ = this;
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsModuleNamespaceObject(this: *const Value) -> bool {
    class_tag_is(this, "Module")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsExternal(this: *const Value) -> bool {
    // External values are objects of our private `ext_class` (see fam_function).
    if this.is_null() {
        return false;
    }
    super::fam_function::value_is_external(jsval_of(this))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsWasmMemoryObject(this: *const Value) -> bool {
    // TODO(qjs): QuickJS-ng has no WebAssembly; never a WasmMemory object.
    let _ = this;
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsWasmModuleObject(this: *const Value) -> bool {
    // TODO(qjs): QuickJS-ng has no WebAssembly; never a WasmModule object.
    let _ = this;
    false
}

// ===========================================================================
// ArrayBuffer / TypedArray / DataView
// ===========================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsArrayBuffer(this: *const Value) -> bool {
    class_tag_is(this, "ArrayBuffer")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsSharedArrayBuffer(this: *const Value) -> bool {
    class_tag_is(this, "SharedArrayBuffer")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsDataView(this: *const Value) -> bool {
    class_tag_is(this, "DataView")
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsTypedArray(this: *const Value) -> bool {
    typed_array_type(this) >= 0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsArrayBufferView(this: *const Value) -> bool {
    // ArrayBufferView = TypedArray | DataView
    v8__Value__IsTypedArray(this) || v8__Value__IsDataView(this)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUint8Array(this: *const Value) -> bool {
    typed_array_type(this) == JS_TYPED_ARRAY_UINT8
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUint8ClampedArray(this: *const Value) -> bool {
    typed_array_type(this) == JS_TYPED_ARRAY_UINT8C
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsInt8Array(this: *const Value) -> bool {
    typed_array_type(this) == JS_TYPED_ARRAY_INT8
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUint16Array(this: *const Value) -> bool {
    typed_array_type(this) == JS_TYPED_ARRAY_UINT16
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsInt16Array(this: *const Value) -> bool {
    typed_array_type(this) == JS_TYPED_ARRAY_INT16
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUint32Array(this: *const Value) -> bool {
    typed_array_type(this) == JS_TYPED_ARRAY_UINT32
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsInt32Array(this: *const Value) -> bool {
    typed_array_type(this) == JS_TYPED_ARRAY_INT32
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsFloat32Array(this: *const Value) -> bool {
    typed_array_type(this) == JS_TYPED_ARRAY_FLOAT32
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsFloat64Array(this: *const Value) -> bool {
    typed_array_type(this) == JS_TYPED_ARRAY_FLOAT64
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsBigInt64Array(this: *const Value) -> bool {
    typed_array_type(this) == JS_TYPED_ARRAY_BIG_INT64
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsBigUint64Array(this: *const Value) -> bool {
    typed_array_type(this) == JS_TYPED_ARRAY_BIG_UINT64
}

// Float16Array is not in my assigned set; the enum value is recorded above for
// completeness but unused here. Silence the dead-code lint locally.
const _: i32 = JS_TYPED_ARRAY_FLOAT16;

// ===========================================================================
// Comparisons
// ===========================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__StrictEquals(this: *const Value, that: *const Value) -> bool {
    let c = ctx();
    if c.is_null() || this.is_null() || that.is_null() {
        return false;
    }
    unsafe { JS_IsStrictEqual(c, jsval_of(this), jsval_of(that)) }
}

// ===========================================================================
// Conversions
// ===========================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToBigInt(
    this: *const Value,
    context: *const Context,
) -> *const BigInt {
    // TODO(qjs): no direct JS_ToBigInt in the declared surface; only pass
    // through if already a BigInt. (BigInt(x) coercion would need an eval/call.)
    let c = ctx_of(context);
    if c.is_null() || this.is_null() {
        return ptr::null();
    }
    let v = jsval_of(this);
    if jsv_is_bigint(&v) {
        // Borrowed pass-through: dup so the arena owns its refcount.
        return intern_dup::<BigInt>(c, v);
    }
    ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToObject(
    this: *const Value,
    context: *const Context,
) -> *const Object {
    let c = ctx_of(context);
    if c.is_null() || this.is_null() {
        return ptr::null();
    }
    unsafe {
        // JS_ToObject returns an owned (+1) value (or an exception).
        let o = JS_ToObject(c, jsval_of(this));
        if jsv_is_exception(&o) {
            let exc = JS_GetException(c);
            JS_FreeValue(c, exc);
            return ptr::null();
        }
        intern::<Object>(o)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToInteger(
    this: *const Value,
    context: *const Context,
) -> *const Integer {
    let c = ctx_of(context);
    if c.is_null() || this.is_null() {
        return ptr::null();
    }
    unsafe {
        let mut n: f64 = 0.0;
        if JS_ToFloat64(c, &mut n, jsval_of(this)) < 0 {
            let exc = JS_GetException(c);
            JS_FreeValue(c, exc);
            return ptr::null();
        }
        // ToInteger: truncate toward zero; NaN -> 0.
        let truncated = if n.is_nan() { 0.0 } else { n.trunc() };
        // Integer is a Number under the hood; mint a fresh owned number value.
        let v = JS_NewFloat64(c, truncated);
        intern::<Integer>(v)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToBoolean(
    this: *const Value,
    _isolate: *mut RealIsolate,
) -> *const Boolean {
    let c = ctx();
    if c.is_null() || this.is_null() {
        return ptr::null();
    }
    unsafe {
        // JS_ToBool returns 1/0 (or -1 on error, which we coerce to false).
        let b = JS_ToBool(c, jsval_of(this));
        let v = JS_NewBool(c, if b > 0 { 1 } else { 0 });
        intern::<Boolean>(v)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__NumberValue(
    this: *const Value,
    context: *const Context,
    out: *mut Maybe<f64>,
) {
    let c = ctx_of(context);
    if c.is_null() || this.is_null() {
        unsafe { maybe_none(out) };
        return;
    }
    unsafe {
        let mut n: f64 = 0.0;
        if JS_ToFloat64(c, &mut n, jsval_of(this)) < 0 {
            let exc = JS_GetException(c);
            JS_FreeValue(c, exc);
            maybe_none(out);
        } else {
            maybe_set(out, n);
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__Int32Value(
    this: *const Value,
    context: *const Context,
    out: *mut Maybe<i32>,
) {
    let c = ctx_of(context);
    if c.is_null() || this.is_null() {
        unsafe { maybe_none(out) };
        return;
    }
    unsafe {
        // JS_ToInt32 already implements ECMAScript ToInt32 (modulo-2^32 wrap).
        let mut i: i32 = 0;
        if JS_ToInt32(c, &mut i, jsval_of(this)) < 0 {
            let exc = JS_GetException(c);
            JS_FreeValue(c, exc);
            maybe_none(out);
        } else {
            maybe_set(out, i);
        }
    }
}

// ---------------------------------------------------------------------------
// Pre-existing ToString shim (kept from the original qjs shim_value.rs).
// v8__Value__ToString is defined by the foundation (shim_value.rs).

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__TypeOf(
    this: *const Value,
    _isolate: *mut RealIsolate,
) -> *const V8String {
    let ctx = current_ctx();
    if ctx.is_null() {
        return ptr::null();
    }
    let v = jsval_of(this);
    // Map QuickJS tags to JS `typeof` strings.
    let s: &[u8] = match v.tag {
        JS_TAG_UNDEFINED | JS_TAG_UNINITIALIZED => b"undefined\0",
        JS_TAG_NULL => b"object\0",
        JS_TAG_BOOL => b"boolean\0",
        JS_TAG_INT | JS_TAG_FLOAT64 => b"number\0",
        JS_TAG_STRING | JS_TAG_STRING_ROPE => b"string\0",
        JS_TAG_SYMBOL => b"symbol\0",
        JS_TAG_BIG_INT | JS_TAG_SHORT_BIG_INT => b"bigint\0",
        JS_TAG_OBJECT => {
            if unsafe { JS_IsFunction(ctx, v) } != 0 {
                b"function\0"
            } else {
                b"object\0"
            }
        }
        _ => b"object\0",
    };
    let js = unsafe { JS_NewString(ctx, s.as_ptr() as *const c_char) };
    intern::<V8String>(js)
}
