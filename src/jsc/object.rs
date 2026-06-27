#![allow(non_snake_case, unused)]

use crate::jsc::core::{
  ctx_of, current_ctx, current_iso, intern, intern_ctx, iso_state, jsval,
};
use crate::jsc::jsc_sys::*;
use crate::support::{Maybe, MaybeBool, int};
use crate::{
  Array, Context, IndexFilter, IntegrityLevel, KeyCollectionMode,
  KeyConversionMode, Map, Name, Object, Private, PropertyAttribute,
  PropertyDescriptor, PropertyFilter, RealIsolate, Set, String, Value,
};
use std::os::raw::{c_char, c_void};
use std::ptr;

#[inline]
fn obj_of(ctx: JSContextRef, p: *const Object) -> JSObjectRef {
  let v = jsval(p);
  if v.is_null() {
    return ptr::null_mut();
  }
  let mut exc: JSValueRef = ptr::null();
  unsafe { JSValueToObject(ctx, v, &mut exc) }
}

#[inline]
fn just_bool(b: bool) -> MaybeBool {
  if b {
    MaybeBool::JustTrue
  } else {
    MaybeBool::JustFalse
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__New(isolate: *mut RealIsolate) -> *const Object {
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
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
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
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
pub extern "C" fn v8__Object__GetPrototype(
  this: *const Object,
) -> *const Value {
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
  unsafe {
    JSObjectSetPropertyForKey(ctx, o, jsval(key), jsval(value), 0, &mut exc)
  };
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
  unsafe {
    JSObjectSetPropertyForKey(ctx, o, jsval(key), jsval(value), 0, &mut exc)
  };
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

  let a = attr.as_u32();
  let mut jsc_attr: u32 = 0;
  if a & (1 << 0) != 0 {
    jsc_attr |= 1 << 1;
  }
  if a & (1 << 1) != 0 {
    jsc_attr |= 1 << 2;
  }
  if a & (1 << 2) != 0 {
    jsc_attr |= 1 << 3;
  }
  let mut exc: JSValueRef = ptr::null();
  unsafe {
    JSObjectSetPropertyForKey(
      ctx,
      o,
      jsval(key),
      jsval(value),
      jsc_attr,
      &mut exc,
    )
  };
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
pub extern "C" fn v8__Object__GetConstructorName(
  this: *const Object,
) -> *const String {
  let ctx = current_ctx();
  let o = obj_of(ctx, this);
  if o.is_null() {
    return ptr::null();
  }

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
  let _ = (filter, key_conversion);
  property_names_array(this, context, false)
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
  let _ = (mode, key_conversion);

  let filter =
    unsafe { *(&property_filter as *const PropertyFilter as *const u32) };

  if filter & 8 != 0 {
    return empty_array(this, context);
  }

  let skip_indices = index_filter as u32 == 1;
  property_names_array(this, context, skip_indices)
}

fn property_names_array(
  this: *const Object,
  context: *const Context,
  skip_indices: bool,
) -> *const Array {
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
      if skip_indices && jsstr_is_array_index(s) {
        continue;
      }
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

fn empty_array(_this: *const Object, context: *const Context) -> *const Array {
  let ctx = ctx_of(context) as JSContextRef;
  if ctx.is_null() {
    return ptr::null();
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let arr = JSObjectMakeArray(ctx, 0, ptr::null(), &mut exc);
    if !exc.is_null() || arr.is_null() {
      return ptr::null();
    }
    intern_ctx::<Array>(ctx, arr as JSValueRef)
  }
}

unsafe fn jsstr_is_array_index(s: JSStringRef) -> bool {
  let len = unsafe { JSStringGetLength(s) };
  if len == 0 || len > 10 {
    return false;
  }
  let chars = unsafe { JSStringGetCharactersPtr(s) };
  if chars.is_null() {
    return false;
  }
  let slice = unsafe { std::slice::from_raw_parts(chars, len) };

  if slice[0] == b'0' as u16 {
    return len == 1;
  }
  let mut val: u64 = 0;
  for &c in slice {
    if !(b'0' as u16..=b'9' as u16).contains(&c) {
      return false;
    }
    val = val * 10 + (c - b'0' as u16) as u64;
  }
  val <= u32::MAX as u64
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetPrivate(
  this: *const Object,
  context: *const Context,
  key: *const Private,
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

  let mut exc: JSValueRef = ptr::null();
  unsafe {
    JSObjectSetPropertyForKey(
      ctx,
      o,
      jsval(key),
      jsval(value),
      1 << 2,
      &mut exc,
    )
  };
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

    unsafe {
      *(&mut tmp as *mut Maybe<PropertyAttribute> as *mut bool) = true;
    }
    tmp
  } else {
    unsafe { std::mem::zeroed() }
  };
  unsafe {
    ptr::write(out, m);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Array__New(
  isolate: *mut RealIsolate,
  length: int,
) -> *const Array {
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
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
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
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
    let ls =
      JSStringCreateWithUTF8CString(b"length\0".as_ptr() as *const c_char);
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

thread_local! {
    static WRAP_TABLE: std::cell::RefCell<
        std::collections::HashMap<(usize, u16), usize>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
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
  WRAP_TABLE.with(|t| {
    t.borrow_mut()
      .insert((wrapper as usize, tag), value as usize);
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
  WRAP_TABLE.with(|t| {
    t.borrow()
      .get(&(wrapper as usize, tag))
      .map(|&p| p as *mut crate::binding::RustObj)
      .unwrap_or(ptr::null_mut())
  })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__IsApiWrapper(this: *const Object) -> bool {
  if this.is_null() {
    return false;
  }
  WRAP_TABLE.with(|t| {
    let map = t.borrow();
    map.keys().any(|(w, _)| *w == this as usize)
  })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetOwnPropertyDescriptor(
  this: *const Object,
  context: *const Context,
  key: *const Name,
) -> *const Value {
  if this.is_null() {
    return ptr::null();
  }
  let ctx = ctx_of(context) as JSContextRef;
  if ctx.is_null() {
    return ptr::null();
  }
  let obj = obj_of(ctx, this);
  if obj.is_null() {
    return ptr::null();
  }
  unsafe {
    let global = JSContextGetGlobalObject(ctx);
    let object_key = JSStringCreateWithUTF8CString(c"Object".as_ptr());
    let mut exc: JSValueRef = ptr::null();
    let object_ctor = JSObjectGetProperty(ctx, global, object_key, &mut exc);
    JSStringRelease(object_key);
    if object_ctor.is_null() || !JSValueIsObject(ctx, object_ctor) {
      return ptr::null();
    }
    let gopd_key =
      JSStringCreateWithUTF8CString(c"getOwnPropertyDescriptor".as_ptr());
    let gopd =
      JSObjectGetProperty(ctx, object_ctor as JSObjectRef, gopd_key, &mut exc);
    JSStringRelease(gopd_key);
    if gopd.is_null() || !JSValueIsObject(ctx, gopd) {
      return ptr::null();
    }
    let args = [obj as JSValueRef, jsval(key)];
    let r = JSObjectCallAsFunction(
      ctx,
      gopd as JSObjectRef,
      object_ctor as JSObjectRef,
      2,
      args.as_ptr(),
      &mut exc,
    );
    if r.is_null() {
      if !exc.is_null() {
        crate::jsc::core::record_pending_exception(ctx, exc);
      }
      return ptr::null();
    }

    intern_ctx::<Value>(ctx, r)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__HasIndex(
  this: *const Object,
  context: *const Context,
  index: u32,
) -> MaybeBool {
  let ctx = ctx_of(context) as JSContextRef;
  let o = obj_of(ctx, this);
  if o.is_null() {
    return MaybeBool::Nothing;
  }
  let mut exc: JSValueRef = ptr::null();
  let v = unsafe { JSObjectGetPropertyAtIndex(ctx, o, index, &mut exc) };
  if !exc.is_null() {
    return MaybeBool::Nothing;
  }

  just_bool(!unsafe { JSValueIsUndefined(ctx, v) })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__DeleteIndex(
  this: *const Object,
  context: *const Context,
  index: u32,
) -> MaybeBool {
  let ctx = ctx_of(context) as JSContextRef;
  let o = obj_of(ctx, this);
  if o.is_null() {
    return MaybeBool::Nothing;
  }

  let key = unsafe { JSValueMakeNumber(ctx, index as f64) };
  let mut exc: JSValueRef = ptr::null();
  let r = unsafe { JSObjectDeletePropertyForKey(ctx, o, key, &mut exc) };
  if !exc.is_null() {
    return MaybeBool::Nothing;
  }
  just_bool(r)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetRealNamedProperty(
  this: *const Object,
  context: *const Context,
  key: *const Name,
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
pub extern "C" fn v8__Object__HasRealNamedProperty(
  this: *const Object,
  context: *const Context,
  key: *const Name,
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
pub extern "C" fn v8__Object__GetRealNamedPropertyAttributes(
  this: *const Object,
  context: *const Context,
  key: *const Name,
  out: *mut Maybe<PropertyAttribute>,
) {
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
    unsafe {
      *(&mut tmp as *mut Maybe<PropertyAttribute> as *mut bool) = true;
    }
    tmp
  } else {
    unsafe { std::mem::zeroed() }
  };
  unsafe {
    ptr::write(out, m);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__HasPrivate(
  this: *const Object,
  context: *const Context,
  key: *const Private,
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
pub extern "C" fn v8__Object__DeletePrivate(
  this: *const Object,
  context: *const Context,
  key: *const Private,
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
pub extern "C" fn v8__Object__GetCreationContext(
  this: *const Object,
) -> *const Context {
  let iso = current_iso();
  if iso.is_null() {
    return ptr::null();
  }
  iso_state(iso)
    .contexts
    .last()
    .copied()
    .unwrap_or(ptr::null_mut()) as *const Context
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetIntegrityLevel(
  this: *const Object,
  context: *const Context,
  level: IntegrityLevel,
) -> MaybeBool {
  let ctx = ctx_of(context) as JSContextRef;
  let o = obj_of(ctx, this);
  if o.is_null() {
    return MaybeBool::Nothing;
  }

  let fname = match level {
    IntegrityLevel::Frozen => b"freeze\0".as_ref(),
    IntegrityLevel::Sealed => b"seal\0".as_ref(),
  };
  unsafe {
    let global = JSContextGetGlobalObject(ctx);
    let mut exc: JSValueRef = ptr::null();
    let oname =
      JSStringCreateWithUTF8CString(b"Object\0".as_ptr() as *const c_char);
    let object_ctor = JSObjectGetProperty(ctx, global, oname, &mut exc);
    JSStringRelease(oname);
    if !exc.is_null() || !JSValueIsObject(ctx, object_ctor) {
      return MaybeBool::Nothing;
    }
    let object_obj = JSValueToObject(ctx, object_ctor, &mut exc);
    let fs = JSStringCreateWithUTF8CString(fname.as_ptr() as *const c_char);
    let func = JSObjectGetProperty(ctx, object_obj, fs, &mut exc);
    JSStringRelease(fs);
    if !exc.is_null() {
      return MaybeBool::Nothing;
    }
    let func_obj = JSValueToObject(ctx, func, &mut exc);
    if func_obj.is_null() {
      return MaybeBool::Nothing;
    }
    let args = [o as JSValueRef];
    JSObjectCallAsFunction(
      ctx,
      func_obj,
      ptr::null_mut(),
      1,
      args.as_ptr(),
      &mut exc,
    );
    if !exc.is_null() {
      return MaybeBool::Nothing;
    }
  }
  MaybeBool::JustTrue
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__PreviewEntries(
  this: *const Object,
  is_key_value: *mut bool,
) -> *const Array {
  let ctx = current_ctx();
  let o = obj_of(ctx, this);
  if ctx.is_null() || o.is_null() {
    return ptr::null();
  }
  // A Map/Set ITERATOR has no `.entries()`/`.values()` of its own; previewing
  // it means peeking the remaining backing entries without consuming. Do that
  // natively (introspect.cpp). Collections fall through to the iterator-method
  // path below.
  {
    let mut kv = false;
    let arr = unsafe {
      crate::jsc::introspect::v82jsc_iterator_preview(
        ctx,
        o as JSValueRef,
        &mut kv,
      )
    };
    if !arr.is_null() {
      if !is_key_value.is_null() {
        unsafe { *is_key_value = kv };
      }
      return intern::<Array>(arr);
    }
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();

    let try_iter = |name: &[u8]| -> JSValueRef {
      let mut e: JSValueRef = ptr::null();
      let ns = JSStringCreateWithUTF8CString(name.as_ptr() as *const c_char);
      let m = JSObjectGetProperty(ctx, o, ns, &mut e);
      JSStringRelease(ns);
      if !e.is_null() || !JSValueIsObject(ctx, m) {
        return ptr::null();
      }
      let mo = JSValueToObject(ctx, m, &mut e);
      if mo.is_null() || !JSObjectIsFunction(ctx, mo) {
        return ptr::null();
      }
      JSObjectCallAsFunction(ctx, mo, o, 0, ptr::null(), &mut e)
    };
    let (iter, kv) = {
      let it = try_iter(b"entries\0");
      if !it.is_null() {
        (it, true)
      } else {
        (try_iter(b"values\0"), false)
      }
    };
    if iter.is_null() {
      if !is_key_value.is_null() {
        *is_key_value = false;
      }
      return ptr::null();
    }
    if !is_key_value.is_null() {
      *is_key_value = kv;
    }

    let global = JSContextGetGlobalObject(ctx);
    let an =
      JSStringCreateWithUTF8CString(b"Array\0".as_ptr() as *const c_char);
    let array_ctor = JSObjectGetProperty(ctx, global, an, &mut exc);
    JSStringRelease(an);
    let array_obj = JSValueToObject(ctx, array_ctor, &mut exc);
    let fr = JSStringCreateWithUTF8CString(b"from\0".as_ptr() as *const c_char);
    let from = JSObjectGetProperty(ctx, array_obj, fr, &mut exc);
    JSStringRelease(fr);
    let from_obj = JSValueToObject(ctx, from, &mut exc);
    if from_obj.is_null() {
      return ptr::null();
    }
    let args = [iter];
    let arr = JSObjectCallAsFunction(
      ctx,
      from_obj,
      ptr::null_mut(),
      1,
      args.as_ptr(),
      &mut exc,
    );
    if !exc.is_null() || arr.is_null() {
      return ptr::null();
    }

    if kv {
      let flat = b"(function(pairs){var r=[];for(var i=0;i<pairs.length;i++){r.push(pairs[i][0]);r.push(pairs[i][1]);}return r;})\0";
      let fs = JSStringCreateWithUTF8CString(flat.as_ptr() as *const c_char);
      let fnv = JSEvaluateScript(
        ctx,
        fs,
        ptr::null_mut(),
        ptr::null_mut(),
        1,
        &mut exc,
      );
      JSStringRelease(fs);
      let fnobj = JSValueToObject(ctx, fnv, &mut exc);
      if !fnobj.is_null() {
        let a2 = [arr];
        let flat_arr = JSObjectCallAsFunction(
          ctx,
          fnobj,
          ptr::null_mut(),
          1,
          a2.as_ptr(),
          &mut exc,
        );
        if exc.is_null() && !flat_arr.is_null() {
          return intern_ctx::<Array>(ctx, flat_arr);
        }
      }
    }
    intern_ctx::<Array>(ctx, arr)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Size(map: *const Map) -> usize {
  map_set_size(jsval(map))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__Size(map: *const Set) -> usize {
  map_set_size(jsval(map))
}

fn map_set_size(v: JSValueRef) -> usize {
  let ctx = current_ctx();
  if ctx.is_null() || v.is_null() {
    return 0;
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let o = JSValueToObject(ctx, v, &mut exc);
    if o.is_null() {
      return 0;
    }
    let ss = JSStringCreateWithUTF8CString(b"size\0".as_ptr() as *const c_char);
    let sz = JSObjectGetProperty(ctx, o, ss, &mut exc);
    JSStringRelease(ss);
    if !exc.is_null() {
      return 0;
    }
    JSValueToNumber(ctx, sz, &mut exc) as usize
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__As__Array(this: *const Map) -> *const Array {
  map_set_as_array(jsval(this), true)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__As__Array(this: *const Set) -> *const Array {
  map_set_as_array(jsval(this), false)
}

fn map_set_as_array(v: JSValueRef, is_map: bool) -> *const Array {
  let ctx = current_ctx();
  if ctx.is_null() || v.is_null() {
    return ptr::null();
  }
  unsafe {
    let src = if is_map {
      b"(function(m){var r=[];m.forEach(function(val,key){r.push(key);r.push(val);});return r;})\0".as_ref()
    } else {
      b"(function(s){var r=[];s.forEach(function(val){r.push(val);});return r;})\0".as_ref()
    };
    let mut exc: JSValueRef = ptr::null();
    let fs = JSStringCreateWithUTF8CString(src.as_ptr() as *const c_char);
    let fnv =
      JSEvaluateScript(ctx, fs, ptr::null_mut(), ptr::null_mut(), 1, &mut exc);
    JSStringRelease(fs);
    let fnobj = JSValueToObject(ctx, fnv, &mut exc);
    if fnobj.is_null() {
      return ptr::null();
    }
    let args = [v];
    let arr = JSObjectCallAsFunction(
      ctx,
      fnobj,
      ptr::null_mut(),
      1,
      args.as_ptr(),
      &mut exc,
    );
    if !exc.is_null() || arr.is_null() {
      return ptr::null();
    }
    intern_ctx::<Array>(ctx, arr)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__New(isolate: *mut RealIsolate) -> *const Set {
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  if ctx.is_null() {
    return ptr::null();
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let fs =
      JSStringCreateWithUTF8CString(b"(new Set())\0".as_ptr() as *const c_char);
    let v =
      JSEvaluateScript(ctx, fs, ptr::null_mut(), ptr::null_mut(), 1, &mut exc);
    JSStringRelease(fs);
    if !exc.is_null() {
      return ptr::null();
    }
    intern_ctx::<Set>(ctx, v)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__Add(
  this: *const Set,
  context: *const Context,
  key: *const Value,
) -> *const Set {
  let ctx = ctx_of(context) as JSContextRef;
  let o = obj_of(ctx, this as *const Object);
  if o.is_null() {
    return ptr::null();
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let as_ = JSStringCreateWithUTF8CString(b"add\0".as_ptr() as *const c_char);
    let add = JSObjectGetProperty(ctx, o, as_, &mut exc);
    JSStringRelease(as_);
    let add_obj = JSValueToObject(ctx, add, &mut exc);
    if add_obj.is_null() {
      return ptr::null();
    }
    let args = [jsval(key)];
    JSObjectCallAsFunction(ctx, add_obj, o, 1, args.as_ptr(), &mut exc);
    if !exc.is_null() {
      return ptr::null();
    }
  }

  this
}

// ---------------------------------------------------------------------------
// Link-stubs for v8 C-ABI symbols that `test_api.rs` references but this
// backend doesn't implement yet. Each returns a benign default
// (null / 0 / false / `Nothing`) so the target LINKS and the many tests that
// don't touch these paths run; tests that do exercise them fail gracefully
// without crashing. Promote individual stubs to real implementations over time.
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Clear(_this: *const std::os::raw::c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Delete(
  _this: *const std::os::raw::c_void,
  _context: *const std::os::raw::c_void,
  _key: *const std::os::raw::c_void,
) -> crate::support::MaybeBool {
  crate::support::MaybeBool::Nothing
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Get(
  _this: *const std::os::raw::c_void,
  _context: *const std::os::raw::c_void,
  _key: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Has(
  _this: *const std::os::raw::c_void,
  _context: *const std::os::raw::c_void,
  _key: *const std::os::raw::c_void,
) -> crate::support::MaybeBool {
  crate::support::MaybeBool::Nothing
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__New(
  _isolate: *mut std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Set(
  _this: *const std::os::raw::c_void,
  _context: *const std::os::raw::c_void,
  _key: *const std::os::raw::c_void,
  _value: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetAlignedPointerFromInternalField(
  _this: *const std::os::raw::c_void,
  _index: crate::support::int,
  _tag: u16,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetInternalField(
  _this: *const std::os::raw::c_void,
  _index: crate::support::int,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__InternalFieldCount(
  _this: *const std::os::raw::c_void,
) -> crate::support::int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetAccessor(
  _this: *const std::os::raw::c_void,
  _context: *const std::os::raw::c_void,
  _key: *const std::os::raw::c_void,
  _getter: *const std::os::raw::c_void,
  _setter: *const std::os::raw::c_void,
  _data_or_null: *const std::os::raw::c_void,
  _attr: crate::PropertyAttribute,
) -> crate::support::MaybeBool {
  crate::support::MaybeBool::Nothing
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetAlignedPointerInInternalField(
  _this: *const std::os::raw::c_void,
  _index: crate::support::int,
  _value: *const std::os::raw::c_void,
  _tag: u16,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetInternalField(
  _this: *const std::os::raw::c_void,
  _index: crate::support::int,
  _data: *const std::os::raw::c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetLazyDataProperty(
  _this: *const std::os::raw::c_void,
  _context: *const std::os::raw::c_void,
  _key: *const std::os::raw::c_void,
  _getter: *const std::os::raw::c_void,
  _data_or_null: *const std::os::raw::c_void,
  _attr: crate::PropertyAttribute,
  _getter_side_effect_type: crate::SideEffectType,
  _setter_side_effect_type: crate::SideEffectType,
) -> crate::support::MaybeBool {
  crate::support::MaybeBool::Nothing
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__RegExp__Exec(
  _this: *const std::os::raw::c_void,
  _context: *const std::os::raw::c_void,
  _subject: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__RegExp__GetSource(
  _this: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__RegExp__New(
  _context: *const std::os::raw::c_void,
  _pattern: *const std::os::raw::c_void,
  _flags: crate::RegExpCreationFlags,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__Clear(_this: *const std::os::raw::c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__Delete(
  _this: *const std::os::raw::c_void,
  _context: *const std::os::raw::c_void,
  _key: *const std::os::raw::c_void,
) -> crate::support::MaybeBool {
  crate::support::MaybeBool::Nothing
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__Has(
  _this: *const std::os::raw::c_void,
  _context: *const std::os::raw::c_void,
  _key: *const std::os::raw::c_void,
) -> crate::support::MaybeBool {
  crate::support::MaybeBool::Nothing
}
