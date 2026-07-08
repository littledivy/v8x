#![allow(non_snake_case, unused)]

use crate::quickjs::core::{
  ctx_of, current_ctx, current_iso, intern, jsval_of,
};
use crate::quickjs::quickjs_sys::*;

use crate::support::MaybeBool;
use crate::value_deserializer::{
  CxxValueDeserializer, CxxValueDeserializerDelegate,
};
use crate::value_serializer::{CxxValueSerializer, CxxValueSerializerDelegate};
use crate::{
  ArrayBuffer, Context, Local, Object, RealIsolate, SharedArrayBuffer,
  String as V8String, Value,
};

use std::ffi::c_void;
use std::mem::MaybeUninit;
use std::os::raw::{c_char, c_int};

const JS_WRITE_OBJ_REFERENCE: c_int = 1 << 3;
const JS_WRITE_OBJ_SAB: c_int = 1 << 2;
const JS_READ_OBJ_REFERENCE: c_int = 1 << 3;
const JS_READ_OBJ_SAB: c_int = 1 << 2;

const MAGIC: &[u8; 4] = b"QJSV";

const TAG_VALUE: u8 = b'V';
const TAG_U32: u8 = 4;
const TAG_U64: u8 = 8;
const TAG_DOUBLE: u8 = b'D';
const TAG_RAW: u8 = b'R';

// Graph-mode tags: used only when the value carries transferred ArrayBuffers
// (and, later, host objects), which `JS_WriteObject` can't express. The default
// path stays a single `TAG_VALUE` + opaque `JS_WriteObject` blob, byte-identical
// to before — graph mode is gated so every currently-working case is untouched.
const TAG_GRAPH: u8 = b'G';
const TAG_XFER_AB: u8 = b'T';
const TAG_ARRAY: u8 = b'A';
const TAG_LEAF: u8 = b'L';
const TAG_HOST: u8 = b'H';
const TAG_OBJECT: u8 = b'O';
const TAG_SAB: u8 = b'S';

const SAB_CLONE_ERROR: &[u8] = b"#<SharedArrayBuffer> could not be cloned.";

#[repr(C)]
struct JSPropertyEnum {
  is_enumerable: bool,
  atom: JSAtom,
}
const JS_GPN_OWN_ENUM: c_int = (1 << 0) | (1 << 1) | (1 << 4); // string|symbol|enum-only

unsafe extern "C" {
  fn JS_IsInstanceOf(ctx: *mut JSContext, val: JSValue, obj: JSValue) -> c_int;
  fn JS_ValueToAtom(ctx: *mut JSContext, val: JSValue) -> JSAtom;
  fn JS_GetOwnPropertyNames(
    ctx: *mut JSContext,
    ptab: *mut *mut JSPropertyEnum,
    plen: *mut u32,
    obj: JSValue,
    flags: c_int,
  ) -> c_int;
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
  // deno's serializer-delegate trampolines (rusty_v8, same crate, #[no_mangle]).
  // In real V8 these are called BY the C++ serializer; here we drive them.
  fn v8__ValueSerializer__Delegate__ThrowDataCloneError(
    delegate: *mut CxxValueSerializerDelegate,
    message: *const V8String,
  );
  fn v8__ValueSerializer__Delegate__GetSharedArrayBufferId(
    delegate: *mut CxxValueSerializerDelegate,
    isolate: *mut RealIsolate,
    shared_array_buffer: *const SharedArrayBuffer,
    clone_id: *mut u32,
  ) -> bool;
  fn v8__ValueSerializer__Delegate__WriteHostObject(
    delegate: *mut CxxValueSerializerDelegate,
    isolate: *mut RealIsolate,
    object: *const Object,
  ) -> MaybeBool;
  fn v8__ValueDeserializer__Delegate__GetSharedArrayBufferFromId(
    delegate: *mut CxxValueDeserializerDelegate,
    isolate: *mut RealIsolate,
    transfer_id: u32,
  ) -> *const SharedArrayBuffer;
  fn v8__ValueDeserializer__Delegate__ReadHostObject(
    delegate: *mut CxxValueDeserializerDelegate,
    isolate: *mut RealIsolate,
  ) -> *const Object;
}

/// Atom for the `Symbol.for("Deno.core.hostObject")` brand deno tags transferable
/// host objects (MessagePort, CryptoKey) with. Computed per serialize (cheap, and
/// avoids cross-context atom-lifetime hazards). Returns 0 (JS_ATOM_NULL) on error.
fn host_brand_atom(ctx: *mut JSContext) -> JSAtom {
  if ctx.is_null() {
    return 0;
  }
  let src = c"Symbol.for(\"Deno.core.hostObject\")";
  let sym = unsafe {
    JS_Eval(
      ctx,
      src.as_ptr(),
      src.count_bytes(),
      c"<v82jsc-ser>".as_ptr(),
      JS_EVAL_TYPE_GLOBAL,
    )
  };
  if sym.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return 0;
  }
  let atom = unsafe { JS_ValueToAtom(ctx, sym) };
  unsafe { JS_FreeValue(ctx, sym) };
  atom
}

#[inline]
fn is_host_object(ctx: *mut JSContext, v: JSValue, brand: JSAtom) -> bool {
  brand != 0
    && jsv_is_object(&v)
    && unsafe { JS_HasProperty(ctx, v, brand) } > 0
}

fn is_shared_array_buffer(ctx: *mut JSContext, v: JSValue) -> bool {
  if ctx.is_null() || !jsv_is_object(&v) {
    return false;
  }
  unsafe {
    let global = JS_GetGlobalObject(ctx);
    let ctor = JS_GetPropertyStr(
      ctx,
      global,
      c"SharedArrayBuffer".as_ptr() as *const c_char,
    );
    let result = if jsv_is_object(&ctor) {
      let r = JS_IsInstanceOf(ctx, v, ctor);
      if r < 0 {
        let exc = JS_GetException(ctx);
        JS_FreeValue(ctx, exc);
        false
      } else {
        r > 0
      }
    } else {
      false
    };
    JS_FreeValue(ctx, ctor);
    JS_FreeValue(ctx, global);
    result
  }
}

/// Does the value graph contain a SharedArrayBuffer, transferred ArrayBuffer, or
/// host object?
/// Read-only (pure QuickJS, no delegate/scope machinery), so it is safe to run on
/// every serialize; only walks arrays (host objects nested in plain objects need
/// the not-yet-added object-enumeration path and stay on the default path).
fn graph_needs_walk(
  st: &SerState,
  ctx: *mut JSContext,
  v: JSValue,
  brand: JSAtom,
  depth: u32,
) -> bool {
  if !jsv_is_object(&v) || depth > 200 {
    return false;
  }
  if is_shared_array_buffer(ctx, v) {
    return true;
  }
  let ptr = unsafe { v.u.ptr } as usize;
  if st.xfer_ab.contains_key(&ptr) || is_host_object(ctx, v, brand) {
    return true;
  }
  if unsafe { JS_IsArray(v) } {
    let lenval = unsafe { JS_GetPropertyStr(ctx, v, c"length".as_ptr()) };
    let mut len: i32 = 0;
    unsafe {
      JS_ToInt32(ctx, &mut len, lenval);
      JS_FreeValue(ctx, lenval);
    }
    for i in 0..len.max(0) as u32 {
      let el = unsafe { JS_GetPropertyUint32(ctx, v, i) };
      let hit = graph_needs_walk(st, ctx, el, brand, depth + 1);
      unsafe { JS_FreeValue(ctx, el) };
      if hit {
        return true;
      }
    }
    return false;
  }
  // Plain object: recurse own enumerable property values.
  let mut found = false;
  for_each_own(ctx, v, |_keyatom, propval| {
    if !found && graph_needs_walk(st, ctx, propval, brand, depth + 1) {
      found = true;
    }
  });
  found
}

/// Iterate own enumerable (string + symbol) properties of `v`, calling `f(atom,
/// value)`. The value is owned during the callback and freed after; the atom is
/// borrowed (freed internally). No-op if `v` is not an object.
fn for_each_own<F: FnMut(JSAtom, JSValue)>(
  ctx: *mut JSContext,
  v: JSValue,
  mut f: F,
) {
  if !jsv_is_object(&v) {
    return;
  }
  let mut ptab: *mut JSPropertyEnum = std::ptr::null_mut();
  let mut plen: u32 = 0;
  let rc = unsafe {
    JS_GetOwnPropertyNames(ctx, &mut ptab, &mut plen, v, JS_GPN_OWN_ENUM)
  };
  if rc != 0 || ptab.is_null() {
    return;
  }
  for i in 0..plen as usize {
    let atom = unsafe { (*ptab.add(i)).atom };
    let propval = unsafe { JS_GetProperty(ctx, v, atom) };
    f(atom, propval);
    unsafe {
      JS_FreeValue(ctx, propval);
      JS_FreeAtom(ctx, atom);
    }
  }
  unsafe { js_free(ctx, ptab as *mut c_void) };
}

struct SerState {
  buf: Vec<u8>,
  // JSObject pointer of each transferred ArrayBuffer -> its transfer id.
  xfer_ab: std::collections::HashMap<usize, u32>,
  // The deno delegate + isolate, needed to drive host-object serialization
  // (MessagePort / CryptoKey) via the delegate trampolines.
  isolate: *mut RealIsolate,
  delegate: *mut CxxValueSerializerDelegate,
  data_clone_error: bool,
}

struct DeState {
  buf: Vec<u8>,
  pos: usize,
  // transfer id -> the reconstructed ArrayBuffer (owns one ref; freed in DESTRUCT
  // if never consumed).
  xfer_ab: std::collections::HashMap<u32, JSValue>,
  ctx: *mut JSContext,
  isolate: *mut RealIsolate,
  delegate: *mut CxxValueDeserializerDelegate,
}

#[inline]
unsafe fn ser_state<'a>(this: *mut CxxValueSerializer) -> &'a mut SerState {
  let slot = this as *mut *mut SerState;
  unsafe { &mut **slot }
}

#[inline]
unsafe fn de_state<'a>(this: *mut CxxValueDeserializer) -> &'a mut DeState {
  let slot = this as *mut *mut DeState;
  unsafe { &mut **slot }
}

#[inline]
fn read_u8(buf: &[u8], pos: &mut usize) -> Option<u8> {
  if *pos + 1 > buf.len() {
    return None;
  }
  let v = buf[*pos];
  *pos += 1;
  Some(v)
}

#[inline]
fn read_le_u32(buf: &[u8], pos: &mut usize) -> Option<u32> {
  if *pos + 4 > buf.len() {
    return None;
  }
  let v = u32::from_le_bytes(buf[*pos..*pos + 4].try_into().ok()?);
  *pos += 4;
  Some(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__Delegate__CONSTRUCT(
  buf: *mut MaybeUninit<CxxValueSerializerDelegate>,
) {
  unsafe {
    let slot = buf as *mut *mut c_void;
    *slot = std::ptr::null_mut();
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__CONSTRUCT(
  buf: *mut MaybeUninit<CxxValueSerializer>,
  isolate: *mut RealIsolate,
  delegate: *mut CxxValueSerializerDelegate,
) {
  let state = Box::new(SerState {
    buf: Vec::new(),
    xfer_ab: std::collections::HashMap::new(),
    isolate,
    delegate,
    data_clone_error: false,
  });
  unsafe {
    let slot = buf as *mut *mut SerState;
    *slot = Box::into_raw(state);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__DESTRUCT(this: *mut CxxValueSerializer) {
  unsafe {
    let slot = this as *mut *mut SerState;
    if !(*slot).is_null() {
      drop(Box::from_raw(*slot));
      *slot = std::ptr::null_mut();
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__Release(
  this: *mut CxxValueSerializer,
  ptr: *mut *mut u8,
  size: *mut usize,
) {
  let st = unsafe { ser_state(this) };
  let len = st.buf.len();
  if len == 0 {
    unsafe {
      *ptr = std::ptr::null_mut();
      *size = 0;
    }
    return;
  }
  unsafe {
    let layout = std::alloc::Layout::from_size_align(len, 1).unwrap();
    let out = std::alloc::alloc(layout);
    if out.is_null() {
      *ptr = std::ptr::null_mut();
      *size = 0;
      return;
    }
    std::ptr::copy_nonoverlapping(st.buf.as_ptr(), out, len);
    *ptr = out;
    *size = len;
  }
  st.buf.clear();
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__SetTreatArrayBufferViewsAsHostObjects(
  _this: *mut CxxValueSerializer,
  _mode: bool,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__TransferArrayBuffer(
  this: *mut CxxValueSerializer,
  transfer_id: u32,
  array_buffer: Local<ArrayBuffer>,
) {
  let st = unsafe { ser_state(this) };
  let v = jsval_of::<ArrayBuffer>(
    array_buffer.as_non_null().as_ptr() as *const ArrayBuffer
  );
  // The buffer is already detached by the time deno calls this, but its JSObject
  // identity is stable and still appears (detached) in the value graph.
  if jsv_is_object(&v) {
    let ptr = unsafe { v.u.ptr } as usize;
    st.xfer_ab.insert(ptr, transfer_id);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__WriteHeader(
  this: *mut CxxValueSerializer,
) {
  let st = unsafe { ser_state(this) };
  if st.buf.is_empty() {
    st.buf.extend_from_slice(MAGIC);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__WriteValue(
  this: *mut CxxValueSerializer,
  context: Local<Context>,
  value: Local<Value>,
) -> MaybeBool {
  let st = unsafe { ser_state(this) };
  let ctx = ctx_of(context.as_non_null().as_ptr() as *const Context);

  let v = jsval_of::<Value>(value.as_non_null().as_ptr() as *const Value);
  st.data_clone_error = false;

  // Default path: a single opaque `JS_WriteObject` blob under `TAG_VALUE`. Only
  // switch to the recursive graph walk when the value carries a transferred
  // ArrayBuffer (JS_WriteObject throws on the detached buffer) or a host object
  // (MessagePort/CryptoKey — JS_WriteObject would dump it as a dead plain object).
  let brand = host_brand_atom(ctx);
  let needs_walk = graph_needs_walk(st, ctx, v, brand, 0);
  let ok = if !needs_walk {
    ser_blob(st, ctx, v, TAG_VALUE)
  } else {
    st.buf.push(TAG_GRAPH);
    ser_rec(st, ctx, v, brand)
  };
  if brand != 0 {
    unsafe { JS_FreeAtom(ctx, brand) };
  }
  if ok {
    MaybeBool::JustTrue
  } else if st.data_clone_error {
    MaybeBool::Nothing
  } else {
    MaybeBool::JustFalse
  }
}

fn throw_sab_clone_error(st: &mut SerState, ctx: *mut JSContext) {
  st.data_clone_error = true;
  if ctx.is_null() || st.delegate.is_null() {
    return;
  }
  let message = unsafe {
    JS_NewStringLen(
      ctx,
      SAB_CLONE_ERROR.as_ptr() as *const c_char,
      SAB_CLONE_ERROR.len(),
    )
  };
  if message.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return;
  }
  let message = intern::<V8String>(message);
  if !message.is_null() {
    unsafe {
      v8__ValueSerializer__Delegate__ThrowDataCloneError(st.delegate, message);
    }
    throw_error_if_clear(ctx, message);
  }
}

fn throw_error_if_clear(ctx: *mut JSContext, message: *const V8String) {
  if ctx.is_null() || message.is_null() || unsafe { JS_HasException(ctx) } {
    return;
  }
  unsafe {
    let global = JS_GetGlobalObject(ctx);
    let ctor = JS_GetPropertyStr(ctx, global, c"Error".as_ptr());
    JS_FreeValue(ctx, global);
    if ctor.tag == JS_TAG_EXCEPTION || !JS_IsConstructor(ctx, ctor) {
      JS_FreeValue(ctx, ctor);
      return;
    }

    let msg = JS_DupValue(ctx, jsval_of::<V8String>(message));
    let mut args = [msg];
    let err = JS_CallConstructor(ctx, ctor, 1, args.as_mut_ptr());
    JS_FreeValue(ctx, ctor);
    JS_FreeValue(ctx, msg);
    if err.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      return;
    }
    JS_Throw(ctx, err);
  }
}

fn get_shared_array_buffer_id(
  st: &mut SerState,
  ctx: *mut JSContext,
  v: JSValue,
) -> Option<u32> {
  if st.delegate.is_null() {
    return None;
  }
  let sab = intern::<SharedArrayBuffer>(unsafe { JS_DupValue(ctx, v) });
  if sab.is_null() {
    return None;
  }
  let mut id = 0;
  let ok = unsafe {
    v8__ValueSerializer__Delegate__GetSharedArrayBufferId(
      st.delegate,
      st.isolate,
      sab,
      &mut id,
    )
  };
  ok.then_some(id)
}

/// Serialize one value as an opaque `JS_WriteObject` blob under `tag` (a length-
/// prefixed byte run). Used both for the top-level default path (`TAG_VALUE`) and
/// for graph-mode leaves (`TAG_LEAF`).
fn ser_blob(
  st: &mut SerState,
  ctx: *mut JSContext,
  v: JSValue,
  tag: u8,
) -> bool {
  let mut blob_len: usize = 0;
  let blob_ptr = unsafe {
    JS_WriteObject(
      ctx,
      &mut blob_len,
      v,
      JS_WRITE_OBJ_REFERENCE | JS_WRITE_OBJ_SAB,
    )
  };
  if blob_ptr.is_null() {
    return false;
  }
  let blob = unsafe { std::slice::from_raw_parts(blob_ptr, blob_len) };
  st.buf.push(tag);
  st.buf.extend_from_slice(&(blob_len as u32).to_le_bytes());
  st.buf.extend_from_slice(blob);
  unsafe { js_free(ctx, blob_ptr as *mut c_void) };
  true
}

/// Recursive structured-clone walk (graph mode). Intercepts SharedArrayBuffers
/// and transferred ArrayBuffers (emit delegate/transfer-id references), then
/// recurses arrays element-wise; everything else is an opaque `JS_WriteObject`
/// leaf.
fn ser_rec(
  st: &mut SerState,
  ctx: *mut JSContext,
  v: JSValue,
  brand: JSAtom,
) -> bool {
  if jsv_is_object(&v) {
    if is_shared_array_buffer(ctx, v) {
      if let Some(id) = get_shared_array_buffer_id(st, ctx, v) {
        st.buf.push(TAG_SAB);
        st.buf.extend_from_slice(&id.to_le_bytes());
        return true;
      }
      throw_sab_clone_error(st, ctx);
      return false;
    }
    let ptr = unsafe { v.u.ptr } as usize;
    if let Some(&id) = st.xfer_ab.get(&ptr) {
      st.buf.push(TAG_XFER_AB);
      st.buf.extend_from_slice(&id.to_le_bytes());
      return true;
    }
    if is_host_object(ctx, v, brand) {
      st.buf.push(TAG_HOST);
      // Hand the host object to deno's delegate, which appends its index/value
      // to our buffer via our Write{Uint32,Value} fns.
      let obj = intern::<Object>(unsafe { JS_DupValue(ctx, v) });
      let r = unsafe {
        v8__ValueSerializer__Delegate__WriteHostObject(
          st.delegate,
          st.isolate,
          obj,
        )
      };
      return matches!(r, MaybeBool::JustTrue);
    }
    if unsafe { JS_IsArray(v) } {
      let lenval = unsafe { JS_GetPropertyStr(ctx, v, c"length".as_ptr()) };
      let mut len: i32 = 0;
      unsafe {
        JS_ToInt32(ctx, &mut len, lenval);
        JS_FreeValue(ctx, lenval);
      }
      let len = len.max(0) as u32;
      st.buf.push(TAG_ARRAY);
      st.buf.extend_from_slice(&len.to_le_bytes());
      for i in 0..len {
        let el = unsafe { JS_GetPropertyUint32(ctx, v, i) };
        let ok = ser_rec(st, ctx, el, brand);
        unsafe { JS_FreeValue(ctx, el) };
        if !ok {
          return false;
        }
      }
      return true;
    }
    // Plain object whose subtree holds a host object / transferred AB: enumerate
    // own enumerable props so the nested host object is intercepted. Objects with
    // no such subtree fall through to an opaque JS_WriteObject leaf (full
    // fidelity — keeps property attributes, Map/Set, getters, etc.).
    if graph_needs_walk(st, ctx, v, brand, 0) {
      let mut count: u32 = 0;
      for_each_own(ctx, v, |_a, _val| count += 1);
      st.buf.push(TAG_OBJECT);
      st.buf.extend_from_slice(&count.to_le_bytes());
      let mut ok = true;
      for_each_own(ctx, v, |atom, propval| {
        if !ok {
          return;
        }
        let keyval = unsafe { JS_AtomToValue(ctx, atom) };
        let kok = ser_blob(st, ctx, keyval, TAG_LEAF);
        unsafe { JS_FreeValue(ctx, keyval) };
        if !kok || !ser_rec(st, ctx, propval, brand) {
          ok = false;
        }
      });
      return ok;
    }
  }
  ser_blob(st, ctx, v, TAG_LEAF)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__WriteUint32(
  this: *mut CxxValueSerializer,
  value: u32,
) {
  let st = unsafe { ser_state(this) };
  st.buf.push(TAG_U32);
  st.buf.extend_from_slice(&value.to_le_bytes());
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__WriteUint64(
  this: *mut CxxValueSerializer,
  value: u64,
) {
  let st = unsafe { ser_state(this) };
  st.buf.push(TAG_U64);
  st.buf.extend_from_slice(&value.to_le_bytes());
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__WriteDouble(
  this: *mut CxxValueSerializer,
  value: f64,
) {
  let st = unsafe { ser_state(this) };
  st.buf.push(TAG_DOUBLE);
  st.buf.extend_from_slice(&value.to_le_bytes());
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__WriteRawBytes(
  this: *mut CxxValueSerializer,
  source: *const c_void,
  length: usize,
) {
  let st = unsafe { ser_state(this) };
  st.buf.push(TAG_RAW);
  st.buf.extend_from_slice(&(length as u32).to_le_bytes());
  if !source.is_null() && length > 0 {
    let slice =
      unsafe { std::slice::from_raw_parts(source as *const u8, length) };
    st.buf.extend_from_slice(slice);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__Delegate__CONSTRUCT(
  buf: *mut MaybeUninit<CxxValueDeserializerDelegate>,
) {
  unsafe {
    let slot = buf as *mut *mut c_void;
    *slot = std::ptr::null_mut();
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__CONSTRUCT(
  buf: *mut MaybeUninit<CxxValueDeserializer>,
  isolate: *mut RealIsolate,
  data: *const u8,
  size: usize,
  delegate: *mut CxxValueDeserializerDelegate,
) {
  let bytes = if data.is_null() || size == 0 {
    Vec::new()
  } else {
    unsafe { std::slice::from_raw_parts(data, size).to_vec() }
  };
  let state = Box::new(DeState {
    buf: bytes,
    pos: 0,
    xfer_ab: std::collections::HashMap::new(),
    ctx: std::ptr::null_mut(),
    isolate,
    delegate,
  });
  unsafe {
    let slot = buf as *mut *mut DeState;
    *slot = Box::into_raw(state);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__DESTRUCT(
  this: *mut CxxValueDeserializer,
) {
  unsafe {
    let slot = this as *mut *mut DeState;
    if !(*slot).is_null() {
      let boxed = Box::from_raw(*slot);
      if !boxed.ctx.is_null() {
        for (_, v) in boxed.xfer_ab.iter() {
          JS_FreeValue(boxed.ctx, *v);
        }
      }
      drop(boxed);
      *slot = std::ptr::null_mut();
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__TransferArrayBuffer(
  this: *mut CxxValueDeserializer,
  transfer_id: u32,
  array_buffer: Local<ArrayBuffer>,
) {
  let st = unsafe { de_state(this) };
  let ctx = current_ctx();
  let v = jsval_of::<ArrayBuffer>(
    array_buffer.as_non_null().as_ptr() as *const ArrayBuffer
  );
  if !ctx.is_null() && jsv_is_object(&v) {
    st.ctx = ctx;
    let owned = unsafe { JS_DupValue(ctx, v) };
    if let Some(old) = st.xfer_ab.insert(transfer_id, owned) {
      unsafe { JS_FreeValue(ctx, old) };
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__TransferSharedArrayBuffer(
  _this: *mut CxxValueDeserializer,
  _transfer_id: u32,
  _array_buffer: Local<SharedArrayBuffer>,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__SetSupportsLegacyWireFormat(
  _this: *mut CxxValueDeserializer,
  _supports_legacy_wire_format: bool,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__ReadHeader(
  this: *mut CxxValueDeserializer,
  _context: Local<Context>,
) -> MaybeBool {
  let st = unsafe { de_state(this) };

  if st.buf.len() - st.pos >= 4 && &st.buf[st.pos..st.pos + 4] == &MAGIC[..] {
    st.pos += 4;
  }
  MaybeBool::JustTrue
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__GetWireFormatVersion(
  _this: *mut CxxValueDeserializer,
) -> u32 {
  15
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__ReadValue(
  this: *mut CxxValueDeserializer,
  context: Local<Context>,
) -> *const Value {
  let st = unsafe { de_state(this) };
  let ctx = ctx_of(context.as_non_null().as_ptr() as *const Context);

  let tag = match read_u8(&st.buf, &mut st.pos) {
    Some(t) => t,
    None => return std::ptr::null(),
  };
  let v = match tag {
    TAG_VALUE => de_blob(st, ctx),
    TAG_GRAPH => de_rec(st, ctx),
    _ => return std::ptr::null(),
  };
  if v.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return std::ptr::null();
  }
  intern::<Value>(v)
}

/// Read an opaque `JS_WriteObject` blob (the bytes after a `TAG_VALUE`/`TAG_LEAF`
/// tag that the caller already consumed). Returns `JS_EXCEPTION` on any failure.
fn de_blob(st: &mut DeState, ctx: *mut JSContext) -> JSValue {
  let len = match read_le_u32(&st.buf, &mut st.pos) {
    Some(l) => l as usize,
    None => return jsv_exception(),
  };
  if st.pos + len > st.buf.len() {
    return jsv_exception();
  }
  let start = st.pos;
  st.pos += len;
  unsafe {
    JS_ReadObject(
      ctx,
      st.buf.as_ptr().add(start),
      len,
      JS_READ_OBJ_REFERENCE | JS_READ_OBJ_SAB,
    )
  }
}

/// Recursive structured-clone read (graph mode). Mirrors `ser_rec`.
fn de_rec(st: &mut DeState, ctx: *mut JSContext) -> JSValue {
  match read_u8(&st.buf, &mut st.pos) {
    Some(TAG_XFER_AB) => {
      let id = match read_le_u32(&st.buf, &mut st.pos) {
        Some(i) => i,
        None => return jsv_exception(),
      };
      match st.xfer_ab.get(&id) {
        Some(&ab) => unsafe { JS_DupValue(ctx, ab) },
        None => jsv_exception(),
      }
    }
    Some(TAG_SAB) => {
      let id = match read_le_u32(&st.buf, &mut st.pos) {
        Some(i) => i,
        None => return jsv_exception(),
      };
      let sab = unsafe {
        v8__ValueDeserializer__Delegate__GetSharedArrayBufferFromId(
          st.delegate,
          st.isolate,
          id,
        )
      };
      if sab.is_null() {
        return jsv_exception();
      }
      let v = jsval_of::<SharedArrayBuffer>(sab);
      unsafe { JS_DupValue(ctx, v) }
    }
    Some(TAG_ARRAY) => {
      let len = match read_le_u32(&st.buf, &mut st.pos) {
        Some(l) => l,
        None => return jsv_exception(),
      };
      let arr = unsafe { JS_NewArray(ctx) };
      if arr.tag == JS_TAG_EXCEPTION {
        return arr;
      }
      for i in 0..len {
        let el = de_rec(st, ctx);
        if el.tag == JS_TAG_EXCEPTION {
          unsafe { JS_FreeValue(ctx, arr) };
          return jsv_exception();
        }
        unsafe { JS_SetPropertyUint32(ctx, arr, i, el) };
      }
      arr
    }
    Some(TAG_HOST) => {
      let obj = unsafe {
        v8__ValueDeserializer__Delegate__ReadHostObject(st.delegate, st.isolate)
      };
      if obj.is_null() {
        return jsv_exception();
      }
      let v = jsval_of::<Object>(obj);
      unsafe { JS_DupValue(ctx, v) }
    }
    Some(TAG_OBJECT) => {
      let count = match read_le_u32(&st.buf, &mut st.pos) {
        Some(c) => c,
        None => return jsv_exception(),
      };
      let obj = unsafe { JS_NewObject(ctx) };
      if obj.tag == JS_TAG_EXCEPTION {
        return obj;
      }
      for _ in 0..count {
        let key = de_rec(st, ctx);
        if key.tag == JS_TAG_EXCEPTION {
          unsafe { JS_FreeValue(ctx, obj) };
          return jsv_exception();
        }
        let val = de_rec(st, ctx);
        if val.tag == JS_TAG_EXCEPTION {
          unsafe {
            JS_FreeValue(ctx, key);
            JS_FreeValue(ctx, obj);
          }
          return jsv_exception();
        }
        let atom = unsafe { JS_ValueToAtom(ctx, key) };
        unsafe {
          JS_FreeValue(ctx, key);
          JS_SetProperty(ctx, obj, atom, val);
          JS_FreeAtom(ctx, atom);
        }
      }
      obj
    }
    Some(TAG_LEAF) => de_blob(st, ctx),
    _ => jsv_exception(),
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__ReadUint32(
  this: *mut CxxValueDeserializer,
  value: *mut u32,
) -> bool {
  let st = unsafe { de_state(this) };
  let save = st.pos;
  if read_u8(&st.buf, &mut st.pos) != Some(TAG_U32) {
    st.pos = save;
    return false;
  }
  match read_le_u32(&st.buf, &mut st.pos) {
    Some(v) => {
      unsafe { *value = v };
      true
    }
    None => {
      st.pos = save;
      false
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__ReadUint64(
  this: *mut CxxValueDeserializer,
  value: *mut u64,
) -> bool {
  let st = unsafe { de_state(this) };
  let save = st.pos;
  if read_u8(&st.buf, &mut st.pos) != Some(TAG_U64) || st.pos + 8 > st.buf.len()
  {
    st.pos = save;
    return false;
  }
  let v = u64::from_le_bytes(st.buf[st.pos..st.pos + 8].try_into().unwrap());
  st.pos += 8;
  unsafe { *value = v };
  true
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__ReadDouble(
  this: *mut CxxValueDeserializer,
  value: *mut f64,
) -> bool {
  let st = unsafe { de_state(this) };
  let save = st.pos;
  if read_u8(&st.buf, &mut st.pos) != Some(TAG_DOUBLE)
    || st.pos + 8 > st.buf.len()
  {
    st.pos = save;
    return false;
  }
  let v = f64::from_le_bytes(st.buf[st.pos..st.pos + 8].try_into().unwrap());
  st.pos += 8;
  unsafe { *value = v };
  true
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__ReadRawBytes(
  this: *mut CxxValueDeserializer,
  length: usize,
  data: *mut *const c_void,
) -> bool {
  let st = unsafe { de_state(this) };
  let save = st.pos;

  if read_u8(&st.buf, &mut st.pos) != Some(TAG_RAW) {
    st.pos = save;
    return false;
  }
  let stored = match read_le_u32(&st.buf, &mut st.pos) {
    Some(l) => l as usize,
    None => {
      st.pos = save;
      return false;
    }
  };
  let _ = stored;
  if st.pos + length > st.buf.len() {
    st.pos = save;
    return false;
  }
  let p = unsafe { st.buf.as_ptr().add(st.pos) } as *const c_void;
  st.pos += length;
  unsafe { *data = p };
  true
}
