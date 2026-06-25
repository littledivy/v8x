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
        // QuickJS's JS_SetProperty uses JS_PROP_THROW, so an ordinary `[[Set]]`
        // onto a non-writable data property (e.g. a function's read-only `name`)
        // or a non-extensible object THROWS. v8's `Object::Set` is a kDontThrow
        // set: it returns `Just(false)` for those cases without leaving a pending
        // exception (only a *throwing setter* propagates). napi relies on this —
        // each napi call runs inside a TryCatch, and a spurious "'name' is
        // read-only" poisons module registration. So swallow write-rejection
        // exceptions into `Just(false)`; re-arm anything else as `Nothing`.
        if unsafe { swallow_write_rejection(ctx) } {
            return MaybeBool::JustFalse;
        }
        return MaybeBool::Nothing;
    }
    just_bool(r != 0)
}

/// Inspect the pending exception after a failed `[[Set]]`. If it's a write
/// rejection (read-only property / non-extensible object), clear it and return
/// true (caller maps to `Just(false)`). Otherwise leave it pending and return
/// false (caller propagates as `Nothing`).
unsafe fn swallow_write_rejection(ctx: *mut JSContext) -> bool {
    if unsafe { JS_HasException(ctx) } == 0 {
        // No exception pending: a plain `false` result. Treat as Just(false).
        return true;
    }
    let exc = unsafe { JS_GetException(ctx) }; // +1, clears slot
    let cs = unsafe { JS_ToCString(ctx, exc) };
    let is_write_reject = if cs.is_null() {
        false
    } else {
        let msg = unsafe { std::ffi::CStr::from_ptr(cs) }.to_string_lossy();
        let m = msg.as_ref();
        m.contains("read-only") || m.contains("not extensible") || m.contains("non-extensible")
    };
    if !cs.is_null() {
        unsafe { JS_FreeCString(ctx, cs) };
    }
    if is_write_reject {
        unsafe { JS_FreeValue(ctx, exc) };
        true
    } else {
        // Re-arm the original exception for the caller to propagate.
        unsafe { JS_Throw(ctx, exc) };
        false
    }
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
    let ctx = ctx_of(context);
    if ctx.is_null() || this.is_null() || desc.is_null() {
        return MaybeBool::Nothing;
    }
    let atom = key_atom(ctx, key);
    if atom == 0 {
        return MaybeBool::Nothing;
    }
    let r = super::fam_property::pd_define(ctx, jsval_of(this), atom, desc);
    unsafe { JS_FreeAtom(ctx, atom) };
    if r < 0 {
        return MaybeBool::Nothing;
    }
    just_bool(r != 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetOwnPropertyDescriptor(
    this: *const Object,
    context: *const Context,
    key: *const Name,
) -> *const Value {
    let ctx = ctx_of(context);
    if ctx.is_null() || this.is_null() {
        return ptr::null();
    }
    // Route through the JS builtin `Object.getOwnPropertyDescriptor(obj, key)`,
    // which returns a plain `{value|get|set, writable, enumerable, configurable}`
    // object (or `undefined`) — exactly the v8 contract.
    unsafe {
        let global = JS_GetGlobalObject(ctx);
        let object_ctor = JS_GetPropertyStr(ctx, global, c"Object".as_ptr());
        JS_FreeValue(ctx, global);
        let func =
            JS_GetPropertyStr(ctx, object_ctor, c"getOwnPropertyDescriptor".as_ptr());
        JS_FreeValue(ctx, object_ctor);
        let mut args = [jsval_of(this), jsval_of(key)];
        let res = JS_Call(ctx, func, jsv_undefined(), 2, args.as_mut_ptr());
        JS_FreeValue(ctx, func);
        if res.tag == JS_TAG_EXCEPTION {
            return ptr::null();
        }
        // intern takes ownership of the +1 returned by JS_Call.
        intern::<Value>(res)
    }
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
    // v8's GetConstructorName ALWAYS returns a String (the caller unwraps it),
    // defaulting to "Object" for objects with no usable constructor (e.g. a
    // null-prototype namespace like `Deno`). Read this.constructor.name and fall
    // back to "Object" on any miss, never null — returning null panics the
    // caller (object.rs get_constructor_name) and crashes console.log.
    unsafe {
        let fallback = || intern::<V8String>(JS_NewString(ctx, c"Object".as_ptr()));
        // Clear any exception left by a failed lookup so it doesn't leak.
        let drain_exc = || {
            if JS_HasException(ctx) != 0 {
                JS_FreeValue(ctx, JS_GetException(ctx));
            }
        };
        let ctor_atom = JS_NewAtom(ctx, c"constructor".as_ptr());
        let ctor = JS_GetProperty(ctx, jsval_of(this), ctor_atom);
        JS_FreeAtom(ctx, ctor_atom);
        if ctor.tag == JS_TAG_EXCEPTION {
            drain_exc();
            return fallback();
        }
        if !jsv_is_object(&ctor) {
            JS_FreeValue(ctx, ctor);
            return fallback();
        }
        let name_atom = JS_NewAtom(ctx, c"name".as_ptr());
        let name = JS_GetProperty(ctx, ctor, name_atom);
        JS_FreeAtom(ctx, name_atom);
        JS_FreeValue(ctx, ctor);
        if name.tag == JS_TAG_EXCEPTION {
            drain_exc();
            return fallback();
        }
        if !jsv_is_string(&name) {
            JS_FreeValue(ctx, name);
            return fallback();
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
pub extern "C" fn v8__Name__GetIdentityHash(this: *const Name) -> int {
    // Hash by the value's atom: stable per distinct string/symbol and nonzero.
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return 1;
    }
    let atom = unsafe { JS_ValueToAtom(ctx, jsval_of(this)) };
    if atom == 0 {
        return 1;
    }
    unsafe { JS_FreeAtom(ctx, atom) };
    let h = (atom ^ 0x9e37_79b9).wrapping_mul(2654435761);
    ((h & 0x7fff_ffff) | 1) as int
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
    property_names_array(this, context, gpn_from_filter(&filter))
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
    // TODO(qjs): mode (own vs include-prototypes) is not honoured; QuickJS
    // JS_GetOwnPropertyNames is own-only — fine for the common deno path. We DO
    // honour the property filter: the console inspector asks once for enumerable
    // keys and again (for hidden) without the filter, so ignoring ONLY_ENUMERABLE
    // makes every property render twice.
    let _ = (mode, key_conversion);
    // IndexFilter::SkipIndices == 1.
    let skip_indices = index_filter as u32 == 1;
    property_names_array_filtered(
        this,
        context,
        gpn_from_filter(&property_filter),
        skip_indices,
    )
}

/// Map a v8 `PropertyFilter` bitset onto QuickJS `JS_GPN_*` flags.
/// v8 bits: ONLY_ENUMERABLE=2, SKIP_STRINGS=8, SKIP_SYMBOLS=16.
fn gpn_from_filter(filter: &PropertyFilter) -> c_int {
    let f = read_filter(filter);
    let mut gpn = 0;
    if f & 8 == 0 {
        gpn |= JS_GPN_STRING_MASK;
    }
    if f & 16 == 0 {
        gpn |= JS_GPN_SYMBOL_MASK;
    }
    if gpn == 0 {
        gpn = JS_GPN_STRING_MASK;
    }
    if f & 2 != 0 {
        gpn |= JS_GPN_ENUM_ONLY;
    }
    gpn
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
    property_names_array_filtered(this, context, gpn_flags, false)
}

/// QuickJS marks integer (array-index) atoms with the high bit `JS_ATOM_TAG_INT`.
const JS_ATOM_TAG_INT: u32 = 1 << 31;

fn property_names_array_filtered(
    this: *const Object,
    context: *const Context,
    gpn_flags: c_int,
    skip_indices: bool,
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
        let mut out = 0u32;
        for i in 0..len as usize {
            let atom = (*tab.add(i)).atom;
            // SkipIndices: drop integer (array-index) atoms so e.g. an array's
            // "0"/"1"/.. don't show up as extra named properties.
            if skip_indices && (atom & JS_ATOM_TAG_INT) != 0 {
                continue;
            }
            // JS_AtomToValue returns +1; JS_SetPropertyUint32 consumes it.
            let key_val = JS_AtomToValue(ctx, atom);
            JS_SetPropertyUint32(ctx, arr, out, key_val);
            out += 1;
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

/// Atom (+1) for an array index. Caller must `JS_FreeAtom`. 0 on failure.
#[inline]
fn index_atom(ctx: *mut JSContext, index: u32) -> JSAtom {
    let v = unsafe { JS_NewInt64(ctx, index as i64) };
    let a = unsafe { JS_ValueToAtom(ctx, v) };
    unsafe { JS_FreeValue(ctx, v) };
    a
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__HasIndex(
    this: *const Object,
    context: *const Context,
    index: u32,
) -> MaybeBool {
    let ctx = ctx_of(context);
    if ctx.is_null() || this.is_null() {
        return MaybeBool::Nothing;
    }
    let atom = index_atom(ctx, index);
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
pub extern "C" fn v8__Object__DeleteIndex(
    this: *const Object,
    context: *const Context,
    index: u32,
) -> MaybeBool {
    let ctx = ctx_of(context);
    if ctx.is_null() || this.is_null() {
        return MaybeBool::Nothing;
    }
    let atom = index_atom(ctx, index);
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
pub extern "C" fn v8__Object__HasPrivate(
    this: *const Object,
    context: *const Context,
    key: *const Private,
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
pub extern "C" fn v8__Object__DeletePrivate(
    this: *const Object,
    context: *const Context,
    key: *const Private,
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

// "Real" named property accessors. V8 distinguishes these from interceptor-aware
// lookups; QuickJS has no interceptors, so they reduce to the ordinary lookups.

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetRealNamedProperty(
    this: *const Object,
    context: *const Context,
    key: *const Name,
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
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return ptr::null();
    }
    // A missing real property should report empty, not `undefined`.
    if jsv_is_undefined(&v) {
        unsafe { JS_FreeValue(ctx, v) };
        return ptr::null();
    }
    intern::<Value>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__HasRealNamedProperty(
    this: *const Object,
    context: *const Context,
    key: *const Name,
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
pub extern "C" fn v8__Object__GetRealNamedPropertyAttributes(
    this: *const Object,
    context: *const Context,
    key: *const Name,
    out: *mut Maybe<PropertyAttribute>,
) {
    // QuickJS has no public per-property attribute query that walks the chain;
    // report NONE when the property exists, else no value (mirrors
    // GetPropertyAttributes).
    let ctx = ctx_of(context);
    if ctx.is_null() || this.is_null() {
        unsafe { maybe_attr_none(out) };
        return;
    }
    let atom = key_atom(ctx, key);
    if atom == 0 {
        unsafe { maybe_attr_none(out) };
        return;
    }
    let r = unsafe { JS_HasProperty(ctx, jsval_of(this), atom) };
    unsafe { JS_FreeAtom(ctx, atom) };
    if r == 1 {
        unsafe { maybe_attr_set(out, 0) }; // PropertyAttribute::NONE
    } else {
        unsafe { maybe_attr_none(out) };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetCreationContext(this: *const Object) -> *const Context {
    // Single-context model: every object belongs to the current context.
    let _ = this;
    let ctx = current_ctx();
    if ctx.is_null() {
        return ptr::null();
    }
    super::shim_core::intern_ctx(ctx)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetIntegrityLevel(
    this: *const Object,
    context: *const Context,
    level: crate::IntegrityLevel,
) -> MaybeBool {
    let ctx = ctx_of(context);
    if ctx.is_null() || this.is_null() {
        return MaybeBool::Nothing;
    }
    // Apply via Object.freeze / Object.seal.
    let method: &[u8] = match level {
        crate::IntegrityLevel::Frozen => b"freeze\0",
        crate::IntegrityLevel::Sealed => b"seal\0",
    };
    unsafe {
        let global = JS_GetGlobalObject(ctx);
        let object_ctor = JS_GetPropertyStr(ctx, global, c"Object".as_ptr());
        JS_FreeValue(ctx, global);
        let func = JS_GetPropertyStr(ctx, object_ctor, method.as_ptr() as *const c_char);
        JS_FreeValue(ctx, object_ctor);
        if !jsv_is_object(&func) {
            JS_FreeValue(ctx, func);
            return MaybeBool::Nothing;
        }
        let mut args = [JS_DupValue(ctx, jsval_of(this))];
        let r = JS_Call(ctx, func, jsv_undefined(), 1, args.as_mut_ptr());
        JS_FreeValue(ctx, func);
        JS_FreeValue(ctx, args[0]);
        if r.tag == JS_TAG_EXCEPTION {
            let exc = JS_GetException(ctx);
            JS_FreeValue(ctx, exc);
            return MaybeBool::Nothing;
        }
        JS_FreeValue(ctx, r);
        MaybeBool::JustTrue
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__PreviewEntries(
    this: *const Object,
    is_key_value: *mut bool,
) -> *const Array {
    // Used by the inspector to preview Map/Set/iterator contents. We synthesize
    // the entries array via JS: for Map/Set, `[...obj]`; for iterators, drain a
    // clone. This is best-effort; on any failure we report an empty array.
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        if !is_key_value.is_null() {
            unsafe { *is_key_value = false };
        }
        return ptr::null();
    }
    unsafe {
        // Determine if it's a Map (key/value pairs) vs Set/array (values) by
        // testing for a `.set` method (Map/WeakMap) — best effort.
        let is_map = {
            let m = JS_GetPropertyStr(ctx, jsval_of(this), c"set".as_ptr());
            let is_fn = JS_IsFunction(ctx, m) != 0;
            JS_FreeValue(ctx, m);
            is_fn
        };
        if !is_key_value.is_null() {
            *is_key_value = is_map;
        }
        // `Array.from(this)` collects entries from any iterable.
        let global = JS_GetGlobalObject(ctx);
        let array_ctor = JS_GetPropertyStr(ctx, global, c"Array".as_ptr());
        JS_FreeValue(ctx, global);
        let from = JS_GetPropertyStr(ctx, array_ctor, c"from".as_ptr());
        if !jsv_is_object(&from) {
            JS_FreeValue(ctx, from);
            JS_FreeValue(ctx, array_ctor);
            return ptr::null();
        }
        let mut args = [JS_DupValue(ctx, jsval_of(this))];
        let r = JS_Call(ctx, from, array_ctor, 1, args.as_mut_ptr());
        JS_FreeValue(ctx, from);
        JS_FreeValue(ctx, array_ctor);
        JS_FreeValue(ctx, args[0]);
        if r.tag == JS_TAG_EXCEPTION {
            let exc = JS_GetException(ctx);
            JS_FreeValue(ctx, exc);
            return ptr::null();
        }
        intern::<Array>(r)
    }
}

#[inline]
unsafe fn maybe_attr_set(out: *mut Maybe<PropertyAttribute>, bits: u32) {
    if out.is_null() {
        return;
    }
    // Maybe<T> is #[repr(C)] { has_value: bool, value: T }; PropertyAttribute is
    // a #[repr(C)] u32 wrapper.
    unsafe {
        let p = out as *mut u8;
        *(p as *mut bool) = true;
        let voff = std::mem::offset_of!(MaybeAttrMirror, value);
        *(p.add(voff) as *mut u32) = bits;
    }
}

#[inline]
unsafe fn maybe_attr_none(out: *mut Maybe<PropertyAttribute>) {
    if out.is_null() {
        return;
    }
    unsafe {
        *(out as *mut bool) = false;
    }
}

#[repr(C)]
struct MaybeAttrMirror {
    has_value: bool,
    value: PropertyAttribute,
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
