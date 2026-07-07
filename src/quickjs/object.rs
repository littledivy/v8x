//! QuickJS-backed `object` family shims: v8::Object Get/Set/Has/Delete/
//! CreateDataProperty/DefineOwnProperty/GetOwnPropertyNames/Prototype/Private,
//! plus v8::Array New/Length.
//!
//! Ported from reference/qjs_v8_compat/src/object.rs (which uses lossy
//! string keys) but upgraded to atom-based property access via
//! `JS_ValueToAtom` so symbol and integer keys round-trip correctly — this
//! matches the JSC backend's key-aware `*ForKey` calls in src/object.rs.
//!
//! Refcount discipline (see core): every fresh `JSValue` returned by a
//! QuickJS getter is +1 and goes through `intern`. Values handed to
//! `JS_SetProperty`/`JS_DefinePropertyValue` are *consumed* by QuickJS, so we
//! `JS_DupValue` first because the caller's handle slot still owns its copy.
//! Atoms from `JS_ValueToAtom`/`JS_NewAtom` are freed with `JS_FreeAtom`.
#![allow(non_snake_case, unused)]

use super::core::{
  ctx_of, current_ctx, current_iso, intern, intern_dup, iso_state, jsval_of,
};
use super::quickjs_sys::*;
use crate::support::{Maybe, MaybeBool, int};
use crate::{
  Array, Context, IndexFilter, KeyCollectionMode, KeyConversionMode, Name,
  Object, Private, PropertyAttribute, PropertyDescriptor, PropertyFilter,
  RealIsolate, String as V8String, Value,
};
use std::os::raw::{c_char, c_int};
use std::ptr;

unsafe extern "C" {
  fn JS_ValueToAtom(ctx: *mut JSContext, val: JSValue) -> JSAtom;
  fn JS_GetProperty(
    ctx: *mut JSContext,
    this_obj: JSValue,
    prop: JSAtom,
  ) -> JSValue;
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
  fn JS_SetPrototype(
    ctx: *mut JSContext,
    obj: JSValue,
    proto_val: JSValue,
  ) -> c_int;
  fn JS_GetLength(ctx: *mut JSContext, obj: JSValue, pres: *mut i64) -> c_int;
  fn JS_ToBool(ctx: *mut JSContext, val: JSValue) -> c_int;
  fn JS_GetOwnPropertyNames(
    ctx: *mut JSContext,
    ptab: *mut *mut JSPropertyEnum,
    plen: *mut u32,
    obj: JSValue,
    flags: c_int,
  ) -> c_int;
  fn JS_FreePropertyEnum(
    ctx: *mut JSContext,
    tab: *mut JSPropertyEnum,
    len: u32,
  );
}

#[repr(C)]
struct JSPropertyEnum {
  is_enumerable: bool,
  atom: JSAtom,
}

const JS_GPN_STRING_MASK: c_int = 1 << 0;
const JS_GPN_SYMBOL_MASK: c_int = 1 << 1;
const JS_GPN_ENUM_ONLY: c_int = 1 << 4;

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

#[inline]
pub(crate) fn key_atom<T>(ctx: *mut JSContext, key: *const T) -> JSAtom {
  if key.is_null() {
    return 0;
  }
  unsafe { JS_ValueToAtom(ctx, jsval_of(key)) }
}

#[inline]
fn attr_to_jsprop(attr: u32) -> c_int {
  let mut flags = JS_PROP_HAS_VALUE
    | JS_PROP_HAS_WRITABLE
    | JS_PROP_HAS_ENUMERABLE
    | JS_PROP_HAS_CONFIGURABLE;
  if attr & 1 == 0 {
    flags |= JS_PROP_WRITABLE;
  }
  if attr & 2 == 0 {
    flags |= JS_PROP_ENUMERABLE;
  }
  if attr & 4 == 0 {
    flags |= JS_PROP_CONFIGURABLE;
  }
  flags
}

const JS_PROP_HAS_CONFIGURABLE: c_int = 1 << 8;
const JS_PROP_HAS_WRITABLE: c_int = 1 << 9;
const JS_PROP_HAS_ENUMERABLE: c_int = 1 << 10;
const JS_PROP_HAS_VALUE: c_int = 1 << 13;

#[inline]
fn read_attr(attr: &PropertyAttribute) -> u32 {
  unsafe { *(attr as *const PropertyAttribute as *const u32) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__New(isolate: *mut RealIsolate) -> *const Object {
  let ctx = iso_ctx(isolate);
  if ctx.is_null() {
    return ptr::null();
  }

  let o = unsafe { JS_NewObject(ctx) };
  if o.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }
  let h = intern::<Object>(o);
  h
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

  let h = intern::<Value>(v);
  h
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
pub extern "C" fn v8__Object__GetPrototype(
  this: *const Object,
) -> *const Value {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }

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

  let v = unsafe { JS_DupValue(ctx, jsval_of(value)) };
  let r = unsafe { JS_SetProperty(ctx, jsval_of(this), atom, v) };
  unsafe { JS_FreeAtom(ctx, atom) };
  if r < 0 {
    if unsafe { swallow_write_rejection(ctx) } {
      return MaybeBool::JustFalse;
    }
    return MaybeBool::Nothing;
  }
  just_bool(r != 0)
}

unsafe fn swallow_write_rejection(ctx: *mut JSContext) -> bool {
  if !unsafe { JS_HasException(ctx) } {
    return true;
  }
  let exc = unsafe { JS_GetException(ctx) };
  let cs = unsafe { JS_ToCString(ctx, exc) };
  let is_write_reject = if cs.is_null() {
    false
  } else {
    let msg = unsafe { std::ffi::CStr::from_ptr(cs) }.to_string_lossy();
    let m = msg.as_ref();
    m.contains("read-only")
      || m.contains("not extensible")
      || m.contains("non-extensible")
  };
  if !cs.is_null() {
    unsafe { JS_FreeCString(ctx, cs) };
  }
  if is_write_reject {
    unsafe { JS_FreeValue(ctx, exc) };
    true
  } else {
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

  let v = unsafe { JS_DupValue(ctx, jsval_of(value)) };
  let r = unsafe {
    JS_DefinePropertyValue(ctx, jsval_of(this), atom, v, JS_PROP_C_W_E)
  };
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
  let r =
    unsafe { JS_DefinePropertyValue(ctx, jsval_of(this), atom, v, flags) };
  unsafe { JS_FreeAtom(ctx, atom) };
  if r < 0 {
    return MaybeBool::Nothing;
  }
  let raw_attr = read_attr(&attr);
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
  let r = super::property::pd_define(ctx, jsval_of(this), atom, desc);
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
  // flags=0, NOT JS_PROP_THROW: V8's `Object::Delete` reports an
  // unconfigurable property as `false` without throwing (sloppy-mode delete
  // semantics). JS_PROP_THROW would leave a pending exception the embedder
  // never expects (deno_core deletes init-only props and ignores the result;
  // the stale exception then aborts the next unrelated eval).
  let r = unsafe { JS_DeleteProperty(ctx, jsval_of(this), atom, 0) };
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
pub extern "C" fn v8__Object__GetConstructorName(
  this: *const Object,
) -> *const V8String {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }

  unsafe {
    let fallback = || intern::<V8String>(JS_NewString(ctx, c"Object".as_ptr()));

    let drain_exc = || {
      if JS_HasException(ctx) {
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

    intern::<V8String>(name)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetIdentityHash(this: *const Object) -> int {
  let v = jsval_of(this);
  let p = jsv_get_ptr(&v) as usize;
  let h = (p as u32) ^ ((p >> 32) as u32);
  (h & 0x7fff_ffff) as int
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Name__GetIdentityHash(this: *const Name) -> int {
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
  let _ = (mode, key_conversion);

  let skip_indices = index_filter as u32 == 1;
  property_names_array_filtered(
    this,
    context,
    gpn_from_filter(&property_filter),
    skip_indices,
  )
}

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
  unsafe { *(f as *const PropertyFilter as *const u32) }
}

fn property_names_array(
  this: *const Object,
  context: *const Context,
  gpn_flags: c_int,
) -> *const Array {
  property_names_array_filtered(this, context, gpn_flags, false)
}

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

  let arr = unsafe { JS_NewArray(ctx) };
  if arr.tag == JS_TAG_EXCEPTION {
    unsafe { JS_FreePropertyEnum(ctx, tab, len) };
    return ptr::null();
  }
  unsafe {
    let mut out = 0u32;
    for i in 0..len as usize {
      let atom = (*tab.add(i)).atom;

      if skip_indices && (atom & JS_ATOM_TAG_INT) != 0 {
        continue;
      }

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

  let v = unsafe { JS_DupValue(ctx, jsval_of(value)) };
  let flags = JS_PROP_HAS_VALUE
    | JS_PROP_HAS_WRITABLE
    | JS_PROP_HAS_CONFIGURABLE
    | JS_PROP_WRITABLE
    | JS_PROP_CONFIGURABLE;
  let r =
    unsafe { JS_DefinePropertyValue(ctx, jsval_of(this), atom, v, flags) };
  unsafe { JS_FreeAtom(ctx, atom) };
  if r < 0 {
    return MaybeBool::Nothing;
  }
  just_bool(r != 0)
}

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
  // flags=0, NOT JS_PROP_THROW: V8's `Object::Delete` reports an
  // unconfigurable property as `false` without throwing (sloppy-mode delete
  // semantics). JS_PROP_THROW would leave a pending exception the embedder
  // never expects (deno_core deletes init-only props and ignores the result;
  // the stale exception then aborts the next unrelated eval).
  let r = unsafe { JS_DeleteProperty(ctx, jsval_of(this), atom, 0) };
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
  // flags=0, NOT JS_PROP_THROW: V8's `Object::Delete` reports an
  // unconfigurable property as `false` without throwing (sloppy-mode delete
  // semantics). JS_PROP_THROW would leave a pending exception the embedder
  // never expects (deno_core deletes init-only props and ignores the result;
  // the stale exception then aborts the next unrelated eval).
  let r = unsafe { JS_DeleteProperty(ctx, jsval_of(this), atom, 0) };
  unsafe { JS_FreeAtom(ctx, atom) };
  if r < 0 {
    return MaybeBool::Nothing;
  }
  just_bool(r != 0)
}

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
    unsafe { maybe_attr_set(out, 0) };
  } else {
    unsafe { maybe_attr_none(out) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetCreationContext(
  this: *const Object,
) -> *const Context {
  let _ = this;
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }
  super::core::intern_ctx(ctx)
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

  let method: &[u8] = match level {
    crate::IntegrityLevel::Frozen => b"freeze\0",
    crate::IntegrityLevel::Sealed => b"seal\0",
  };
  unsafe {
    let global = JS_GetGlobalObject(ctx);
    let object_ctor = JS_GetPropertyStr(ctx, global, c"Object".as_ptr());
    JS_FreeValue(ctx, global);
    let func =
      JS_GetPropertyStr(ctx, object_ctor, method.as_ptr() as *const c_char);
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
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    if !is_key_value.is_null() {
      unsafe { *is_key_value = false };
    }
    return ptr::null();
  }
  // A Map/Set ITERATOR has no `.entries()`; previewing it must peek the
  // remaining backing entries WITHOUT consuming it (Array.from would). Native
  // peek (quickjs.c js_v82jsc_iterator_preview); collections fall through.
  {
    let mut kv: std::os::raw::c_int = 0;
    let arr =
      unsafe { js_v82jsc_iterator_preview(ctx, jsval_of(this), &mut kv) };
    if arr.tag != JS_TAG_NULL && arr.tag != JS_TAG_EXCEPTION {
      if !is_key_value.is_null() {
        unsafe { *is_key_value = kv != 0 };
      }
      return intern::<Array>(arr);
    }
    if arr.tag == JS_TAG_EXCEPTION {
      unsafe { JS_FreeValue(ctx, JS_GetException(ctx)) };
    }
  }
  unsafe {
    let is_map = {
      let m = JS_GetPropertyStr(ctx, jsval_of(this), c"set".as_ptr());
      let is_fn = JS_IsFunction(ctx, m);
      JS_FreeValue(ctx, m);
      is_fn
    };
    if !is_key_value.is_null() {
      *is_key_value = is_map;
    }

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

    unsafe {
      *(&mut tmp as *mut Maybe<PropertyAttribute> as *mut bool) = true;
    }
    tmp
  } else {
    unsafe { std::mem::zeroed() }
  };
  unsafe { ptr::write(out, m) };
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Array__New(
  isolate: *mut RealIsolate,
  length: int,
) -> *const Array {
  let ctx = iso_ctx(isolate);
  if ctx.is_null() {
    return ptr::null();
  }
  let arr = unsafe { JS_NewArray(ctx) };
  if arr.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }
  if length > 0 {
    unsafe { JS_SetLength_local(ctx, arr, length as i64) };
  }
  intern::<Array>(arr)
}

#[inline]
unsafe fn JS_SetLength_local(ctx: *mut JSContext, arr: JSValue, len: i64) {
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

thread_local! {
    static WRAP_TABLE: std::cell::RefCell<
        std::collections::HashMap<(usize, u16), usize>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());

    static INTERNAL_FIELDS: std::cell::RefCell<
        std::collections::HashMap<usize, Vec<JSValue>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());

    static ALIGNED_FIELDS: std::cell::RefCell<
        std::collections::HashMap<(usize, crate::support::int, u16), usize>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

#[inline]
fn wrap_key(wrapper: *const Object) -> usize {
  unsafe { jsval_of(wrapper).u.ptr as usize }
}

#[inline]
fn value_key(v: JSValue) -> usize {
  unsafe { v.u.ptr as usize }
}

pub(crate) fn set_internal_field_count_for_value(
  obj: JSValue,
  count: crate::support::int,
) {
  if count <= 0 || !jsv_is_object(&obj) {
    return;
  }
  let mut fields = Vec::with_capacity(count as usize);
  for _ in 0..count {
    fields.push(jsv_undefined());
  }
  INTERNAL_FIELDS.with(|t| {
    t.borrow_mut().insert(value_key(obj), fields);
  });
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

// ---------------------------------------------------------------------------
// Link-stubs for v8 C-ABI symbols that `test_api.rs` references but this
// backend doesn't implement yet. Each returns a benign default
// (null / 0 / false / `Nothing`) so the target LINKS and the many tests that
// don't touch these paths run; tests that do exercise them fail gracefully
// without crashing. Promote individual stubs to real implementations over time.
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetAlignedPointerFromInternalField(
  this: *const std::os::raw::c_void,
  index: crate::support::int,
  tag: u16,
) -> *const std::os::raw::c_void {
  if this.is_null() || index < 0 {
    return std::ptr::null();
  }
  let key = value_key(jsval_of(this));
  ALIGNED_FIELDS.with(|t| {
    t.borrow().get(&(key, index, tag)).copied().unwrap_or(0)
      as *const std::os::raw::c_void
  })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetInternalField(
  this: *const std::os::raw::c_void,
  index: crate::support::int,
) -> *const std::os::raw::c_void {
  if this.is_null() || index < 0 {
    return std::ptr::null();
  }
  let ctx = current_ctx();
  if ctx.is_null() {
    return std::ptr::null();
  }
  let key = value_key(jsval_of(this));
  INTERNAL_FIELDS.with(|t| {
    t.borrow()
      .get(&key)
      .and_then(|fields| fields.get(index as usize).copied())
      .map(|v| intern_dup::<crate::Data>(ctx, v) as *const std::os::raw::c_void)
      .unwrap_or(std::ptr::null())
  })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__InternalFieldCount(
  this: *const std::os::raw::c_void,
) -> crate::support::int {
  if this.is_null() {
    return 0;
  }
  let key = value_key(jsval_of(this));
  INTERNAL_FIELDS.with(|t| {
    t.borrow()
      .get(&key)
      .map(|fields| fields.len() as crate::support::int)
      .unwrap_or(0)
  })
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
  this: *const std::os::raw::c_void,
  index: crate::support::int,
  value: *const std::os::raw::c_void,
  tag: u16,
) {
  if this.is_null() || index < 0 {
    return;
  }
  let key = value_key(jsval_of(this));
  let in_range = INTERNAL_FIELDS.with(|t| {
    t.borrow()
      .get(&key)
      .map(|fields| (index as usize) < fields.len())
      .unwrap_or(false)
  });
  if !in_range {
    return;
  }
  ALIGNED_FIELDS.with(|t| {
    t.borrow_mut().insert((key, index, tag), value as usize);
  });
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetInternalField(
  this: *const std::os::raw::c_void,
  index: crate::support::int,
  data: *const std::os::raw::c_void,
) {
  if this.is_null() || index < 0 {
    return;
  }
  let ctx = current_ctx();
  if ctx.is_null() {
    return;
  }
  let key = value_key(jsval_of(this));
  let value = unsafe { JS_DupValue(ctx, jsval_of(data)) };
  INTERNAL_FIELDS.with(|t| {
    let mut t = t.borrow_mut();
    let Some(fields) = t.get_mut(&key) else {
      unsafe { JS_FreeValue(ctx, value) };
      return;
    };
    let Some(slot) = fields.get_mut(index as usize) else {
      unsafe { JS_FreeValue(ctx, value) };
      return;
    };
    let old = std::mem::replace(slot, value);
    unsafe { JS_FreeValue(ctx, old) };
  });
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
