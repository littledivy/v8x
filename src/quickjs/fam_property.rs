// Family: "property" — PropertyDescriptor construct/destruct/getters/setters,
// QuickJS-ng backend.
//
// The vendored `PropertyDescriptor` is `#[repr(transparent)] struct([usize; 1])`
// — a single machine word the C++ side normally uses to hold a pointer to a
// private impl struct. We mirror that: the one usize stores a raw pointer to a
// heap-allocated `PdImpl` that we own. CONSTRUCT boxes one and writes the
// pointer into the out slot (placement-new semantics); DESTRUCT frees it.
//
// Unlike the JSC backend (where a `JSValueRef` is a pointer protected against
// the context), QuickJS `JSValue` is a 16-byte struct that carries its own
// refcount. So we store an OWNED (`JS_DupValue`'d) `JSValue` for value/get/set
// and drop that refcount with `JS_FreeValue` on DESTRUCT. Presence is tracked
// solely by the `has_*` booleans (a `JSValue` can't be a null pointer; absent
// slots hold `undefined`). Getters re-intern the stored value into the current
// handle scope so the returned `*const Value` is GC-rooted per the crate rule.
#![allow(non_snake_case, unused)]

use crate::quickjs::quickjs_sys::*;
use crate::quickjs::shim_core::{current_ctx, intern_dup, jsval_of};
use crate::{PropertyDescriptor, Value};

// Our private backing struct. Booleans track presence (`has_*`) and value of
// each attribute, mirroring v8::PropertyDescriptor's PrivateData.
struct PdImpl {
    // Context owning the refcounts on value/get/set; used to free them.
    ctx: *mut JSContext,

    has_enumerable: bool,
    enumerable: bool,
    has_configurable: bool,
    configurable: bool,
    has_writable: bool,
    writable: bool,

    // Owned (+1) JSValues, valid only when the corresponding `has_*` is set.
    value: JSValue,
    has_value: bool,
    get: JSValue,
    has_get: bool,
    set: JSValue,
    has_set: bool,
}

impl PdImpl {
    fn empty() -> Self {
        PdImpl {
            ctx: current_ctx(),
            has_enumerable: false,
            enumerable: false,
            has_configurable: false,
            configurable: false,
            has_writable: false,
            writable: false,
            value: jsv_undefined(),
            has_value: false,
            get: jsv_undefined(),
            has_get: false,
            set: jsv_undefined(),
            has_set: false,
        }
    }
}

#[inline]
unsafe fn imp<'a>(this: *const PropertyDescriptor) -> &'a PdImpl {
    let p = *(this as *const usize) as *const PdImpl;
    &*p
}

#[inline]
unsafe fn imp_mut<'a>(this: *mut PropertyDescriptor) -> &'a mut PdImpl {
    let p = *(this as *const usize) as *mut PdImpl;
    &mut *p
}

#[inline]
unsafe fn write_impl(out: *mut PropertyDescriptor, pd: PdImpl) {
    let boxed = Box::into_raw(Box::new(pd));
    *(out as *mut usize) = boxed as usize;
}

/// Take an OWNED (+1) refcount on a borrowed handle's JSValue, for storage.
#[inline]
unsafe fn dup_handle(ctx: *mut JSContext, v: *const Value) -> JSValue {
    let jv = jsval_of(v);
    if ctx.is_null() {
        return jv;
    }
    JS_DupValue(ctx, jv)
}

// ----- Constructors -----

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__CONSTRUCT(out: *mut PropertyDescriptor) {
    unsafe {
        write_impl(out, PdImpl::empty());
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__CONSTRUCT__Value(
    this: *const PropertyDescriptor,
    value: *const Value,
) {
    unsafe {
        let mut pd = PdImpl::empty();
        pd.value = dup_handle(pd.ctx, value);
        pd.has_value = true;
        write_impl(this as *mut PropertyDescriptor, pd);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__CONSTRUCT__Value_Writable(
    this: *const PropertyDescriptor,
    value: *const Value,
    writable: bool,
) {
    unsafe {
        let mut pd = PdImpl::empty();
        pd.value = dup_handle(pd.ctx, value);
        pd.has_value = true;
        pd.writable = writable;
        pd.has_writable = true;
        write_impl(this as *mut PropertyDescriptor, pd);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__CONSTRUCT__Get_Set(
    this: *const PropertyDescriptor,
    get: *const Value,
    set: *const Value,
) {
    unsafe {
        let mut pd = PdImpl::empty();
        pd.get = dup_handle(pd.ctx, get);
        pd.has_get = true;
        pd.set = dup_handle(pd.ctx, set);
        pd.has_set = true;
        write_impl(this as *mut PropertyDescriptor, pd);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__DESTRUCT(this: *mut PropertyDescriptor) {
    unsafe {
        let slot = this as *mut usize;
        let raw = *slot as *mut PdImpl;
        if raw.is_null() {
            return;
        }
        let pd = Box::from_raw(raw);
        if !pd.ctx.is_null() {
            if pd.has_value {
                JS_FreeValue(pd.ctx, pd.value);
            }
            if pd.has_get {
                JS_FreeValue(pd.ctx, pd.get);
            }
            if pd.has_set {
                JS_FreeValue(pd.ctx, pd.set);
            }
        }
        *slot = 0;
        // pd dropped here, freeing the box.
    }
}

// ----- Attribute value getters -----

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__configurable(
    this: *const PropertyDescriptor,
) -> bool {
    unsafe { imp(this).configurable }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__enumerable(
    this: *const PropertyDescriptor,
) -> bool {
    unsafe { imp(this).enumerable }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__writable(
    this: *const PropertyDescriptor,
) -> bool {
    unsafe { imp(this).writable }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__value(
    this: *const PropertyDescriptor,
) -> *const Value {
    unsafe {
        let pd = imp(this);
        if !pd.has_value {
            return std::ptr::null();
        }
        // Borrowed read: dup into the current handle scope.
        intern_dup::<Value>(pd.ctx, pd.value)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__get(
    this: *const PropertyDescriptor,
) -> *const Value {
    unsafe {
        let pd = imp(this);
        if !pd.has_get {
            return std::ptr::null();
        }
        intern_dup::<Value>(pd.ctx, pd.get)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__set(
    this: *const PropertyDescriptor,
) -> *const Value {
    unsafe {
        let pd = imp(this);
        if !pd.has_set {
            return std::ptr::null();
        }
        intern_dup::<Value>(pd.ctx, pd.set)
    }
}

// ----- Presence ("has_*") getters -----

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__has_configurable(
    this: *const PropertyDescriptor,
) -> bool {
    unsafe { imp(this).has_configurable }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__has_enumerable(
    this: *const PropertyDescriptor,
) -> bool {
    unsafe { imp(this).has_enumerable }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__has_writable(
    this: *const PropertyDescriptor,
) -> bool {
    unsafe { imp(this).has_writable }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__has_value(
    this: *const PropertyDescriptor,
) -> bool {
    unsafe { imp(this).has_value }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__has_get(
    this: *const PropertyDescriptor,
) -> bool {
    unsafe { imp(this).has_get }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__has_set(
    this: *const PropertyDescriptor,
) -> bool {
    unsafe { imp(this).has_set }
}

// ----- Setters -----

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__set_enumerable(
    this: *mut PropertyDescriptor,
    enumerable: bool,
) {
    unsafe {
        let pd = imp_mut(this);
        pd.enumerable = enumerable;
        pd.has_enumerable = true;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__set_configurable(
    this: *mut PropertyDescriptor,
    configurable: bool,
) {
    unsafe {
        let pd = imp_mut(this);
        pd.configurable = configurable;
        pd.has_configurable = true;
    }
}

// ----- Apply a descriptor to an object (for v8__Object__DefineProperty) -----

use std::os::raw::c_int;

unsafe extern "C" {
    // JSValueConst val/getter/setter: NOT consumed by JS_DefineProperty.
    fn JS_DefineProperty(
        ctx: *mut JSContext,
        this_obj: JSValue,
        prop: JSAtom,
        val: JSValue,
        getter: JSValue,
        setter: JSValue,
        flags: c_int,
    ) -> c_int;
}

const JS_PROP_CONFIGURABLE: c_int = 1 << 0;
const JS_PROP_WRITABLE: c_int = 1 << 1;
const JS_PROP_ENUMERABLE: c_int = 1 << 2;
const JS_PROP_HAS_CONFIGURABLE: c_int = 1 << 8;
const JS_PROP_HAS_WRITABLE: c_int = 1 << 9;
const JS_PROP_HAS_ENUMERABLE: c_int = 1 << 10;
const JS_PROP_HAS_GET: c_int = 1 << 11;
const JS_PROP_HAS_SET: c_int = 1 << 12;
const JS_PROP_HAS_VALUE: c_int = 1 << 13;
const JS_PROP_THROW: c_int = 1 << 14;

/// Define the property described by `this` onto `obj` under `atom`, translating
/// our `PdImpl` presence/flags into QuickJS `JS_DefineProperty` flags. Returns
/// the `JS_DefineProperty` result (<0 = exception, 0 = false, >0 = true).
pub(crate) fn pd_define(
    ctx: *mut JSContext,
    obj: JSValue,
    atom: JSAtom,
    this: *const PropertyDescriptor,
) -> c_int {
    let pd = unsafe { imp(this) };
    let mut flags: c_int = JS_PROP_THROW;
    let mut val = jsv_undefined();
    let mut getter = jsv_undefined();
    let mut setter = jsv_undefined();

    if pd.has_value {
        flags |= JS_PROP_HAS_VALUE;
        val = pd.value;
    }
    if pd.has_get {
        flags |= JS_PROP_HAS_GET;
        getter = pd.get;
    }
    if pd.has_set {
        flags |= JS_PROP_HAS_SET;
        setter = pd.set;
    }
    if pd.has_writable {
        flags |= JS_PROP_HAS_WRITABLE;
        if pd.writable {
            flags |= JS_PROP_WRITABLE;
        }
    }
    if pd.has_enumerable {
        flags |= JS_PROP_HAS_ENUMERABLE;
        if pd.enumerable {
            flags |= JS_PROP_ENUMERABLE;
        }
    }
    if pd.has_configurable {
        flags |= JS_PROP_HAS_CONFIGURABLE;
        if pd.configurable {
            flags |= JS_PROP_CONFIGURABLE;
        }
    }
    unsafe { JS_DefineProperty(ctx, obj, atom, val, getter, setter, flags) }
}
