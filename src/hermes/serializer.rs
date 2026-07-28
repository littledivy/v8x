//! E5: v8::ValueSerializer / ValueDeserializer for the Hermes backend.
//!
//! deno_core's `op_structured_clone` (and structuredClone in ext/web) drives
//! `v8::ValueSerializer`/`Deserializer`: `write_header()`, `write_value(v)`,
//! `release()` -> bytes, then `ValueDeserializer::new(bytes)`, `read_header()`,
//! `read_value()`. On real V8 those produce/consume V8's structured-clone wire
//! format. Hermes/JSI has no native value serialization, so the actual value
//! <-> bytes conversion is a self-describing recursive walk over JSI values,
//! implemented in C++ (`v8x_hermes_structured_serialize` /
//! `v8x_hermes_structured_deserialize`, in hermes_shim.cpp). This Rust layer
//! owns the byte buffer and the deno delegate glue, mirroring the QuickJS
//! backend's structure.
//!
//! Scope (honest): primitives (null, undefined, bool, number), strings, arrays,
//! and plain string-keyed objects round-trip. BigInt, Symbol, Date, RegExp,
//! Map, Set, TypedArray/ArrayBuffer, transferables, host objects, and object
//! identity/cycles are NOT covered - an uncloneable value makes `write_value`
//! return `Nothing`, so the op reports a clean DataCloneError instead of
//! corrupting. That covers the E5 win condition (`structuredClone({a:1,b:[2,3]})`
//! deep-equals the input) and the common JSON-shaped clones.
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::mem::MaybeUninit;

use crate::support::MaybeBool;
use crate::value_deserializer::CxxValueDeserializer;
use crate::value_serializer::CxxValueSerializer;
use crate::{Context, Local, RealIsolate, Value};

use super::core::{current_rtw, slot_of, slot_ptr};

unsafe extern "C" {
  fn v8x_hermes_structured_serialize(
    rtw: *mut c_void,
    slot: i64,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
  ) -> std::os::raw::c_int;
  fn v8x_hermes_sc_free(ptr: *mut u8);
  fn v8x_hermes_structured_deserialize(
    rtw: *mut c_void,
    data: *const u8,
    len: usize,
  ) -> i64;
}

// deno's read_header validates a leading header. We bracket the JSI payload with
// a fixed 4-byte magic so read_header can sanity-check the stream came from us.
const HDR: &[u8; 4] = b"HRMV";

// ---- Serializer -----------------------------------------------------------

struct SerState {
  buf: Vec<u8>,
}

#[inline]
unsafe fn ser_state<'a>(this: *mut CxxValueSerializer) -> &'a mut SerState {
  let slot = this as *mut *mut SerState;
  unsafe { &mut **slot }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__Delegate__CONSTRUCT(
  buf: *mut MaybeUninit<crate::value_serializer::CxxValueSerializerDelegate>,
) {
  // The Hermes serializer drives no delegate callbacks (no transferables / host
  // objects), so the delegate is just a null placeholder slot.
  unsafe {
    let slot = buf as *mut *mut c_void;
    *slot = std::ptr::null_mut();
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__CONSTRUCT(
  buf: *mut MaybeUninit<CxxValueSerializer>,
  _isolate: *mut RealIsolate,
  _delegate: *mut crate::value_serializer::CxxValueSerializerDelegate,
) {
  let state = Box::new(SerState { buf: Vec::new() });
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
pub extern "C" fn v8__ValueSerializer__WriteHeader(
  this: *mut CxxValueSerializer,
) {
  let st = unsafe { ser_state(this) };
  st.buf.extend_from_slice(HDR);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__WriteValue(
  this: *mut CxxValueSerializer,
  _context: Local<Context>,
  value: Local<Value>,
) -> MaybeBool {
  let st = unsafe { ser_state(this) };
  let rtw = current_rtw();
  if rtw.is_null() {
    return MaybeBool::JustFalse;
  }
  let slot = slot_of(value.as_non_null().as_ptr() as *const Value);
  let mut ptr: *mut u8 = std::ptr::null_mut();
  let mut len: usize = 0;
  let ok = unsafe {
    v8x_hermes_structured_serialize(rtw, slot, &mut ptr, &mut len)
  };
  if ok == 0 || ptr.is_null() {
    // Uncloneable value: report Nothing so deno raises a DataCloneError.
    return MaybeBool::Nothing;
  }
  let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
  st.buf.extend_from_slice(bytes);
  unsafe { v8x_hermes_sc_free(ptr) };
  MaybeBool::JustTrue
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
  // No ArrayBufferView/host-object support in this backend's clone path.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__WriteUint32(
  this: *mut CxxValueSerializer,
  value: u32,
) {
  let st = unsafe { ser_state(this) };
  st.buf.extend_from_slice(&value.to_le_bytes());
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__WriteUint64(
  this: *mut CxxValueSerializer,
  value: u64,
) {
  let st = unsafe { ser_state(this) };
  st.buf.extend_from_slice(&value.to_le_bytes());
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__WriteDouble(
  this: *mut CxxValueSerializer,
  value: f64,
) {
  let st = unsafe { ser_state(this) };
  st.buf.extend_from_slice(&value.to_le_bytes());
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__WriteRawBytes(
  this: *mut CxxValueSerializer,
  source: *const c_void,
  length: usize,
) {
  let st = unsafe { ser_state(this) };
  if !source.is_null() && length > 0 {
    let slice =
      unsafe { std::slice::from_raw_parts(source as *const u8, length) };
    st.buf.extend_from_slice(slice);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__TransferArrayBuffer(
  _this: *mut CxxValueSerializer,
  _transfer_id: u32,
  _array_buffer: Local<crate::ArrayBuffer>,
) {
  // Transferables are out of scope; the walk refuses ArrayBuffers, so a
  // transfer never reaches deserialization.
}

// ---- Deserializer ---------------------------------------------------------

struct DeState {
  buf: Vec<u8>,
  source_data: *const u8,
  pos: usize,
}

#[inline]
unsafe fn de_state<'a>(this: *mut CxxValueDeserializer) -> &'a mut DeState {
  let slot = this as *mut *mut DeState;
  unsafe { &mut **slot }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__Delegate__CONSTRUCT(
  buf: *mut MaybeUninit<crate::value_deserializer::CxxValueDeserializerDelegate>,
) {
  unsafe {
    let slot = buf as *mut *mut c_void;
    *slot = std::ptr::null_mut();
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__CONSTRUCT(
  buf: *mut MaybeUninit<CxxValueDeserializer>,
  _isolate: *mut RealIsolate,
  data: *const u8,
  size: usize,
  _delegate: *mut crate::value_deserializer::CxxValueDeserializerDelegate,
) {
  let bytes = if data.is_null() || size == 0 {
    Vec::new()
  } else {
    unsafe { std::slice::from_raw_parts(data, size).to_vec() }
  };
  let state = Box::new(DeState {
    buf: bytes,
    source_data: data,
    pos: 0,
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
      drop(Box::from_raw(*slot));
      *slot = std::ptr::null_mut();
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__ReadHeader(
  this: *mut CxxValueDeserializer,
  _context: Local<Context>,
) -> MaybeBool {
  let st = unsafe { de_state(this) };
  if st.buf.len() - st.pos >= HDR.len()
    && &st.buf[st.pos..st.pos + HDR.len()] == &HDR[..]
  {
    st.pos += HDR.len();
    MaybeBool::JustTrue
  } else {
    MaybeBool::JustFalse
  }
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
  _context: Local<Context>,
) -> *const Value {
  let st = unsafe { de_state(this) };
  let rtw = current_rtw();
  if rtw.is_null() || st.pos > st.buf.len() {
    return std::ptr::null();
  }
  let remaining = &st.buf[st.pos..];
  let slot = unsafe {
    v8x_hermes_structured_deserialize(
      rtw,
      remaining.as_ptr(),
      remaining.len(),
    )
  };
  // The C++ reader consumes the whole remaining payload for a single top-level
  // value; deno only calls read_value once for structuredClone. Advance to end
  // so a stray second read returns null rather than re-parsing.
  st.pos = st.buf.len();
  if slot < 0 {
    return std::ptr::null();
  }
  slot_ptr::<Value>(slot)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__ReadUint32(
  this: *mut CxxValueDeserializer,
  value: *mut u32,
) -> bool {
  let st = unsafe { de_state(this) };
  if st.pos + 4 > st.buf.len() {
    return false;
  }
  let v = u32::from_le_bytes(st.buf[st.pos..st.pos + 4].try_into().unwrap());
  st.pos += 4;
  unsafe { *value = v };
  true
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__ReadUint64(
  this: *mut CxxValueDeserializer,
  value: *mut u64,
) -> bool {
  let st = unsafe { de_state(this) };
  if st.pos + 8 > st.buf.len() {
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
  if st.pos + 8 > st.buf.len() {
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
  if st.source_data.is_null() || st.pos + length > st.buf.len() {
    return false;
  }
  let p = unsafe { st.source_data.add(st.pos) } as *const c_void;
  st.pos += length;
  unsafe { *data = p };
  true
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__TransferArrayBuffer(
  _this: *mut CxxValueDeserializer,
  _transfer_id: u32,
  _array_buffer: Local<crate::ArrayBuffer>,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__TransferSharedArrayBuffer(
  _this: *mut CxxValueDeserializer,
  _transfer_id: u32,
  _array_buffer: Local<crate::SharedArrayBuffer>,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__SetSupportsLegacyWireFormat(
  _this: *mut CxxValueDeserializer,
  _supports_legacy_wire_format: bool,
) {
}
