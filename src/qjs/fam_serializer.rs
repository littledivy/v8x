// Family: serializer — ValueSerializer / ValueDeserializer (QuickJS-ng backend)
//
// QuickJS-ng exposes `JS_WriteObject` / `JS_ReadObject`, a real object
// (de)serialization mechanism used for its bytecode cache. With the
// `JS_WRITE_OBJ_REFERENCE` flag it round-trips arbitrary JS values (objects,
// arrays, typed arrays, primitives, shared references / cycles), which is a
// far better structured-clone analogue than JSON. We use it for
// Write/ReadValue. The remaining primitive writers (uint32/uint64/double/raw)
// and the header are encoded ourselves, length/format-tagged so the reader can
// recover them in order — mirroring the JSC shim's self-describing wire format.
//
// Wire format (little-endian), produced/consumed by this shim only:
//   [u8;4] magic "QJSV"                              (WriteHeader)
//   value record:   [u8 tag=V][u32 blob_len][blob]   (WriteValue,  JS_WriteObject)
//   u32 record:     [u8 tag=4][u32 le]               (WriteUint32)
//   u64 record:     [u8 tag=8][u64 le]               (WriteUint64)
//   double record:  [u8 tag=D][f64 le]               (WriteDouble)
//   raw record:     [u8 tag=R][u32 len][bytes]       (WriteRawBytes)
//
// A Box<SerState>/Box<DeState> pointer is stashed in the first (vtable) word of
// the Cxx* object, which we own outright. Refcounts: JS_WriteObject borrows the
// value (no free needed beyond what we own); JS_ReadObject RETURNS a +1 owned
// JSValue which we hand to `intern` (moves ownership into the handle arena).
#![allow(non_snake_case, unused)]

use crate::qjs::quickjs_sys::*;
use crate::qjs::shim_core::{ctx_of, current_ctx, current_iso, intern, jsval_of};

use crate::support::MaybeBool;
use crate::value_deserializer::{CxxValueDeserializer, CxxValueDeserializerDelegate};
use crate::value_serializer::{CxxValueSerializer, CxxValueSerializerDelegate};
use crate::{ArrayBuffer, Context, Local, Object, RealIsolate, SharedArrayBuffer, Value};

use std::ffi::c_void;
use std::mem::MaybeUninit;
use std::os::raw::c_int;

// QuickJS-ng JS_WriteObject / JS_ReadObject flags (from quickjs.h). Not yet in
// quickjs_sys.rs, declared locally.
const JS_WRITE_OBJ_REFERENCE: c_int = 1 << 3;
const JS_READ_OBJ_REFERENCE: c_int = 1 << 3;

const MAGIC: &[u8; 4] = b"QJSV";

const TAG_VALUE: u8 = b'V';
const TAG_U32: u8 = 4;
const TAG_U64: u8 = 8;
const TAG_DOUBLE: u8 = b'D';
const TAG_RAW: u8 = b'R';

// ===================================================================
// Rust-side state stored in the Cxx object's first (vtable) word.
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

// ===================================================================
// ValueSerializer
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__Delegate__CONSTRUCT(
    buf: *mut MaybeUninit<CxxValueSerializerDelegate>,
) {
    // No real delegate machinery; zero the slot so it's well-defined.
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
    // separate atomic only updated via V8's ReallocateBufferMemory delegate
    // callback. Under QuickJS that callback never runs, so capacity is always 0
    // and any non-zero `size` would trip the `size <= capacity` assertion /
    // cause an unsound Vec reconstruction.
    //
    // To stay sound we return a null pointer and zero size, which makes
    // `release()` return an empty Vec without touching the (bogus) capacity.
    // The encoded bytes still live in our SerState until DESTRUCT.
    // TODO(qjs): wire the heap buffer-size atomic so Release can hand back the
    // real serialized bytes.
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
    // No host-object machinery in this encoder. No-op.
    // TODO(qjs): unsupported under QuickJS structured-clone shim.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__TransferArrayBuffer(
    _this: *mut CxxValueSerializer,
    _transfer_id: u32,
    _array_buffer: Local<ArrayBuffer>,
) {
    // ArrayBuffer transfer is not modelled here. No-op.
    // TODO(qjs): unsupported under QuickJS structured-clone shim.
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
    let ctx = ctx_of(context.as_non_null().as_ptr() as *const Context);
    // Borrowed read of the handle's JSValue; JS_WriteObject does not consume it.
    let v = jsval_of::<Value>(value.as_non_null().as_ptr() as *const Value);

    let mut blob_len: usize = 0;
    let blob_ptr =
        unsafe { JS_WriteObject(ctx, &mut blob_len, v, JS_WRITE_OBJ_REFERENCE) };
    if blob_ptr.is_null() {
        // Not serializable (e.g. a thrown error left pending). Report failure.
        return MaybeBool::JustFalse;
    }
    let blob = unsafe { std::slice::from_raw_parts(blob_ptr, blob_len) };

    st.buf.push(TAG_VALUE);
    st.buf
        .extend_from_slice(&(blob_len as u32).to_le_bytes());
    st.buf.extend_from_slice(blob);

    // The blob was allocated by QuickJS via js_malloc; release it.
    unsafe { js_free(ctx, blob_ptr as *mut c_void) };
    MaybeBool::JustTrue
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__WriteUint32(this: *mut CxxValueSerializer, value: u32) {
    let st = unsafe { ser_state(this) };
    st.buf.push(TAG_U32);
    st.buf.extend_from_slice(&value.to_le_bytes());
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__WriteUint64(this: *mut CxxValueSerializer, value: u64) {
    let st = unsafe { ser_state(this) };
    st.buf.push(TAG_U64);
    st.buf.extend_from_slice(&value.to_le_bytes());
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueSerializer__WriteDouble(this: *mut CxxValueSerializer, value: f64) {
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
    // TODO(qjs): ArrayBuffer transfer unsupported under QuickJS shim. No-op.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ValueDeserializer__TransferSharedArrayBuffer(
    _this: *mut CxxValueDeserializer,
    _transfer_id: u32,
    _array_buffer: Local<SharedArrayBuffer>,
) {
    // TODO(qjs): SharedArrayBuffer transfer unsupported under QuickJS shim. No-op.
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
    let ctx = ctx_of(context.as_non_null().as_ptr() as *const Context);

    // Expect a value record tag.
    match read_u8(&st.buf, &mut st.pos) {
        Some(TAG_VALUE) => {}
        _ => return std::ptr::null(),
    }
    let len = match read_le_u32(&st.buf, &mut st.pos) {
        Some(l) => l as usize,
        None => return std::ptr::null(),
    };
    if st.pos + len > st.buf.len() {
        return std::ptr::null();
    }
    let start = st.pos;
    st.pos += len;

    // JS_ReadObject returns a +1 owned JSValue (or an exception tag on failure).
    let v = unsafe {
        JS_ReadObject(ctx, st.buf.as_ptr().add(start), len, JS_READ_OBJ_REFERENCE)
    };
    if v.tag == JS_TAG_EXCEPTION {
        // Clear any pending exception and report failure.
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return std::ptr::null();
    }
    // Move ownership of the +1 value into the handle arena.
    intern::<Value>(v)
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
    if read_u8(&st.buf, &mut st.pos) != Some(TAG_U64) || st.pos + 8 > st.buf.len() {
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
    if read_u8(&st.buf, &mut st.pos) != Some(TAG_DOUBLE) || st.pos + 8 > st.buf.len() {
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
    // Skip the raw record tag + length prefix we wrote, then expose `length`
    // bytes. We honor the caller's requested `length` (V8 semantics) and only
    // validate that many bytes remain.
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
