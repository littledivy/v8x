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

// JSC C API functions come from `crate::jsc_sys` (bindgen) via the glob import.

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
    // Hand the encoded bytes back to the crate's `release()`. Allocate exactly
    // `len` bytes (global allocator, align 1 — the layout `Vec<u8>` and the
    // crate's Free/ReallocateBufferMemory use) and copy our buffer in, so the
    // Vec the crate reconstructs owns/frees it soundly. `release()` treats `size`
    // as the capacity when its buffer-size atomic is 0 (JSC: the V8 reallocate
    // delegate never runs).
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

// ---- Type-preserving structured-clone encoding -----------------------------
// A recursive, type-tagged format so Date / RegExp / Map / Set / TypedArray /
// ArrayBuffer round-trip (JSON loses all of these). Acyclic: a self-referential
// object is rejected (JSON rejected it too — no regression).
mod tag {
    pub const UNDEF: u8 = 0;
    pub const NULL: u8 = 1;
    pub const TRUE: u8 = 2;
    pub const FALSE: u8 = 3;
    pub const INT: u8 = 4;
    pub const DOUBLE: u8 = 5;
    pub const STRING: u8 = 6;
    pub const DATE: u8 = 7;
    pub const REGEXP: u8 = 8;
    pub const ARRAY: u8 = 9;
    pub const OBJECT: u8 = 10;
    pub const MAP: u8 = 11;
    pub const SET: u8 = 12;
    pub const ARRAYBUFFER: u8 = 13;
    pub const TYPEDARRAY: u8 = 14;
    pub const BIGINT: u8 = 15;
}

unsafe fn global_ctor(ctx: JSContextRef, name: *const c_char) -> JSObjectRef {
    unsafe {
        let g = JSContextGetGlobalObject(ctx);
        let k = JSStringCreateWithUTF8CString(name);
        let mut exc: JSValueRef = std::ptr::null();
        let c = JSObjectGetProperty(ctx, g, k, &mut exc);
        JSStringRelease(k);
        if c.is_null() || !JSValueIsObject(ctx, c) {
            std::ptr::null_mut()
        } else {
            c as JSObjectRef
        }
    }
}

unsafe fn is_instance(ctx: JSContextRef, v: JSValueRef, ctor_name: *const c_char) -> bool {
    unsafe {
        let ctor = global_ctor(ctx, ctor_name);
        if ctor.is_null() {
            return false;
        }
        let mut exc: JSValueRef = std::ptr::null();
        JSValueIsInstanceOfConstructor(ctx, v, ctor, &mut exc)
    }
}

/// `Array.from(v)` — used to read Map/Set contents as a plain array.
unsafe fn array_from(ctx: JSContextRef, v: JSValueRef) -> JSValueRef {
    unsafe {
        let arr_ctor = global_ctor(ctx, c"Array".as_ptr());
        if arr_ctor.is_null() {
            return JSValueMakeUndefined(ctx);
        }
        let k = JSStringCreateWithUTF8CString(c"from".as_ptr());
        let mut exc: JSValueRef = std::ptr::null();
        let from = JSObjectGetProperty(ctx, arr_ctor, k, &mut exc);
        JSStringRelease(k);
        if from.is_null() || !JSValueIsObject(ctx, from) {
            return JSValueMakeUndefined(ctx);
        }
        let args = [v];
        JSObjectCallAsFunction(ctx, from as JSObjectRef, std::ptr::null_mut(), 1, args.as_ptr(), &mut exc)
    }
}

fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}
unsafe fn put_string(ctx: JSContextRef, buf: &mut Vec<u8>, v: JSValueRef) {
    unsafe {
        let s = JSValueToStringCopy(ctx, v, std::ptr::null_mut());
        let bytes = if s.is_null() { Vec::new() } else { jsstring_to_utf8(s) };
        if !s.is_null() {
            JSStringRelease(s);
        }
        put_u32(buf, bytes.len() as u32);
        buf.extend_from_slice(&bytes);
    }
}
unsafe fn get_prop(ctx: JSContextRef, o: JSObjectRef, name: *const c_char) -> JSValueRef {
    unsafe {
        let k = JSStringCreateWithUTF8CString(name);
        let mut exc: JSValueRef = std::ptr::null();
        let v = JSObjectGetProperty(ctx, o, k, &mut exc);
        JSStringRelease(k);
        v
    }
}

/// Encode `v` into `buf`. Returns false on an unsupported / cyclic value.
unsafe fn encode_value(ctx: JSContextRef, v: JSValueRef, buf: &mut Vec<u8>, depth: u32) -> bool {
    if depth > 512 {
        return false; // likely a cycle; bail (JSON did too)
    }
    unsafe {
        let ty = JSValueGetType(ctx, v);
        match ty {
            JSType_kJSTypeUndefined => buf.push(tag::UNDEF),
            JSType_kJSTypeNull => buf.push(tag::NULL),
            JSType_kJSTypeBoolean => {
                buf.push(if JSValueToBoolean(ctx, v) { tag::TRUE } else { tag::FALSE })
            }
            JSType_kJSTypeNumber => {
                let n = JSValueToNumber(ctx, v, std::ptr::null_mut());
                if n.fract() == 0.0 && n >= i32::MIN as f64 && n <= i32::MAX as f64 {
                    buf.push(tag::INT);
                    buf.extend_from_slice(&(n as i32).to_le_bytes());
                } else {
                    buf.push(tag::DOUBLE);
                    buf.extend_from_slice(&n.to_le_bytes());
                }
            }
            JSType_kJSTypeString => {
                buf.push(tag::STRING);
                put_string(ctx, buf, v);
            }
            JSType_kJSTypeSymbol => return false,
            JSType_kJSTypeBigInt => {
                buf.push(tag::BIGINT);
                put_string(ctx, buf, v);
            }
            _ => {
                // Object subtypes.
                let obj = v as JSObjectRef;
                if JSValueIsDate(ctx, v) {
                    buf.push(tag::DATE);
                    let t = JSValueToNumber(ctx, v, std::ptr::null_mut());
                    buf.extend_from_slice(&t.to_le_bytes());
                    return true;
                }
                let ta = JSValueGetTypedArrayType(ctx, v, std::ptr::null_mut());
                if ta == JSTypedArrayType_kJSTypedArrayTypeArrayBuffer {
                    let len = JSObjectGetArrayBufferByteLength(ctx, obj, std::ptr::null_mut());
                    let ptr = JSObjectGetArrayBufferBytesPtr(ctx, obj, std::ptr::null_mut());
                    buf.push(tag::ARRAYBUFFER);
                    put_u32(buf, len as u32);
                    if !ptr.is_null() && len > 0 {
                        buf.extend_from_slice(std::slice::from_raw_parts(ptr as *const u8, len));
                    }
                    return true;
                }
                if ta != JSTypedArrayType_kJSTypedArrayTypeNone {
                    let len = JSObjectGetTypedArrayLength(ctx, obj, std::ptr::null_mut());
                    let blen = JSObjectGetTypedArrayByteLength(ctx, obj, std::ptr::null_mut());
                    let ptr = JSObjectGetTypedArrayBytesPtr(ctx, obj, std::ptr::null_mut());
                    buf.push(tag::TYPEDARRAY);
                    buf.push(ta as u8);
                    put_u32(buf, len as u32);
                    put_u32(buf, blen as u32);
                    if !ptr.is_null() && blen > 0 {
                        buf.extend_from_slice(std::slice::from_raw_parts(ptr as *const u8, blen));
                    }
                    return true;
                }
                if is_instance(ctx, v, c"RegExp".as_ptr()) {
                    buf.push(tag::REGEXP);
                    put_string(ctx, buf, get_prop(ctx, obj, c"source".as_ptr()));
                    put_string(ctx, buf, get_prop(ctx, obj, c"flags".as_ptr()));
                    return true;
                }
                if is_instance(ctx, v, c"Map".as_ptr()) {
                    // entries = Array.from(map) -> [[k,v],...]
                    let entries = array_from(ctx, v);
                    let earr = entries as JSObjectRef;
                    let n = JSValueToNumber(ctx, get_prop(ctx, earr, c"length".as_ptr()), std::ptr::null_mut()) as u32;
                    buf.push(tag::MAP);
                    put_u32(buf, n);
                    for i in 0..n {
                        let pair = JSObjectGetPropertyAtIndex(ctx, earr, i, std::ptr::null_mut()) as JSObjectRef;
                        let kk = JSObjectGetPropertyAtIndex(ctx, pair, 0, std::ptr::null_mut());
                        let vv = JSObjectGetPropertyAtIndex(ctx, pair, 1, std::ptr::null_mut());
                        if !encode_value(ctx, kk, buf, depth + 1) || !encode_value(ctx, vv, buf, depth + 1) {
                            return false;
                        }
                    }
                    return true;
                }
                if is_instance(ctx, v, c"Set".as_ptr()) {
                    let arr = array_from(ctx, v);
                    let sarr = arr as JSObjectRef;
                    let n = JSValueToNumber(ctx, get_prop(ctx, sarr, c"length".as_ptr()), std::ptr::null_mut()) as u32;
                    buf.push(tag::SET);
                    put_u32(buf, n);
                    for i in 0..n {
                        let vv = JSObjectGetPropertyAtIndex(ctx, sarr, i, std::ptr::null_mut());
                        if !encode_value(ctx, vv, buf, depth + 1) {
                            return false;
                        }
                    }
                    return true;
                }
                if JSValueIsArray(ctx, v) {
                    let n = JSValueToNumber(ctx, get_prop(ctx, obj, c"length".as_ptr()), std::ptr::null_mut()) as u32;
                    buf.push(tag::ARRAY);
                    put_u32(buf, n);
                    for i in 0..n {
                        let el = JSObjectGetPropertyAtIndex(ctx, obj, i, std::ptr::null_mut());
                        if !encode_value(ctx, el, buf, depth + 1) {
                            return false;
                        }
                    }
                    return true;
                }
                // Plain object: own enumerable string keys.
                let names = JSObjectCopyPropertyNames(ctx, obj);
                let count = JSPropertyNameArrayGetCount(names);
                buf.push(tag::OBJECT);
                put_u32(buf, count as u32);
                for i in 0..count {
                    let kname = JSPropertyNameArrayGetNameAtIndex(names, i);
                    let kbytes = jsstring_to_utf8(kname);
                    put_u32(buf, kbytes.len() as u32);
                    buf.extend_from_slice(&kbytes);
                    let pv = JSObjectGetProperty(ctx, obj, kname, std::ptr::null_mut());
                    if !encode_value(ctx, pv, buf, depth + 1) {
                        JSPropertyNameArrayRelease(names);
                        return false;
                    }
                }
                JSPropertyNameArrayRelease(names);
            }
        }
        true
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

    let mut rec: Vec<u8> = Vec::new();
    let ok = unsafe { encode_value(ctx, v, &mut rec, 0) };
    if !ok {
        return MaybeBool::JustFalse;
    }
    let len = rec.len() as u32;
    st.buf.extend_from_slice(&len.to_le_bytes());
    st.buf.extend_from_slice(&rec);
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
    let rec = st.buf[st.pos..st.pos + len].to_vec();
    st.pos += len;
    let mut p = 0usize;
    let v = unsafe { decode_value(ctx, &rec, &mut p) };
    if v.is_null() {
        return std::ptr::null();
    }
    intern_ctx::<Value>(ctx, v)
}

fn rd_u8(b: &[u8], p: &mut usize) -> Option<u8> {
    if *p < b.len() {
        let v = b[*p];
        *p += 1;
        Some(v)
    } else {
        None
    }
}
fn rd_u32(b: &[u8], p: &mut usize) -> Option<u32> {
    if *p + 4 <= b.len() {
        let v = u32::from_le_bytes(b[*p..*p + 4].try_into().ok()?);
        *p += 4;
        Some(v)
    } else {
        None
    }
}
fn rd_bytes<'a>(b: &'a [u8], p: &mut usize, n: usize) -> Option<&'a [u8]> {
    if *p + n <= b.len() {
        let s = &b[*p..*p + n];
        *p += n;
        Some(s)
    } else {
        None
    }
}
unsafe fn rd_jsstring(ctx: JSContextRef, b: &[u8], p: &mut usize) -> JSValueRef {
    let n = match rd_u32(b, p) {
        Some(n) => n as usize,
        None => return unsafe { JSValueMakeUndefined(ctx) },
    };
    let bytes = match rd_bytes(b, p, n) {
        Some(s) => s,
        None => return unsafe { JSValueMakeUndefined(ctx) },
    };
    let mut c = bytes.to_vec();
    c.push(0);
    unsafe {
        let s = JSStringCreateWithUTF8CString(c.as_ptr() as *const c_char);
        let v = JSValueMakeString(ctx, s);
        JSStringRelease(s);
        v
    }
}

/// Decode one value written by `encode_value`. Returns null on malformed input.
unsafe fn decode_value(ctx: JSContextRef, b: &[u8], p: &mut usize) -> JSValueRef {
    unsafe {
        let t = match rd_u8(b, p) {
            Some(t) => t,
            None => return std::ptr::null(),
        };
        match t {
            tag::UNDEF => JSValueMakeUndefined(ctx),
            tag::NULL => JSValueMakeNull(ctx),
            tag::TRUE => JSValueMakeBoolean(ctx, true),
            tag::FALSE => JSValueMakeBoolean(ctx, false),
            tag::INT => {
                let v = rd_bytes(b, p, 4)
                    .and_then(|s| s.try_into().ok())
                    .map(i32::from_le_bytes)
                    .unwrap_or(0);
                JSValueMakeNumber(ctx, v as f64)
            }
            tag::DOUBLE => {
                let v = rd_bytes(b, p, 8)
                    .and_then(|s| s.try_into().ok())
                    .map(f64::from_le_bytes)
                    .unwrap_or(0.0);
                JSValueMakeNumber(ctx, v)
            }
            tag::STRING => rd_jsstring(ctx, b, p),
            tag::BIGINT => {
                // `JSBigIntCreateWithString` isn't exported by the system JSC
                // framework, so reconstruct via the global `BigInt(str)`.
                let s = rd_jsstring(ctx, b, p);
                let bigint = global_ctor(ctx, c"BigInt".as_ptr());
                if bigint.is_null() {
                    return JSValueMakeUndefined(ctx);
                }
                let mut exc: JSValueRef = std::ptr::null();
                let args = [s];
                let v = JSObjectCallAsFunction(ctx, bigint, std::ptr::null_mut(), 1, args.as_ptr(), &mut exc);
                if v.is_null() { JSValueMakeUndefined(ctx) } else { v }
            }
            tag::DATE => {
                let ms = rd_bytes(b, p, 8)
                    .and_then(|s| s.try_into().ok())
                    .map(f64::from_le_bytes)
                    .unwrap_or(0.0);
                let arg = [JSValueMakeNumber(ctx, ms)];
                let mut exc: JSValueRef = std::ptr::null();
                JSObjectMakeDate(ctx, 1, arg.as_ptr(), &mut exc) as JSValueRef
            }
            tag::REGEXP => {
                let src = rd_jsstring(ctx, b, p);
                let flags = rd_jsstring(ctx, b, p);
                let args = [src, flags];
                let mut exc: JSValueRef = std::ptr::null();
                JSObjectMakeRegExp(ctx, 2, args.as_ptr(), &mut exc) as JSValueRef
            }
            tag::ARRAY => {
                let n = rd_u32(b, p).unwrap_or(0);
                let mut exc: JSValueRef = std::ptr::null();
                let arr = JSObjectMakeArray(ctx, 0, std::ptr::null(), &mut exc);
                for i in 0..n {
                    let el = decode_value(ctx, b, p);
                    if el.is_null() {
                        return std::ptr::null();
                    }
                    JSObjectSetPropertyAtIndex(ctx, arr, i, el, &mut exc);
                }
                arr as JSValueRef
            }
            tag::OBJECT => {
                let n = rd_u32(b, p).unwrap_or(0);
                let obj = JSObjectMake(ctx, std::ptr::null_mut(), std::ptr::null_mut());
                for _ in 0..n {
                    let klen = rd_u32(b, p).unwrap_or(0) as usize;
                    let kb = rd_bytes(b, p, klen).unwrap_or(&[]);
                    let mut kc = kb.to_vec();
                    kc.push(0);
                    let key = JSStringCreateWithUTF8CString(kc.as_ptr() as *const c_char);
                    let val = decode_value(ctx, b, p);
                    if val.is_null() {
                        JSStringRelease(key);
                        return std::ptr::null();
                    }
                    let mut exc: JSValueRef = std::ptr::null();
                    JSObjectSetProperty(ctx, obj, key, val, 0, &mut exc);
                    JSStringRelease(key);
                }
                obj as JSValueRef
            }
            tag::MAP => {
                let n = rd_u32(b, p).unwrap_or(0);
                // entries array of [k,v]
                let mut exc: JSValueRef = std::ptr::null();
                let entries = JSObjectMakeArray(ctx, 0, std::ptr::null(), &mut exc);
                for i in 0..n {
                    let k = decode_value(ctx, b, p);
                    let v = decode_value(ctx, b, p);
                    if k.is_null() || v.is_null() {
                        return std::ptr::null();
                    }
                    let pair_items = [k, v];
                    let pair = JSObjectMakeArray(ctx, 2, pair_items.as_ptr(), &mut exc);
                    JSObjectSetPropertyAtIndex(ctx, entries, i, pair as JSValueRef, &mut exc);
                }
                let ctor = global_ctor(ctx, c"Map".as_ptr());
                let args = [entries as JSValueRef];
                JSObjectCallAsConstructor(ctx, ctor, 1, args.as_ptr(), &mut exc) as JSValueRef
            }
            tag::SET => {
                let n = rd_u32(b, p).unwrap_or(0);
                let mut exc: JSValueRef = std::ptr::null();
                let arr = JSObjectMakeArray(ctx, 0, std::ptr::null(), &mut exc);
                for i in 0..n {
                    let v = decode_value(ctx, b, p);
                    if v.is_null() {
                        return std::ptr::null();
                    }
                    JSObjectSetPropertyAtIndex(ctx, arr, i, v, &mut exc);
                }
                let ctor = global_ctor(ctx, c"Set".as_ptr());
                let args = [arr as JSValueRef];
                JSObjectCallAsConstructor(ctx, ctor, 1, args.as_ptr(), &mut exc) as JSValueRef
            }
            tag::ARRAYBUFFER => {
                let n = rd_u32(b, p).unwrap_or(0) as usize;
                let data = rd_bytes(b, p, n).unwrap_or(&[]).to_vec();
                // Build a Uint8Array(n), fill, return its .buffer.
                let mut exc: JSValueRef = std::ptr::null();
                let u8a = JSObjectMakeTypedArray(ctx, JSTypedArrayType_kJSTypedArrayTypeUint8Array, n, &mut exc);
                let ptr = JSObjectGetTypedArrayBytesPtr(ctx, u8a, &mut exc);
                if !ptr.is_null() && n > 0 {
                    std::ptr::copy_nonoverlapping(data.as_ptr(), ptr as *mut u8, n);
                }
                JSObjectGetTypedArrayBuffer(ctx, u8a, &mut exc) as JSValueRef
            }
            tag::TYPEDARRAY => {
                let ta_ty = rd_u8(b, p).unwrap_or(0) as JSTypedArrayType;
                let len = rd_u32(b, p).unwrap_or(0) as usize;
                let blen = rd_u32(b, p).unwrap_or(0) as usize;
                let data = rd_bytes(b, p, blen).unwrap_or(&[]).to_vec();
                let mut exc: JSValueRef = std::ptr::null();
                let arr = JSObjectMakeTypedArray(ctx, ta_ty, len, &mut exc);
                let ptr = JSObjectGetTypedArrayBytesPtr(ctx, arr, &mut exc);
                if !ptr.is_null() && blen > 0 {
                    std::ptr::copy_nonoverlapping(data.as_ptr(), ptr as *mut u8, blen);
                }
                arr as JSValueRef
            }
            _ => std::ptr::null(),
        }
    }
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
