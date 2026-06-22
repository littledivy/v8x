// Family: object — v8::Object Get/Set/Has/Delete/CreateDataProperty/etc + v8::Array.
#![allow(non_snake_case, unused)]

use crate::jsc_sys::*;
use crate::support::{int, Maybe, MaybeBool};
use crate::{
    Array, Context, KeyCollectionMode, KeyConversionMode, IndexFilter, Name, Object, Private,
    PropertyAttribute, PropertyDescriptor, PropertyFilter, RealIsolate, String, Value,
};
use crate::shim_core::{ctx_of, current_ctx, current_iso, intern, intern_ctx, iso_state, jsval};
use std::os::raw::{c_char, c_void};
use std::ptr;

// JSC C API functions not declared in jsc_sys.rs.
unsafe extern "C" {
    fn JSObjectMake(
        ctx: JSContextRef,
        jsClass: JSClassRef,
        data: *mut c_void,
    ) -> JSObjectRef;
    fn JSObjectMakeArray(
        ctx: JSContextRef,
        argumentCount: usize,
        arguments: *const JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSObjectRef;
    fn JSValueToObject(
        ctx: JSContextRef,
        value: JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSObjectRef;
    fn JSObjectGetProperty(
        ctx: JSContextRef,
        object: JSObjectRef,
        propertyName: JSStringRef,
        exception: *mut JSValueRef,
    ) -> JSValueRef;
    fn JSObjectSetProperty(
        ctx: JSContextRef,
        object: JSObjectRef,
        propertyName: JSStringRef,
        value: JSValueRef,
        attributes: u32,
        exception: *mut JSValueRef,
    );
    fn JSObjectHasProperty(
        ctx: JSContextRef,
        object: JSObjectRef,
        propertyName: JSStringRef,
    ) -> bool;
    fn JSObjectDeleteProperty(
        ctx: JSContextRef,
        object: JSObjectRef,
        propertyName: JSStringRef,
        exception: *mut JSValueRef,
    ) -> bool;
    fn JSObjectGetPropertyAtIndex(
        ctx: JSContextRef,
        object: JSObjectRef,
        propertyIndex: u32,
        exception: *mut JSValueRef,
    ) -> JSValueRef;
    fn JSObjectSetPropertyAtIndex(
        ctx: JSContextRef,
        object: JSObjectRef,
        propertyIndex: u32,
        value: JSValueRef,
        exception: *mut JSValueRef,
    );
    fn JSObjectGetPrototype(ctx: JSContextRef, object: JSObjectRef) -> JSValueRef;
    fn JSObjectSetPrototype(ctx: JSContextRef, object: JSObjectRef, value: JSValueRef);
    // Property name enumeration.
    fn JSObjectCopyPropertyNames(
        ctx: JSContextRef,
        object: JSObjectRef,
    ) -> JSPropertyNameArrayRef;
    fn JSPropertyNameArrayGetCount(array: JSPropertyNameArrayRef) -> usize;
    fn JSPropertyNameArrayGetNameAtIndex(
        array: JSPropertyNameArrayRef,
        index: usize,
    ) -> JSStringRef;
    fn JSPropertyNameArrayRelease(array: JSPropertyNameArrayRef);
    // Key-based property access using a JSValueRef as key (handles symbols/numbers).
    fn JSObjectGetPropertyForKey(
        ctx: JSContextRef,
        object: JSObjectRef,
        propertyKey: JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSValueRef;
    fn JSObjectSetPropertyForKey(
        ctx: JSContextRef,
        object: JSObjectRef,
        propertyKey: JSValueRef,
        value: JSValueRef,
        attributes: u32,
        exception: *mut JSValueRef,
    );
    fn JSObjectHasPropertyForKey(
        ctx: JSContextRef,
        object: JSObjectRef,
        propertyKey: JSValueRef,
        exception: *mut JSValueRef,
    ) -> bool;
    fn JSObjectDeletePropertyForKey(
        ctx: JSContextRef,
        object: JSObjectRef,
        propertyKey: JSValueRef,
        exception: *mut JSValueRef,
    ) -> bool;
}

type JSPropertyNameArrayRef = *mut OpaqueJSPropertyNameArray;

// ---- helpers ----------------------------------------------------------------

#[inline]
fn obj_of(ctx: JSContextRef, p: *const Object) -> JSObjectRef {
    // A handle pointer IS a JSValueRef; coerce to an object.
    let v = jsval(p);
    if v.is_null() {
        return ptr::null_mut();
    }
    let mut exc: JSValueRef = ptr::null();
    unsafe { JSValueToObject(ctx, v, &mut exc) }
}

#[inline]
fn just_bool(b: bool) -> MaybeBool {
    if b { MaybeBool::JustTrue } else { MaybeBool::JustFalse }
}

// ===================================================================
// Object
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__New(isolate: *mut RealIsolate) -> *const Object {
    let st = iso_state(isolate);
    let ctx = st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
    if ctx.is_null() {
        return ptr::null();
    }
    let o = unsafe { JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut()) };
    intern_ctx::<Object>(ctx, o as JSValueRef)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__New__with_prototype_and_properties(
    isolate: *mut RealIsolate,
    prototype_or_null: *const Value,
    names: *const *const Name,
    values: *const *const Value,
    length: usize,
) -> *const Object {
    let st = iso_state(isolate);
    let ctx = st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
    if ctx.is_null() {
        return ptr::null();
    }
    let o = unsafe { JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut()) };
    if !prototype_or_null.is_null() {
        let proto = jsval(prototype_or_null);
        unsafe { JSObjectSetPrototype(ctx, o, proto) };
    }
    unsafe {
        for i in 0..length {
            let key = *names.add(i);
            let val = *values.add(i);
            let mut exc: JSValueRef = ptr::null();
            JSObjectSetPropertyForKey(ctx, o, jsval(key), jsval(val), 0, &mut exc);
        }
    }
    intern_ctx::<Object>(ctx, o as JSValueRef)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__Get(
    this: *const Object,
    context: *const Context,
    key: *const Value,
) -> *const Value {
    let ctx = ctx_of(context) as JSContextRef;
    let o = obj_of(ctx, this);
    if o.is_null() {
        return ptr::null();
    }
    let mut exc: JSValueRef = ptr::null();
    let v = unsafe { JSObjectGetPropertyForKey(ctx, o, jsval(key), &mut exc) };
    if !exc.is_null() {
        return ptr::null();
    }
    intern_ctx::<Value>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetIndex(
    this: *const Object,
    context: *const Context,
    index: u32,
) -> *const Value {
    let ctx = ctx_of(context) as JSContextRef;
    let o = obj_of(ctx, this);
    if o.is_null() {
        return ptr::null();
    }
    let mut exc: JSValueRef = ptr::null();
    let v = unsafe { JSObjectGetPropertyAtIndex(ctx, o, index, &mut exc) };
    if !exc.is_null() {
        return ptr::null();
    }
    intern_ctx::<Value>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetPrototype(this: *const Object) -> *const Value {
    let ctx = current_ctx();
    let o = obj_of(ctx, this);
    if o.is_null() {
        return ptr::null();
    }
    let v = unsafe { JSObjectGetPrototype(ctx, o) };
    intern_ctx::<Value>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__Set(
    this: *const Object,
    context: *const Context,
    key: *const Value,
    value: *const Value,
) -> MaybeBool {
    let ctx = ctx_of(context) as JSContextRef;
    let o = obj_of(ctx, this);
    if o.is_null() {
        return MaybeBool::Nothing;
    }
    let mut exc: JSValueRef = ptr::null();
    unsafe { JSObjectSetPropertyForKey(ctx, o, jsval(key), jsval(value), 0, &mut exc) };
    if !exc.is_null() {
        return MaybeBool::Nothing;
    }
    MaybeBool::JustTrue
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetIndex(
    this: *const Object,
    context: *const Context,
    index: u32,
    value: *const Value,
) -> MaybeBool {
    let ctx = ctx_of(context) as JSContextRef;
    let o = obj_of(ctx, this);
    if o.is_null() {
        return MaybeBool::Nothing;
    }
    let mut exc: JSValueRef = ptr::null();
    unsafe { JSObjectSetPropertyAtIndex(ctx, o, index, jsval(value), &mut exc) };
    if !exc.is_null() {
        return MaybeBool::Nothing;
    }
    MaybeBool::JustTrue
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetPrototype(
    this: *const Object,
    context: *const Context,
    prototype: *const Value,
) -> MaybeBool {
    let ctx = ctx_of(context) as JSContextRef;
    let o = obj_of(ctx, this);
    if o.is_null() {
        return MaybeBool::Nothing;
    }
    unsafe { JSObjectSetPrototype(ctx, o, jsval(prototype)) };
    MaybeBool::JustTrue
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__CreateDataProperty(
    this: *const Object,
    context: *const Context,
    key: *const Name,
    value: *const Value,
) -> MaybeBool {
    let ctx = ctx_of(context) as JSContextRef;
    let o = obj_of(ctx, this);
    if o.is_null() {
        return MaybeBool::Nothing;
    }
    let mut exc: JSValueRef = ptr::null();
    unsafe { JSObjectSetPropertyForKey(ctx, o, jsval(key), jsval(value), 0, &mut exc) };
    if !exc.is_null() {
        return MaybeBool::Nothing;
    }
    MaybeBool::JustTrue
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__DefineOwnProperty(
    this: *const Object,
    context: *const Context,
    key: *const Name,
    value: *const Value,
    attr: PropertyAttribute,
) -> MaybeBool {
    let ctx = ctx_of(context) as JSContextRef;
    let o = obj_of(ctx, this);
    if o.is_null() {
        return MaybeBool::Nothing;
    }
    // Map v8 PropertyAttribute bits onto JSC kJSPropertyAttribute* bits:
    // JSC: ReadOnly=1<<1, DontEnum=1<<2, DontDelete=1<<3.
    let a = attr.as_u32();
    let mut jsc_attr: u32 = 0;
    if a & (1 << 0) != 0 { jsc_attr |= 1 << 1; } // READ_ONLY
    if a & (1 << 1) != 0 { jsc_attr |= 1 << 2; } // DONT_ENUM
    if a & (1 << 2) != 0 { jsc_attr |= 1 << 3; } // DONT_DELETE
    let mut exc: JSValueRef = ptr::null();
    unsafe { JSObjectSetPropertyForKey(ctx, o, jsval(key), jsval(value), jsc_attr, &mut exc) };
    if !exc.is_null() {
        return MaybeBool::Nothing;
    }
    MaybeBool::JustTrue
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__DefineProperty(
    this: *const Object,
    context: *const Context,
    key: *const Name,
    desc: *const PropertyDescriptor,
) -> MaybeBool {
    // TODO(v82jsc): full PropertyDescriptor (get/set/value/flags) support requires
    // reading the v8 PropertyDescriptor internals. Inert: report failure.
    MaybeBool::Nothing
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__Delete(
    this: *const Object,
    context: *const Context,
    key: *const Value,
) -> MaybeBool {
    let ctx = ctx_of(context) as JSContextRef;
    let o = obj_of(ctx, this);
    if o.is_null() {
        return MaybeBool::Nothing;
    }
    let mut exc: JSValueRef = ptr::null();
    let r = unsafe { JSObjectDeletePropertyForKey(ctx, o, jsval(key), &mut exc) };
    if !exc.is_null() {
        return MaybeBool::Nothing;
    }
    just_bool(r)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__Has(
    this: *const Object,
    context: *const Context,
    key: *const Value,
) -> MaybeBool {
    let ctx = ctx_of(context) as JSContextRef;
    let o = obj_of(ctx, this);
    if o.is_null() {
        return MaybeBool::Nothing;
    }
    let mut exc: JSValueRef = ptr::null();
    let r = unsafe { JSObjectHasPropertyForKey(ctx, o, jsval(key), &mut exc) };
    if !exc.is_null() {
        return MaybeBool::Nothing;
    }
    just_bool(r)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__HasOwnProperty(
    this: *const Object,
    context: *const Context,
    key: *const Name,
) -> MaybeBool {
    // JSC has no direct "own" check via C API; approximate with HasPropertyForKey.
    // TODO(v82jsc): distinguish own vs inherited properties.
    let ctx = ctx_of(context) as JSContextRef;
    let o = obj_of(ctx, this);
    if o.is_null() {
        return MaybeBool::Nothing;
    }
    let mut exc: JSValueRef = ptr::null();
    let r = unsafe { JSObjectHasPropertyForKey(ctx, o, jsval(key), &mut exc) };
    if !exc.is_null() {
        return MaybeBool::Nothing;
    }
    just_bool(r)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetConstructorName(this: *const Object) -> *const String {
    let ctx = current_ctx();
    let o = obj_of(ctx, this);
    if o.is_null() {
        return ptr::null();
    }
    // Read obj.constructor.name.
    let cname = b"constructor\0";
    let nname = b"name\0";
    unsafe {
        let cs = JSStringCreateWithUTF8CString(cname.as_ptr() as *const c_char);
        let mut exc: JSValueRef = ptr::null();
        let ctor = JSObjectGetProperty(ctx, o, cs, &mut exc);
        JSStringRelease(cs);
        if exc.is_null() && JSValueIsObject(ctx, ctor) {
            let ctor_o = JSValueToObject(ctx, ctor, &mut exc);
            if !ctor_o.is_null() {
                let ns = JSStringCreateWithUTF8CString(nname.as_ptr() as *const c_char);
                let name = JSObjectGetProperty(ctx, ctor_o, ns, &mut exc);
                JSStringRelease(ns);
                if exc.is_null() && JSValueIsString(ctx, name) {
                    return intern_ctx::<String>(ctx, name);
                }
            }
        }
    }
    ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetIdentityHash(this: *const Object) -> int {
    // Use the pointer identity as a stable, nonzero-ish hash.
    let p = jsval(this) as usize;
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
    property_names_array(this, context)
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
    property_names_array(this, context)
}

fn property_names_array(this: *const Object, context: *const Context) -> *const Array {
    let ctx = ctx_of(context) as JSContextRef;
    let o = obj_of(ctx, this);
    if o.is_null() {
        return ptr::null();
    }
    unsafe {
        let names = JSObjectCopyPropertyNames(ctx, o);
        let count = JSPropertyNameArrayGetCount(names);
        let mut vals: Vec<JSValueRef> = Vec::with_capacity(count);
        for i in 0..count {
            let s = JSPropertyNameArrayGetNameAtIndex(names, i);
            let v = JSValueMakeString(ctx, s);
            vals.push(v);
        }
        JSPropertyNameArrayRelease(names);
        let mut exc: JSValueRef = ptr::null();
        let arr = JSObjectMakeArray(ctx, vals.len(), vals.as_ptr(), &mut exc);
        if !exc.is_null() || arr.is_null() {
            return ptr::null();
        }
        intern_ctx::<Array>(ctx, arr as JSValueRef)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetPrivate(
    this: *const Object,
    context: *const Context,
    key: *const Private,
) -> *const Value {
    // A v8 Private handle is backed by a JSValueRef (a symbol). Read by key.
    let ctx = ctx_of(context) as JSContextRef;
    let o = obj_of(ctx, this);
    if o.is_null() {
        return ptr::null();
    }
    let mut exc: JSValueRef = ptr::null();
    let v = unsafe { JSObjectGetPropertyForKey(ctx, o, jsval(key), &mut exc) };
    if !exc.is_null() {
        return ptr::null();
    }
    intern_ctx::<Value>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetPrivate(
    this: *const Object,
    context: *const Context,
    key: *const Private,
    value: *const Value,
) -> MaybeBool {
    let ctx = ctx_of(context) as JSContextRef;
    let o = obj_of(ctx, this);
    if o.is_null() {
        return MaybeBool::Nothing;
    }
    // DontEnum so private data doesn't leak into enumeration.
    let mut exc: JSValueRef = ptr::null();
    unsafe { JSObjectSetPropertyForKey(ctx, o, jsval(key), jsval(value), 1 << 2, &mut exc) };
    if !exc.is_null() {
        return MaybeBool::Nothing;
    }
    MaybeBool::JustTrue
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetPropertyAttributes(
    this: *const Object,
    context: *const Context,
    key: *const Value,
    out: *mut Maybe<PropertyAttribute>,
) {
    // TODO(v82jsc): JSC C API does not expose per-property attribute bits.
    // Report NONE if the property exists, otherwise no value.
    if out.is_null() {
        return;
    }
    let ctx = ctx_of(context) as JSContextRef;
    let o = obj_of(ctx, this);
    let mut exc: JSValueRef = ptr::null();
    let has = if o.is_null() {
        false
    } else {
        unsafe { JSObjectHasPropertyForKey(ctx, o, jsval(key), &mut exc) }
    };
    let m = if has && exc.is_null() {
        let mut tmp: Maybe<PropertyAttribute> = unsafe { std::mem::zeroed() };
        // tmp is { has_value:false, value:NONE(0) }; set has_value=true.
        unsafe { *(&mut tmp as *mut Maybe<PropertyAttribute> as *mut bool) = true; }
        tmp
    } else {
        unsafe { std::mem::zeroed() }
    };
    unsafe { ptr::write(out, m); }
}

// ===================================================================
// Array
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Array__New(isolate: *mut RealIsolate, length: int) -> *const Array {
    let st = iso_state(isolate);
    let ctx = st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
    if ctx.is_null() {
        return ptr::null();
    }
    let len = if length < 0 { 0usize } else { length as usize };
    unsafe {
        let mut exc: JSValueRef = ptr::null();
        let arr = if len == 0 {
            JSObjectMakeArray(ctx, 0, ptr::null(), &mut exc)
        } else {
            let undef = JSValueMakeUndefined(ctx);
            let vals = vec![undef; len];
            JSObjectMakeArray(ctx, vals.len(), vals.as_ptr(), &mut exc)
        };
        if !exc.is_null() || arr.is_null() {
            return ptr::null();
        }
        intern_ctx::<Array>(ctx, arr as JSValueRef)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Array__New_with_elements(
    isolate: *mut RealIsolate,
    elements: *const *const Value,
    length: usize,
) -> *const Array {
    let st = iso_state(isolate);
    let ctx = st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
    if ctx.is_null() {
        return ptr::null();
    }
    unsafe {
        let mut vals: Vec<JSValueRef> = Vec::with_capacity(length);
        for i in 0..length {
            vals.push(jsval(*elements.add(i)));
        }
        let mut exc: JSValueRef = ptr::null();
        let arr = JSObjectMakeArray(ctx, vals.len(), vals.as_ptr(), &mut exc);
        if !exc.is_null() || arr.is_null() {
            return ptr::null();
        }
        intern_ctx::<Array>(ctx, arr as JSValueRef)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Array__Length(array: *const Array) -> u32 {
    let ctx = current_ctx();
    if ctx.is_null() {
        return 0;
    }
    let o = obj_of(ctx, array as *const Object);
    if o.is_null() {
        return 0;
    }
    unsafe {
        let ls = JSStringCreateWithUTF8CString(b"length\0".as_ptr() as *const c_char);
        let mut exc: JSValueRef = ptr::null();
        let lv = JSObjectGetProperty(ctx, o, ls, &mut exc);
        JSStringRelease(ls);
        if !exc.is_null() {
            return 0;
        }
        let n = JSValueToNumber(ctx, lv, &mut exc);
        if !exc.is_null() || n.is_nan() || n < 0.0 {
            return 0;
        }
        n as u32
    }
}
