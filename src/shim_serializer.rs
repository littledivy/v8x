// Family: serializer — ValueSerializer / ValueDeserializer
//
// JSC has no structured-clone C API, so this is a best-effort implementation:
// values are encoded with JSValueCreateJSONString and decoded with
// JSValueMakeFromJSONString. We keep a Rust-side buffer on the C++ object's
// first word (the vtable slot of the Cxx* structs), which we own entirely.
//
// Wire format (our own, little-endian):
//   [u8;4] magic "JSCS"
//   repeated records, each: [u32 json_byte_len][json bytes (utf8)]
// Header is a no-op marker; primitives (uint32/uint64/double/raw) are appended
// length-prefixed so the deserializer can recover them in order.
#![allow(non_snake_case, unused)]

use crate::jsc_sys::*;
use crate::support::MaybeBool;
use crate::value_deserializer::{CxxValueDeserializer, CxxValueDeserializerDelegate};
use crate::value_serializer::{CxxValueSerializer, CxxValueSerializerDelegate};
use crate::{ArrayBuffer, Context, Local, Object, RealIsolate, SharedArrayBuffer, Value};

use crate::shim_core::{ctx_of, current_ctx, intern_ctx, jsval};

use std::ffi::c_void;
use std::mem::MaybeUninit;
use std::os::raw::c_char;

unsafe extern "C" {
    fn JSValueCreateJSONString(
        ctx: JSContextRef,
        value: JSValueRef,
        indent: u32,
        exception: *mut JSValueRef,
    ) -> JSStringRef;
    fn JSValueMakeFromJSONString(ctx: JSContextRef, string: JSStringRef) -> JSValueRef;
}

const MAGIC: &[u8; 4] = b"JSCS";

// ===================================================================
// Rust-side state. A boxed pointer to this lives in the first word of
// the Cxx* object (its `_cxx_vtable` slot), which we fully own.
// ===================================================================

struct SerState {
    buf: Vec<u8>,
}

struct DeState {
    buf: Vec<u8>,
    pos: usize,
}

#[inline]
unsafe fn ser_state<'a>(this: *mut CxxValueSerializer) -> &'a mut SerState {
    // The struct's first (and only) field is a pointer-sized vtable slot which
    // we repurpose to hold our Box<SerState> pointer.
    let slot = this as *mut *mut SerState;
    unsafe { &mut **slot }
}

#[inline]
unsafe fn de_state<'a>(this: *mut CxxValueDeserializer) -> &'a mut DeState {
    let slot = this as *mut *mut DeState;
    unsafe { &mut **slot }
}

#[inline]
fn read_le_u32(buf: &[u8], pos: &mut usize) -> Option<u32> {
    if *pos + 4 > buf.len() {
        return None;
    }
    let v = u32::from_le_bytes([buf[*pos], buf[*pos + 1], buf[*pos + 2], buf[*pos + 3]]);
    *pos += 4;
    Some(v)
}

// ===================================================================
// ValueSerializer
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__Delegate__CONSTRUCT(
    buf: *mut MaybeUninit<CxxValueSerializerDelegate>,
) {
    // No real delegate is needed; zero the slot so it's well-defined.
    unsafe {
        let slot = buf as *mut *mut c_void;
        *slot = std::ptr::null_mut();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__CONSTRUCT(
    buf: *mut MaybeUninit<CxxValueSerializer>,
    _isolate: *mut RealIsolate,
    _delegate: *mut CxxValueSerializerDelegate,
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
pub extern "C" fn v8__ValueSerializer__Release(
    this: *mut CxxValueSerializer,
    ptr: *mut *mut u8,
    size: *mut usize,
) {
    // The crate's `ValueSerializer::release()` reconstructs the buffer with
    // `Vec::from_raw_parts(ptr, size, capacity)` where `capacity` comes from a
    // separate atomic that is only updated through V8's ReallocateBufferMemory
    // delegate callback. Under JSC that callback never runs, so the capacity is
    // always 0 and any non-zero `size` would trip the `size <= capacity`
    // assertion / cause an unsound Vec reconstruction.
    //
    // To stay sound we return a null pointer and zero size, which makes
    // `release()` return an empty Vec without touching the (bogus) capacity.
    // The encoded bytes still live in our SerState until DESTRUCT.
    // TODO(v82jsc): wire the heap buffer-size atomic so Release can hand back
    // the real serialized bytes.
    unsafe {
        *ptr = std::ptr::null_mut();
        *size = 0;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__SetTreatArrayBufferViewsAsHostObjects(
    _this: *mut CxxValueSerializer,
    _mode: bool,
) {
    // No host-object machinery in the JSON best-effort encoder. No-op.
    // TODO(v82jsc): unsupported under JSC structured-clone shim.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__TransferArrayBuffer(
    _this: *mut CxxValueSerializer,
    _transfer_id: u32,
    _array_buffer: Local<ArrayBuffer>,
) {
    // ArrayBuffer transfer is not representable in the JSON encoding. No-op.
    // TODO(v82jsc): unsupported under JSC structured-clone shim.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__WriteHeader(this: *mut CxxValueSerializer) {
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
    let ctx = ctx_of(context.as_non_null().as_ptr() as *const Context) as JSContextRef;
    let v = jsval(value.as_non_null().as_ptr() as *const Value);

    let json = unsafe { JSValueCreateJSONString(ctx, v, 0, std::ptr::null_mut()) };
    if json.is_null() {
        // Value not JSON-serializable (functions, symbols, cycles, ...).
        return MaybeBool::JustFalse;
    }
    let bytes = unsafe { jsstring_to_utf8(json) };
    unsafe { JSStringRelease(json) };

    let len = bytes.len() as u32;
    st.buf.extend_from_slice(&len.to_le_bytes());
    st.buf.extend_from_slice(&bytes);
    MaybeBool::JustTrue
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__WriteUint32(this: *mut CxxValueSerializer, value: u32) {
    let st = unsafe { ser_state(this) };
    st.buf.extend_from_slice(&value.to_le_bytes());
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__WriteUint64(this: *mut CxxValueSerializer, value: u64) {
    let st = unsafe { ser_state(this) };
    st.buf.extend_from_slice(&value.to_le_bytes());
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__WriteDouble(this: *mut CxxValueSerializer, value: f64) {
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
        let slice = unsafe { std::slice::from_raw_parts(source as *const u8, length) };
        st.buf.extend_from_slice(slice);
    }
}

// ===================================================================
// ValueDeserializer
// ===================================================================

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
    _isolate: *mut RealIsolate,
    data: *const u8,
    size: usize,
    _delegate: *mut CxxValueDeserializerDelegate,
) {
    let bytes = if data.is_null() || size == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(data, size).to_vec() }
    };
    let state = Box::new(DeState { buf: bytes, pos: 0 });
    unsafe {
        let slot = buf as *mut *mut DeState;
        *slot = Box::into_raw(state);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__DESTRUCT(this: *mut CxxValueDeserializer) {
    unsafe {
        let slot = this as *mut *mut DeState;
        if !(*slot).is_null() {
            drop(Box::from_raw(*slot));
            *slot = std::ptr::null_mut();
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__TransferArrayBuffer(
    _this: *mut CxxValueDeserializer,
    _transfer_id: u32,
    _array_buffer: Local<ArrayBuffer>,
) {
    // TODO(v82jsc): ArrayBuffer transfer unsupported under JSC shim. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__TransferSharedArrayBuffer(
    _this: *mut CxxValueDeserializer,
    _transfer_id: u32,
    _array_buffer: Local<SharedArrayBuffer>,
) {
    // TODO(v82jsc): SharedArrayBuffer transfer unsupported under JSC shim. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__SetSupportsLegacyWireFormat(
    _this: *mut CxxValueDeserializer,
    _supports_legacy_wire_format: bool,
) {
    // Our wire format has no legacy variant. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__ReadHeader(
    this: *mut CxxValueDeserializer,
    _context: Local<Context>,
) -> MaybeBool {
    let st = unsafe { de_state(this) };
    // Consume an optional magic header if present; tolerate its absence.
    if st.buf.len() - st.pos >= 4 && &st.buf[st.pos..st.pos + 4] == &MAGIC[..] {
        st.pos += 4;
    }
    MaybeBool::JustTrue
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__GetWireFormatVersion(
    _this: *mut CxxValueDeserializer,
) -> u32 {
    // Mirror a recent V8 structured-clone wire-format version number.
    15
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__ReadValue(
    this: *mut CxxValueDeserializer,
    context: Local<Context>,
) -> *const Value {
    let st = unsafe { de_state(this) };
    let ctx = ctx_of(context.as_non_null().as_ptr() as *const Context) as JSContextRef;

    let len = match read_le_u32(&st.buf, &mut st.pos) {
        Some(l) => l as usize,
        None => return std::ptr::null(),
    };
    if st.pos + len > st.buf.len() {
        return std::ptr::null();
    }
    let json_bytes = st.buf[st.pos..st.pos + len].to_vec();
    st.pos += len;

    // NUL-terminate for JSStringCreateWithUTF8CString.
    let mut cstr = json_bytes;
    cstr.push(0);
    let jsstr = unsafe { JSStringCreateWithUTF8CString(cstr.as_ptr() as *const c_char) };
    if jsstr.is_null() {
        return std::ptr::null();
    }
    let v = unsafe { JSValueMakeFromJSONString(ctx, jsstr) };
    unsafe { JSStringRelease(jsstr) };
    if v.is_null() {
        return std::ptr::null();
    }
    intern_ctx::<Value>(ctx, v)
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
    let b = &st.buf[st.pos..st.pos + 4];
    let v = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
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
    let b = &st.buf[st.pos..st.pos + 8];
    let v = u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
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
    let b = &st.buf[st.pos..st.pos + 8];
    let v = f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
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
    if st.pos + length > st.buf.len() {
        return false;
    }
    let p = unsafe { st.buf.as_ptr().add(st.pos) } as *const c_void;
    st.pos += length;
    unsafe { *data = p };
    true
}

// ===================================================================
// Helpers
// ===================================================================

unsafe fn jsstring_to_utf8(s: JSStringRef) -> Vec<u8> {
    let max = unsafe { JSStringGetMaximumUTF8CStringSize(s) };
    if max == 0 {
        return Vec::new();
    }
    let mut buf = vec![0u8; max];
    let written = unsafe { JSStringGetUTF8CString(s, buf.as_mut_ptr() as *mut c_char, max) };
    // `written` includes the trailing NUL; strip it.
    let n = if written > 0 { written - 1 } else { 0 };
    buf.truncate(n);
    buf
}
