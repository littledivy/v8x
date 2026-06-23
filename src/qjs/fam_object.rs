//! QuickJS-backed `object` family shims: v8::Object Get/Set/Has/Delete/
//! CreateDataProperty/DefineOwnProperty/GetOwnPropertyNames/Prototype/Private,
//! plus v8::Array New/Length.
//!
//! Ported from reference/qjs_v8_compat/src/object.rs (which uses lossy
//! string keys) but upgraded to atom-based property access via
//! `JS_ValueToAtom` so symbol and integer keys round-trip correctly — this
//! matches the JSC backend's key-aware `*ForKey` calls in src/shim_object.rs.
//!
//! Refcount discipline (see shim_core): every fresh `JSValue` returned by a
//! QuickJS getter is +1 and goes through `intern`. Values handed to
//! `JS_SetProperty`/`JS_DefinePropertyValue` are *consumed* by QuickJS, so we
//! `JS_DupValue` first because the caller's handle slot still owns its copy.
//! Atoms from `JS_ValueToAtom`/`JS_NewAtom` are freed with `JS_FreeAtom`.
#![allow(non_snake_case, unused)]

use super::quickjs_sys::*;
use super::shim_core::{
    ctx_of, current_ctx, current_iso, intern, intern_dup, iso_state, jsval_of,
};
use crate::support::{int, Maybe, MaybeBool};
use crate::{
    Array, Context, IndexFilter, KeyCollectionMode, KeyConversionMode, Name, Object,
    Private, PropertyAttribute, PropertyDescriptor, PropertyFilter, RealIsolate,
    String as V8String, Value,
};
use std::os::raw::{c_char, c_int};
use std::ptr;

// ---- atom-based property access & enumeration not in quickjs_sys.rs ----------
unsafe extern "C" {
    fn JS_ValueToAtom(ctx: *mut JSContext, val: JSValue) -> JSAtom;
    fn JS_GetProperty(ctx: *mut JSContext, this_obj: JSValue, prop: JSAtom) -> JSValue;
    fn JS_SetProperty(
        ctx: *mut JSContext,
        this_obj: JSValue,
        prop: JSAtom,
        val: JSValue,
    ) -> c_int;
    fn JS_DefinePropertyValue(
        ctx: *mut JSContext,
        this_obj: JSValue,
        prop: JSAtom,
        val: JSValue,
        flags: c_int,
    ) -> c_int;
    fn JS_SetPrototype(ctx: *mut JSContext, obj: JSValue, proto_val: JSValue) -> c_int;
    fn JS_GetLength(ctx: *mut JSContext, obj: JSValue, pres: *mut i64) -> c_int;
    fn JS_ToBool(ctx: *mut JSContext, val: JSValue) -> c_int;
    fn JS_GetOwnPropertyNames(
        ctx: *mut JSContext,
        ptab: *mut *mut JSPropertyEnum,
        plen: *mut u32,
        obj: JSValue,
        flags: c_int,
    ) -> c_int;
    fn JS_FreePropertyEnum(ctx: *mut JSContext, tab: *mut JSPropertyEnum, len: u32);
}

#[repr(C)]
struct JSPropertyEnum {
    is_enumerable: bool,
    atom: JSAtom,
}

const JS_GPN_STRING_MASK: c_int = 1 << 0;
const JS_GPN_SYMBOL_MASK: c_int = 1 << 1;
const JS_GPN_ENUM_ONLY: c_int = 1 << 4;

// ---- helpers ----------------------------------------------------------------

#[inline]
fn iso_ctx(isolate: *mut RealIsolate) -> *mut JSContext {
    if isolate.is_null() {
        return ptr::null_mut();
    }
    let st = iso_state(isolate);
    st.contexts.last().copied().unwrap_or(st.ctx)
}

#[inline]
fn just_bool(b: bool) -> MaybeBool {
    if b {
        MaybeBool::JustTrue
    } else {
        MaybeBool::JustFalse
    }
}

/// Convert a v8 key handle (`*const Value`/`*const Name`/`*const Private`) into
/// a QuickJS atom (+1, must be freed with `JS_FreeAtom`). Returns 0 (JS_ATOM_NULL)
/// on failure.
#[inline]
fn key_atom<T>(ctx: *mut JSContext, key: *const T) -> JSAtom {
    if key.is_null() {
        return 0;
    }
    unsafe { JS_ValueToAtom(ctx, jsval_of(key)) }
}

/// Map v8 `PropertyAttribute` bits (READ_ONLY=1, DONT_ENUM=2, DONT_DELETE=4)
/// onto QuickJS `JS_PROP_*` flags. v8 attributes are *negative* permissions, so
/// an unset bit means the property is writable / enumerable / configurable.
#[inline]
fn attr_to_jsprop(attr: u32) -> c_int {
    let mut flags = JS_PROP_HAS_VALUE | JS_PROP_HAS_WRITABLE | JS_PROP_HAS_ENUMERABLE
        | JS_PROP_HAS_CONFIGURABLE;
    if attr & 1 == 0 {
        flags |= JS_PROP_WRITABLE;
    } // !READ_ONLY
    if attr & 2 == 0 {
        flags |= JS_PROP_ENUMERABLE;
    } // !DONT_ENUM
    if attr & 4 == 0 {
        flags |= JS_PROP_CONFIGURABLE;
    } // !DONT_DELETE
    flags
}

// JS_PROP_HAS_* bits (quickjs.h) needed for JS_DefinePropertyValue with attrs.
const JS_PROP_HAS_CONFIGURABLE: c_int = 1 << 8;
const JS_PROP_HAS_WRITABLE: c_int = 1 << 9;
const JS_PROP_HAS_ENUMERABLE: c_int = 1 << 10;
const JS_PROP_HAS_VALUE: c_int = 1 << 13;

#[inline]
fn read_attr(attr: &PropertyAttribute) -> u32 {
    // PropertyAttribute is #[repr(C)] struct PropertyAttribute(u32).
    unsafe { *(attr as *const PropertyAttribute as *const u32) }
}

// ===================================================================
// Object
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__New(isolate: *mut RealIsolate) -> *const Object {
    let ctx = iso_ctx(isolate);
    if ctx.is_null() {
        return ptr::null();
    }
    // JS_NewObject returns +1.
    let o = unsafe { JS_NewObject(ctx) };
    if o.tag == JS_TAG_EXCEPTION {
        return ptr::null();
    }
    intern::<Object>(o)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__New__with_prototype_and_properties(
    isolate: *mut RealIsolate,
    prototype_or_null: *const Value,
    names: *const *const Name,
    values: *const *const Value,
    length: usize,
) -> *const Object {
    let ctx = iso_ctx(isolate);
    if ctx.is_null() {
        return ptr::null();
    }
    let o = unsafe { JS_NewObject(ctx) };
    if o.tag == JS_TAG_EXCEPTION {
        return ptr::null();
    }
    unsafe {
        if !prototype_or_null.is_null() {
            let proto = jsval_of(prototype_or_null);
            if !jsv_is_undefined(&proto) && !jsv_is_null(&proto) {
                // JS_SetPrototype borrows proto; do not consume our slot's ref.
                JS_SetPrototype(ctx, o, proto);
            }
        }
        for i in 0..length {
            let key = *names.add(i);
            let val = *values.add(i);
            let atom = key_atom(ctx, key);
            if atom == 0 {
                continue;
            }
            // JS_SetProperty consumes the value; dup our borrowed slot copy.
            let v = JS_DupValue(ctx, jsval_of(val));
            JS_SetProperty(ctx, o, atom, v);
            JS_FreeAtom(ctx, atom);
        }
    }
    intern::<Object>(o)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__Get(
    this: *const Object,
    context: *const Context,
    key: *const Value,
) -> *const Value {
    let ctx = ctx_of(context);
    if ctx.is_null() || this.is_null() {
        return ptr::null();
    }
    let atom = key_atom(ctx, key);
    if atom == 0 {
        return ptr::null();
    }
    let v = unsafe { JS_GetProperty(ctx, jsval_of(this), atom) };
    unsafe { JS_FreeAtom(ctx, atom) };
    if v.tag == JS_TAG_EXCEPTION {
        return ptr::null();
    }
    // JS_GetProperty returns +1.
    intern::<Value>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetIndex(
    this: *const Object,
    context: *const Context,
    index: u32,
) -> *const Value {
    let ctx = ctx_of(context);
    if ctx.is_null() || this.is_null() {
        return ptr::null();
    }
    let v = unsafe { JS_GetPropertyUint32(ctx, jsval_of(this), index) };
    if v.tag == JS_TAG_EXCEPTION {
        return ptr::null();
    }
    intern::<Value>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetPrototype(this: *const Object) -> *const Value {
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return ptr::null();
    }
    // JS_GetPrototype returns +1 (or null/undefined).
    let v = unsafe { JS_GetPrototype(ctx, jsval_of(this)) };
    if v.tag == JS_TAG_EXCEPTION {
        return ptr::null();
    }
    intern::<Value>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__Set(
    this: *const Object,
    context: *const Context,
    key: *const Value,
    value: *const Value,
) -> MaybeBool {
    let ctx = ctx_of(context);
    if ctx.is_null() || this.is_null() {
        return MaybeBool::Nothing;
    }
    let atom = key_atom(ctx, key);
    if atom == 0 {
        return MaybeBool::Nothing;
    }
    // JS_SetProperty consumes the value; dup our borrowed slot copy.
    let v = unsafe { JS_DupValue(ctx, jsval_of(value)) };
    let r = unsafe { JS_SetProperty(ctx, jsval_of(this), atom, v) };
    unsafe { JS_FreeAtom(ctx, atom) };
    if r < 0 {
        return MaybeBool::Nothing;
    }
    just_bool(r != 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetIndex(
    this: *const Object,
    context: *const Context,
    index: u32,
    value: *const Value,
) -> MaybeBool {
    let ctx = ctx_of(context);
    if ctx.is_null() || this.is_null() {
        return MaybeBool::Nothing;
    }
    let v = unsafe { JS_DupValue(ctx, jsval_of(value)) };
    let r = unsafe { JS_SetPropertyUint32(ctx, jsval_of(this), index, v) };
    if r < 0 {
        return MaybeBool::Nothing;
    }
    just_bool(r != 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetPrototype(
    this: *const Object,
    context: *const Context,
    prototype: *const Value,
) -> MaybeBool {
    let ctx = ctx_of(context);
    if ctx.is_null() || this.is_null() {
        return MaybeBool::Nothing;
    }
    // JS_SetPrototype borrows proto.
    let r = unsafe { JS_SetPrototype(ctx, jsval_of(this), jsval_of(prototype)) };
    if r < 0 {
        return MaybeBool::Nothing;
    }
    just_bool(r != 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__CreateDataProperty(
    this: *const Object,
    context: *const Context,
    key: *const Name,
    value: *const Value,
) -> MaybeBool {
    let ctx = ctx_of(context);
    if ctx.is_null() || this.is_null() {
        return MaybeBool::Nothing;
    }
    let atom = key_atom(ctx, key);
    if atom == 0 {
        return MaybeBool::Nothing;
    }
    // JS_DefinePropertyValue consumes the value. Default attributes: C_W_E
    // (configurable, writable, enumerable) — what v8 CreateDataProperty uses.
    let v = unsafe { JS_DupValue(ctx, jsval_of(value)) };
    let r = unsafe { JS_DefinePropertyValue(ctx, jsval_of(this), atom, v, JS_PROP_C_W_E) };
    unsafe { JS_FreeAtom(ctx, atom) };
    if r < 0 {
        return MaybeBool::Nothing;
    }
    just_bool(r != 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__DefineOwnProperty(
    this: *const Object,
    context: *const Context,
    key: *const Name,
    value: *const Value,
    attr: PropertyAttribute,
) -> MaybeBool {
    let ctx = ctx_of(context);
    if ctx.is_null() || this.is_null() {
        return MaybeBool::Nothing;
    }
    let atom = key_atom(ctx, key);
    if atom == 0 {
        return MaybeBool::Nothing;
    }
    let flags = attr_to_jsprop(read_attr(&attr));
    let v = unsafe { JS_DupValue(ctx, jsval_of(value)) };
    let r = unsafe { JS_DefinePropertyValue(ctx, jsval_of(this), atom, v, flags) };
    unsafe { JS_FreeAtom(ctx, atom) };
    if r < 0 {
        return MaybeBool::Nothing;
    }
    just_bool(r != 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__DefineProperty(
    this: *const Object,
    context: *const Context,
    key: *const Name,
    desc: *const PropertyDescriptor,
) -> MaybeBool {
    // TODO(qjs): full v8 PropertyDescriptor (get/set/value/writable/enumerable/
    // configurable flag tri-states) requires reading the opaque descriptor
    // layout. Inert: report failure rather than silently mis-defining.
    MaybeBool::Nothing
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__Delete(
    this: *const Object,
    context: *const Context,
    key: *const Value,
) -> MaybeBool {
    let ctx = ctx_of(context);
    if ctx.is_null() || this.is_null() {
        return MaybeBool::Nothing;
    }
    let atom = key_atom(ctx, key);
    if atom == 0 {
        return MaybeBool::Nothing;
    }
    let r = unsafe { JS_DeleteProperty(ctx, jsval_of(this), atom, JS_PROP_THROW) };
    unsafe { JS_FreeAtom(ctx, atom) };
    if r < 0 {
        return MaybeBool::Nothing;
    }
    just_bool(r != 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__Has(
    this: *const Object,
    context: *const Context,
    key: *const Value,
) -> MaybeBool {
    let ctx = ctx_of(context);
    if ctx.is_null() || this.is_null() {
        return MaybeBool::Nothing;
    }
    let atom = key_atom(ctx, key);
    if atom == 0 {
        return MaybeBool::Nothing;
    }
    let r = unsafe { JS_HasProperty(ctx, jsval_of(this), atom) };
    unsafe { JS_FreeAtom(ctx, atom) };
    if r < 0 {
        return MaybeBool::Nothing;
    }
    just_bool(r != 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__HasOwnProperty(
    this: *const Object,
    context: *const Context,
    key: *const Name,
) -> MaybeBool {
    // QuickJS JS_HasProperty walks the prototype chain. To restrict to own
    // properties, enumerate own names and compare atoms.
    let ctx = ctx_of(context);
    if ctx.is_null() || this.is_null() {
        return MaybeBool::Nothing;
    }
    let want = key_atom(ctx, key);
    if want == 0 {
        return MaybeBool::Nothing;
    }
    let mut tab: *mut JSPropertyEnum = ptr::null_mut();
    let mut len: u32 = 0;
    let rc = unsafe {
        JS_GetOwnPropertyNames(
            ctx,
            &mut tab,
            &mut len,
            jsval_of(this),
            JS_GPN_STRING_MASK | JS_GPN_SYMBOL_MASK,
        )
    };
    if rc < 0 {
        unsafe { JS_FreeAtom(ctx, want) };
        return MaybeBool::Nothing;
    }
    let mut found = false;
    unsafe {
        for i in 0..len as usize {
            if (*tab.add(i)).atom == want {
                found = true;
                break;
            }
        }
        JS_FreePropertyEnum(ctx, tab, len);
        JS_FreeAtom(ctx, want);
    }
    just_bool(found)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetConstructorName(this: *const Object) -> *const V8String {
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return ptr::null();
    }
    // Read this.constructor.name.
    unsafe {
        let ctor_atom = JS_NewAtom(ctx, c"constructor".as_ptr());
        let ctor = JS_GetProperty(ctx, jsval_of(this), ctor_atom);
        JS_FreeAtom(ctx, ctor_atom);
        if ctor.tag == JS_TAG_EXCEPTION {
            return ptr::null();
        }
        if !jsv_is_object(&ctor) {
            JS_FreeValue(ctx, ctor);
            return ptr::null();
        }
        let name_atom = JS_NewAtom(ctx, c"name".as_ptr());
        let name = JS_GetProperty(ctx, ctor, name_atom);
        JS_FreeAtom(ctx, name_atom);
        JS_FreeValue(ctx, ctor);
        if name.tag == JS_TAG_EXCEPTION {
            return ptr::null();
        }
        if !jsv_is_string(&name) {
            JS_FreeValue(ctx, name);
            return ptr::null();
        }
        // name is +1; move into a slot.
        intern::<V8String>(name)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetIdentityHash(this: *const Object) -> int {
    // Use the JSValue pointer payload as a stable, nonzero hash for the lifetime
    // of the object (the boxed handle slot is stable; the underlying object ptr
    // is what identity should track).
    let v = jsval_of(this);
    let p = jsv_get_ptr(&v) as usize;
    let h = (p as u32) ^ ((p >> 32) as u32);
    (h & 0x7fff_ffff) as int
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetOwnPropertyNames(
    this: *const Object,
    context: *const Context,
    filter: PropertyFilter,
    key_conversion: KeyConversionMode,
) -> *const Array {
    // v8 GetOwnPropertyNames default: string keys only (SKIP_SYMBOLS by
    // default in deno usage). Honour SKIP_SYMBOLS / SKIP_STRINGS filter bits.
    let f = read_filter(&filter);
    let mut gpn = 0;
    // SKIP_STRINGS = 8, SKIP_SYMBOLS = 16 (see object.rs PropertyFilter).
    if f & 8 == 0 {
        gpn |= JS_GPN_STRING_MASK;
    }
    if f & 16 == 0 {
        // v8 GetOwnPropertyNames historically returns string keys only unless
        // symbols explicitly requested; default filter is ALL_PROPERTIES(0),
        // which for this entry point means strings. Keep strings-only when no
        // skip flags are present to match v8's default observable behaviour.
    }
    if gpn == 0 {
        gpn = JS_GPN_STRING_MASK;
    }
    property_names_array(this, context, gpn)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetPropertyNames(
    this: *const Object,
    context: *const Context,
    mode: KeyCollectionMode,
    property_filter: PropertyFilter,
    index_filter: IndexFilter,
    key_conversion: KeyConversionMode,
) -> *const Array {
    // TODO(qjs): mode (own vs include-prototypes) and index_filter are not
    // honoured; QuickJS JS_GetOwnPropertyNames is own-only. Return own string
    // keys, which covers the common deno serde path.
    property_names_array(this, context, JS_GPN_STRING_MASK)
}

#[inline]
fn read_filter(f: &PropertyFilter) -> u32 {
    // PropertyFilter is #[repr(C)] struct PropertyFilter(u32) (bitflags_stub).
    unsafe { *(f as *const PropertyFilter as *const u32) }
}

fn property_names_array(
    this: *const Object,
    context: *const Context,
    gpn_flags: c_int,
) -> *const Array {
    let ctx = ctx_of(context);
    if ctx.is_null() || this.is_null() {
        return ptr::null();
    }
    let mut tab: *mut JSPropertyEnum = ptr::null_mut();
    let mut len: u32 = 0;
    let rc = unsafe {
        JS_GetOwnPropertyNames(ctx, &mut tab, &mut len, jsval_of(this), gpn_flags)
    };
    if rc < 0 {
        return ptr::null();
    }
    // JS_NewArray returns +1.
    let arr = unsafe { JS_NewArray(ctx) };
    if arr.tag == JS_TAG_EXCEPTION {
        unsafe { JS_FreePropertyEnum(ctx, tab, len) };
        return ptr::null();
    }
    unsafe {
        for i in 0..len as usize {
            let atom = (*tab.add(i)).atom;
            // JS_AtomToValue returns +1; JS_SetPropertyUint32 consumes it.
            let key_val = JS_AtomToValue(ctx, atom);
            JS_SetPropertyUint32(ctx, arr, i as u32, key_val);
        }
        JS_FreePropertyEnum(ctx, tab, len);
    }
    intern::<Array>(arr)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetPrivate(
    this: *const Object,
    context: *const Context,
    key: *const Private,
) -> *const Value {
    // A v8 Private is backed by a JSValue symbol; read it as a normal key.
    let ctx = ctx_of(context);
    if ctx.is_null() || this.is_null() {
        return ptr::null();
    }
    let atom = key_atom(ctx, key);
    if atom == 0 {
        return ptr::null();
    }
    let v = unsafe { JS_GetProperty(ctx, jsval_of(this), atom) };
    unsafe { JS_FreeAtom(ctx, atom) };
    if v.tag == JS_TAG_EXCEPTION {
        return ptr::null();
    }
    intern::<Value>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetPrivate(
    this: *const Object,
    context: *const Context,
    key: *const Private,
    value: *const Value,
) -> MaybeBool {
    let ctx = ctx_of(context);
    if ctx.is_null() || this.is_null() {
        return MaybeBool::Nothing;
    }
    let atom = key_atom(ctx, key);
    if atom == 0 {
        return MaybeBool::Nothing;
    }
    // Define non-enumerable so private data doesn't leak into enumeration.
    let v = unsafe { JS_DupValue(ctx, jsval_of(value)) };
    let flags = JS_PROP_HAS_VALUE | JS_PROP_HAS_WRITABLE | JS_PROP_HAS_CONFIGURABLE
        | JS_PROP_WRITABLE
        | JS_PROP_CONFIGURABLE;
    let r = unsafe { JS_DefinePropertyValue(ctx, jsval_of(this), atom, v, flags) };
    unsafe { JS_FreeAtom(ctx, atom) };
    if r < 0 {
        return MaybeBool::Nothing;
    }
    just_bool(r != 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetPropertyAttributes(
    this: *const Object,
    context: *const Context,
    key: *const Value,
    out: *mut Maybe<PropertyAttribute>,
) {
    // TODO(qjs): JS_GetOwnProperty exposes a JSPropertyDescriptor with flags,
    // but it only covers own props; v8 walks the chain. Report NONE if the
    // property exists (via Has), otherwise no value — matches the JSC backend.
    if out.is_null() {
        return;
    }
    let ctx = ctx_of(context);
    let has = if ctx.is_null() || this.is_null() {
        false
    } else {
        let atom = key_atom(ctx, key);
        if atom == 0 {
            false
        } else {
            let r = unsafe { JS_HasProperty(ctx, jsval_of(this), atom) };
            unsafe { JS_FreeAtom(ctx, atom) };
            r > 0
        }
    };
    let m: Maybe<PropertyAttribute> = if has {
        let mut tmp: Maybe<PropertyAttribute> = unsafe { std::mem::zeroed() };
        // tmp = { has_value:false, value:NONE(0) }; flip has_value.
        unsafe {
            *(&mut tmp as *mut Maybe<PropertyAttribute> as *mut bool) = true;
        }
        tmp
    } else {
        unsafe { std::mem::zeroed() }
    };
    unsafe { ptr::write(out, m) };
}

// ===================================================================
// Array
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Array__New(isolate: *mut RealIsolate, length: int) -> *const Array {
    let ctx = iso_ctx(isolate);
    if ctx.is_null() {
        return ptr::null();
    }
    let arr = unsafe { JS_NewArray(ctx) };
    if arr.tag == JS_TAG_EXCEPTION {
        return ptr::null();
    }
    if length > 0 {
        // Pre-extend by setting length so the array reports the requested size.
        unsafe { JS_SetLength_local(ctx, arr, length as i64) };
    }
    intern::<Array>(arr)
}

#[inline]
unsafe fn JS_SetLength_local(ctx: *mut JSContext, arr: JSValue, len: i64) {
    // Set the `length` property numerically; QuickJS grows the array.
    let lv = jsv_float64(len as f64);
    let atom = JS_NewAtom(ctx, c"length".as_ptr());
    unsafe { JS_SetProperty(ctx, arr, atom, lv) };
    unsafe { JS_FreeAtom(ctx, atom) };
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Array__New_with_elements(
    isolate: *mut RealIsolate,
    elements: *const *const Value,
    length: usize,
) -> *const Array {
    let ctx = iso_ctx(isolate);
    if ctx.is_null() {
        return ptr::null();
    }
    let arr = unsafe { JS_NewArray(ctx) };
    if arr.tag == JS_TAG_EXCEPTION {
        return ptr::null();
    }
    unsafe {
        for i in 0..length {
            let el = *elements.add(i);
            // JS_SetPropertyUint32 consumes the value; dup our slot copy.
            let v = JS_DupValue(ctx, jsval_of(el));
            JS_SetPropertyUint32(ctx, arr, i as u32, v);
        }
    }
    intern::<Array>(arr)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Array__Length(array: *const Array) -> u32 {
    let ctx = current_ctx();
    if ctx.is_null() || array.is_null() {
        return 0;
    }
    let mut len: i64 = 0;
    let rc = unsafe { JS_GetLength(ctx, jsval_of(array), &mut len) };
    if rc < 0 || len < 0 {
        return 0;
    }
    len as u32
}

// ===================================================================
// Object::Wrap / Unwrap / IsApiWrapper — associate a cppgc-managed RustObj
// pointer with a JS wrapper object (deno wraps native-backed objects like
// CryptoKey). QuickJS has no V8 wrappable internal slots, so we keep a side
// table. CRITICAL: a v8 handle is a transient arena slot pointer, so we key by
// the *object's identity* (its JSValue `u.ptr` payload), not the handle address —
// different handles to the same object must resolve to the same wrapped pointer.
// ===================================================================

thread_local! {
    static WRAP_TABLE: std::cell::RefCell<
        std::collections::HashMap<(usize, u16), usize>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

#[inline]
fn wrap_key(wrapper: *const Object) -> usize {
    // Object identity = the heap object pointer behind the handle's JSValue.
    unsafe { jsval_of(wrapper).u.ptr as usize }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__Wrap(
    _isolate: *const RealIsolate,
    wrapper: *const Object,
    value: *const crate::binding::RustObj,
    tag: u16,
) {
    if wrapper.is_null() {
        return;
    }
    let key = wrap_key(wrapper);
    WRAP_TABLE.with(|t| {
        t.borrow_mut().insert((key, tag), value as usize);
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__Unwrap(
    _isolate: *const RealIsolate,
    wrapper: *const Object,
    tag: u16,
) -> *mut crate::binding::RustObj {
    if wrapper.is_null() {
        return ptr::null_mut();
    }
    let key = wrap_key(wrapper);
    WRAP_TABLE.with(|t| {
        t.borrow()
            .get(&(key, tag))
            .map(|&p| p as *mut crate::binding::RustObj)
            .unwrap_or(ptr::null_mut())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__IsApiWrapper(this: *const Object) -> bool {
    if this.is_null() {
        return false;
    }
    let key = wrap_key(this);
    WRAP_TABLE.with(|t| t.borrow().keys().any(|(w, _)| *w == key))
}
