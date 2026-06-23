//! QuickJS-backed String shims (family: "string").
//!
//! Construction, inspection, writing, external strings, and PrimitiveArray.
//!
//! QuickJS-ng stores strings internally as either Latin-1 or UTF-16, but its C
//! ABI hands them out as UTF-8 (`JS_ToCStringLen`). For the v8 surface — which
//! is fundamentally UTF-16 (offsets/lengths in code units) — we transcode the
//! UTF-8 bytes to UTF-16 once per call and operate on that. Refcount rules: any
//! `JSValue` a QuickJS function RETURNS is owned (+1) and goes through
//! `intern`/`JS_FreeValue`; borrowed values we want to keep go through
//! `intern_dup`.
#![allow(non_snake_case)]

use super::quickjs_sys::*;
use super::shim_core::{current_ctx, intern, jsval_of};
use crate::isolate::RealIsolate;
use crate::string::{Encoding, ExternalStringResourceBase, OneByteConst};
use crate::support::{char, int, size_t};
use crate::{Primitive, PrimitiveArray, String as V8String};
use std::ffi::c_void;
use std::os::raw::{c_char, c_int};
use std::ptr;

// v8::String::WriteFlags::kNullTerminate — bit set when the caller wants a
// trailing NUL appended if it fits.
const K_NULL_TERMINATE: int = crate::binding::v8_String_WriteFlags_kNullTerminate as int;

/// Context to allocate strings against. Strings are interned into the current
/// handle scope, so the current context is the right root.
#[inline]
fn ctx_for(isolate: *mut RealIsolate) -> *mut JSContext {
    if !isolate.is_null() {
        super::shim_core::iso_state(isolate)
            .contexts
            .last()
            .copied()
            .unwrap_or(super::shim_core::iso_state(isolate).ctx)
    } else {
        current_ctx()
    }
}

/// Read a v8 String handle as UTF-16 code units. Returns an empty vec on any
/// failure (null ctx/handle, conversion error). Frees the transient C string.
#[inline]
fn handle_to_utf16(ctx: *mut JSContext, this: *const V8String) -> Vec<u16> {
    if ctx.is_null() || this.is_null() {
        return Vec::new();
    }
    let v = jsval_of(this);
    let mut len: usize = 0;
    let cstr = unsafe { JS_ToCStringLen(ctx, &mut len, v) };
    if cstr.is_null() {
        return Vec::new();
    }
    let bytes = unsafe { std::slice::from_raw_parts(cstr as *const u8, len) };
    let out: Vec<u16> = std::string::String::from_utf8_lossy(bytes)
        .encode_utf16()
        .collect();
    unsafe { JS_FreeCString(ctx, cstr) };
    out
}

// ===================================================================
// String creation
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__Empty(isolate: *mut RealIsolate) -> *const V8String {
    let ctx = ctx_for(isolate);
    if ctx.is_null() {
        return ptr::null();
    }
    let v = unsafe { JS_NewStringLen(ctx, c"".as_ptr(), 0) };
    if v.tag == JS_TAG_EXCEPTION {
        return ptr::null();
    }
    intern::<V8String>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewFromTwoByte(
    isolate: *mut RealIsolate,
    data: *const u16,
    _new_type: int,
    length: int,
) -> *const V8String {
    let ctx = ctx_for(isolate);
    if ctx.is_null() {
        return ptr::null();
    }
    let len = if length < 0 { 0 } else { length as usize };
    // Transcode UTF-16 -> UTF-8 for QuickJS's UTF-8 ingest path.
    let utf8: std::string::String = if data.is_null() || len == 0 {
        std::string::String::new()
    } else {
        let units = unsafe { std::slice::from_raw_parts(data, len) };
        std::string::String::from_utf16_lossy(units)
    };
    let v = unsafe { JS_NewStringLen(ctx, utf8.as_ptr() as *const c_char, utf8.len()) };
    if v.tag == JS_TAG_EXCEPTION {
        return ptr::null();
    }
    intern::<V8String>(v)
}

// ===================================================================
// External strings.
//
// QuickJS-ng has no externally-backed strings; we materialize the bytes into an
// ordinary JS string so content is preserved (and report non-external in the
// resource-base query below).
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewExternalOneByteConst(
    isolate: *mut RealIsolate,
    onebyte_const: *const OneByteConst,
) -> *const V8String {
    if onebyte_const.is_null() {
        return ptr::null();
    }
    let ctx = ctx_for(isolate);
    if ctx.is_null() {
        return ptr::null();
    }
    // OneByteConst is ASCII/Latin-1; `as_str` yields valid UTF-8.
    let s: &str = unsafe { (*onebyte_const).as_str() };
    let v = unsafe { JS_NewStringLen(ctx, s.as_ptr() as *const c_char, s.len()) };
    if v.tag == JS_TAG_EXCEPTION {
        return ptr::null();
    }
    intern::<V8String>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewExternalOneByteStatic(
    isolate: *mut RealIsolate,
    buffer: *const char,
    length: int,
) -> *const V8String {
    if buffer.is_null() || length < 0 {
        return ptr::null();
    }
    let ctx = ctx_for(isolate);
    if ctx.is_null() {
        return ptr::null();
    }
    // Latin-1 bytes -> UTF-8 so QuickJS stores correct code points.
    let bytes = unsafe { std::slice::from_raw_parts(buffer as *const u8, length as usize) };
    let utf8: std::string::String =
        bytes.iter().map(|&b| b as u8 as ::std::primitive::char).collect();
    let v = unsafe { JS_NewStringLen(ctx, utf8.as_ptr() as *const c_char, utf8.len()) };
    if v.tag == JS_TAG_EXCEPTION {
        return ptr::null();
    }
    intern::<V8String>(v)
}

// TODO(qjs): QuickJS has no externally-backed strings; report none present.
#[unsafe(no_mangle)]
pub extern "C" fn v8__String__GetExternalStringResourceBase(
    _this: *const V8String,
    encoding: *mut Encoding,
) -> *mut ExternalStringResourceBase {
    if !encoding.is_null() {
        unsafe { *encoding = Encoding::Unknown };
    }
    ptr::null_mut()
}

// ===================================================================
// String inspection
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__Utf8Length(
    this: *const V8String,
    isolate: *mut RealIsolate,
) -> int {
    let ctx = ctx_for(isolate);
    if ctx.is_null() || this.is_null() {
        return 0;
    }
    let v = jsval_of(this);
    let mut len: usize = 0;
    // QuickJS hands us UTF-8 directly, so the byte length is the UTF-8 length.
    let cstr = unsafe { JS_ToCStringLen(ctx, &mut len, v) };
    if cstr.is_null() {
        return 0;
    }
    unsafe { JS_FreeCString(ctx, cstr) };
    len as int
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ContainsOnlyOneByte(this: *const V8String) -> bool {
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return true;
    }
    // True iff every UTF-16 code unit fits in Latin-1 (<= 0xFF).
    handle_to_utf16(ctx, this).iter().all(|&c| c <= 0xFF)
}

// ===================================================================
// String writing
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__Write_v2(
    this: *const V8String,
    isolate: *mut RealIsolate,
    offset: u32,
    length: u32,
    buffer: *mut u16,
    flags: int,
) {
    if this.is_null() || buffer.is_null() {
        return;
    }
    let ctx = ctx_for(isolate);
    let units = handle_to_utf16(ctx, this);
    let total = units.len();
    let start = offset as usize;
    let mut n = 0usize;
    if start < total {
        n = (total - start).min(length as usize);
        unsafe { ptr::copy_nonoverlapping(units.as_ptr().add(start), buffer, n) };
    }
    if (flags & K_NULL_TERMINATE) != 0 && (n as u32) < length {
        unsafe { *buffer.add(n) = 0 };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__WriteOneByte_v2(
    this: *const V8String,
    isolate: *mut RealIsolate,
    offset: u32,
    length: u32,
    buffer: *mut u8,
    flags: int,
) {
    if this.is_null() || buffer.is_null() {
        return;
    }
    let ctx = ctx_for(isolate);
    let units = handle_to_utf16(ctx, this);
    let total = units.len();
    let start = offset as usize;
    let mut n = 0usize;
    if start < total {
        n = (total - start).min(length as usize);
        for i in 0..n {
            // Latin-1 truncation, matching v8's WriteOneByte.
            unsafe { *buffer.add(i) = units[start + i] as u8 };
        }
    }
    if (flags & K_NULL_TERMINATE) != 0 && (n as u32) < length {
        unsafe { *buffer.add(n) = 0 };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__WriteUtf8_v2(
    this: *const V8String,
    isolate: *mut RealIsolate,
    buffer: *mut char,
    capacity: size_t,
    flags: int,
    processed_characters_return: *mut size_t,
) -> int {
    let set_processed = |n: size_t| {
        if !processed_characters_return.is_null() {
            unsafe { *processed_characters_return = n };
        }
    };
    if this.is_null() || buffer.is_null() {
        set_processed(0);
        return 0;
    }
    let ctx = ctx_for(isolate);
    if ctx.is_null() {
        set_processed(0);
        return 0;
    }
    let v = jsval_of(this);
    let mut len: usize = 0;
    let cstr = unsafe { JS_ToCStringLen(ctx, &mut len, v) };
    if cstr.is_null() {
        set_processed(0);
        return 0;
    }
    let bytes = unsafe { std::slice::from_raw_parts(cstr as *const u8, len) };

    // Copy as many *whole* UTF-8 code points as fit so we never split a
    // multi-byte sequence — matching v8's WriteUtf8 truncation behavior.
    let cap = capacity;
    let mut copy = bytes.len().min(cap);
    if copy < bytes.len() {
        // Back up to a UTF-8 boundary (bytes 0b10xxxxxx are continuations).
        while copy > 0 && (bytes[copy] & 0xC0) == 0x80 {
            copy -= 1;
        }
    }
    if copy > 0 {
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), buffer as *mut u8, copy) };
    }
    // Count UTF-16 code units actually emitted (v8 reports characters written).
    let chars_written = std::string::String::from_utf8_lossy(&bytes[..copy])
        .encode_utf16()
        .count();

    // NUL-terminate when requested and there's room.
    if (flags & K_NULL_TERMINATE) != 0 && copy < cap {
        unsafe { *(buffer as *mut u8).add(copy) = 0 };
    }
    unsafe { JS_FreeCString(ctx, cstr) };

    set_processed(chars_written as size_t);
    copy as int
}

// ===================================================================
// PrimitiveArray — backed by a JS Array object.
//
// QuickJS has no dedicated PrimitiveArray; v8 only stores primitives in it, and
// a plain Array works for the slots-and-length contract deno_core relies on.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__PrimitiveArray__New(
    isolate: *mut RealIsolate,
    length: int,
) -> *const PrimitiveArray {
    let ctx = ctx_for(isolate);
    if ctx.is_null() {
        return ptr::null();
    }
    let arr = unsafe { JS_NewArray(ctx) };
    if arr.tag == JS_TAG_EXCEPTION {
        return ptr::null();
    }
    // Pre-fill with `undefined` so the array reports the requested length.
    let n = if length < 0 { 0 } else { length as u32 };
    for i in 0..n {
        unsafe { JS_SetPropertyUint32(ctx, arr, i, jsv_undefined()) };
    }
    intern::<PrimitiveArray>(arr)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PrimitiveArray__Length(this: *const PrimitiveArray) -> int {
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return 0;
    }
    let obj = jsval_of(this);
    let len_val = unsafe { JS_GetPropertyStr(ctx, obj, c"length".as_ptr()) };
    if len_val.tag == JS_TAG_EXCEPTION {
        return 0;
    }
    let mut out: i32 = 0;
    unsafe { JS_ToInt32(ctx, &mut out, len_val) };
    unsafe { JS_FreeValue(ctx, len_val) };
    out as int
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PrimitiveArray__Set(
    this: *const PrimitiveArray,
    isolate: *mut RealIsolate,
    index: int,
    item: *const Primitive,
) {
    if this.is_null() || index < 0 {
        return;
    }
    let ctx = ctx_for(isolate);
    if ctx.is_null() {
        return;
    }
    let obj = jsval_of(this);
    // JS_SetPropertyUint32 takes ownership of the value, so dup the borrowed
    // item (or use a fresh undefined) to keep the caller's handle valid.
    let val = if item.is_null() {
        jsv_undefined()
    } else {
        unsafe { JS_DupValue(ctx, jsval_of(item)) }
    };
    unsafe { JS_SetPropertyUint32(ctx, obj, index as u32, val) };
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PrimitiveArray__Get(
    this: *const PrimitiveArray,
    isolate: *mut RealIsolate,
    index: int,
) -> *const Primitive {
    let ctx = ctx_for(isolate);
    if ctx.is_null() {
        return ptr::null();
    }
    if this.is_null() || index < 0 {
        return intern::<Primitive>(jsv_undefined());
    }
    let obj = jsval_of(this);
    // JS_GetPropertyUint32 RETURNS an owned (+1) value — intern it directly.
    let v = unsafe { JS_GetPropertyUint32(ctx, obj, index as u32) };
    if v.tag == JS_TAG_EXCEPTION {
        return intern::<Primitive>(jsv_undefined());
    }
    intern::<Primitive>(v)
}
