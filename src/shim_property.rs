// Family: "property" — PropertyDescriptor construct/destruct/getters/setters.
//
// The vendored `PropertyDescriptor` is `#[repr(transparent)] struct([usize;1])`
// — i.e. a single machine word that the C++ side normally uses to hold a
// pointer to a private impl struct. We mirror that: the one usize stores a
// raw pointer to a heap-allocated `PdImpl` that we own. CONSTRUCT boxes one
// and writes the pointer into the out slot (placement-new semantics); DESTRUCT
// frees it.
//
// Stored js value handles (value/get/set) are JSC `JSValueRef`s protected
// against the current context for the lifetime of the descriptor, and
// unprotected on DESTRUCT. Getters re-intern them into the handle scope so the
// returned `*const Value` is GC-rooted per the crate rule.
#![allow(non_snake_case, unused)]

use crate::jsc_sys::*;
use crate::shim_core::{current_ctx, current_iso, intern, jsval};
use crate::Value;

// The vendored repr-transparent newtype around [usize; 1].
#[repr(transparent)]
pub struct PropertyDescriptor([usize; 1]);

// Our private backing struct. Booleans track presence (`has_*`) and value of
// each attribute, mirroring v8::PropertyDescriptor's PrivateData.
struct PdImpl {
    ctx: JSContextRef,

    has_enumerable: bool,
    enumerable: bool,
    has_configurable: bool,
    configurable: bool,
    has_writable: bool,
    writable: bool,

    // Protected JSValueRefs (or null if absent).
    value: JSValueRef,
    has_value: bool,
    get: JSValueRef,
    has_get: bool,
    set: JSValueRef,
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
            value: std::ptr::null(),
            has_value: false,
            get: std::ptr::null(),
            has_get: false,
            set: std::ptr::null(),
            has_set: false,
        }
    }
}

#[inline]
unsafe fn imp<'a>(this: *const PropertyDescriptor) -> &'a PdImpl {
    let p = (*this).0[0] as *const PdImpl;
    &*p
}

#[inline]
unsafe fn imp_mut<'a>(this: *mut PropertyDescriptor) -> &'a mut PdImpl {
    let p = (*this).0[0] as *mut PdImpl;
    &mut *p
}

#[inline]
unsafe fn write_impl(out: *mut PropertyDescriptor, pd: PdImpl) {
    let boxed = Box::into_raw(Box::new(pd));
    (*out).0[0] = boxed as usize;
}

#[inline]
unsafe fn protect(ctx: JSContextRef, v: JSValueRef) {
    if !v.is_null() {
        JSValueProtect(ctx, v);
    }
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
        let v = jsval(value);
        protect(pd.ctx, v);
        pd.value = v;
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
        let v = jsval(value);
        protect(pd.ctx, v);
        pd.value = v;
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
        let g = jsval(get);
        let s = jsval(set);
        protect(pd.ctx, g);
        protect(pd.ctx, s);
        pd.get = g;
        pd.has_get = true;
        pd.set = s;
        pd.has_set = true;
        write_impl(this as *mut PropertyDescriptor, pd);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__DESTRUCT(this: *mut PropertyDescriptor) {
    unsafe {
        let raw = (*this).0[0] as *mut PdImpl;
        if raw.is_null() {
            return;
        }
        let pd = Box::from_raw(raw);
        if !pd.value.is_null() {
            JSValueUnprotect(pd.ctx, pd.value);
        }
        if !pd.get.is_null() {
            JSValueUnprotect(pd.ctx, pd.get);
        }
        if !pd.set.is_null() {
            JSValueUnprotect(pd.ctx, pd.set);
        }
        (*this).0[0] = 0;
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
        let v = imp(this).value;
        if v.is_null() {
            return std::ptr::null();
        }
        intern::<Value>(v)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__get(
    this: *const PropertyDescriptor,
) -> *const Value {
    unsafe {
        let v = imp(this).get;
        if v.is_null() {
            return std::ptr::null();
        }
        intern::<Value>(v)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyDescriptor__set(
    this: *const PropertyDescriptor,
) -> *const Value {
    unsafe {
        let v = imp(this).set;
        if v.is_null() {
            return std::ptr::null();
        }
        intern::<Value>(v)
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
