// Family: "string" — v8::String creation/inspection + PrimitiveArray.
#![allow(non_snake_case, unused)]

use crate::jsc::jsc_sys::*;
use crate::isolate::RealIsolate;
use crate::string::{Encoding, NewStringType};
use crate::support::{char, int, size_t};
use crate::{Primitive, PrimitiveArray, String as V8String, Value};
use crate::jsc::shim_core::{ctx_of, current_ctx, current_iso, intern, intern_ctx, iso_state, jsval};
use std::os::raw::c_void;
use std::ptr;

// JSC C API functions come from `crate::jsc::jsc_sys` (bindgen) via the glob import.

// ===================================================================
// String creation
// ===================================================================

#[inline]
unsafe fn make_string_from_utf16(ctx: JSContextRef, data: *const u16, len: usize) -> JSValueRef {
    unsafe {
        let s = JSStringCreateWithCharacters(data, len);
        let v = JSValueMakeString(ctx, s);
        JSStringRelease(s);
        v
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__Empty(isolate: *mut RealIsolate) -> *const V8String {
    let ctx = current_ctx();
    unsafe {
        let s = JSStringCreateWithUTF8CString(c"".as_ptr());
        let v = JSValueMakeString(ctx, s);
        JSStringRelease(s);
        intern_ctx::<V8String>(ctx, v)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewFromUtf8(
    isolate: *mut RealIsolate,
    data: *const char,
    _new_type: NewStringType,
    length: int,
) -> *const V8String {
    let ctx = current_ctx();
    if length < 0 || data.is_null() {
        return ptr::null();
    }
    let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, length as usize) };
    // V8's `NewFromUtf8` (Normal type) does LOSSY decoding — invalid sequences
    // become U+FFFD — and copies the full length (embedded NULs included).
    // `JSStringCreateWithUTF8CString` does neither (it rejects invalid UTF-8 and
    // stops at the first NUL), which made `TextDecoder.decode` of any invalid or
    // NUL-containing bytes return "". Decode lossily and build via UTF-16.
    let cow = std::string::String::from_utf8_lossy(bytes);
    let utf16: Vec<u16> = cow.encode_utf16().collect();
    unsafe {
        let v = make_string_from_utf16(ctx, utf16.as_ptr(), utf16.len());
        intern_ctx::<V8String>(ctx, v)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewFromOneByte(
    isolate: *mut RealIsolate,
    data: *const u8,
    _new_type: NewStringType,
    length: int,
) -> *const V8String {
    let ctx = current_ctx();
    if length < 0 || data.is_null() {
        return ptr::null();
    }
    // Latin-1 -> UTF-16 (each byte is a code point).
    let bytes = unsafe { std::slice::from_raw_parts(data, length as usize) };
    let utf16: Vec<u16> = bytes.iter().map(|&b| b as u16).collect();
    unsafe {
        let v = make_string_from_utf16(ctx, utf16.as_ptr(), utf16.len());
        intern_ctx::<V8String>(ctx, v)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewFromTwoByte(
    isolate: *mut RealIsolate,
    data: *const u16,
    _new_type: NewStringType,
    length: int,
) -> *const V8String {
    let ctx = current_ctx();
    if length < 0 || data.is_null() {
        return ptr::null();
    }
    unsafe {
        let v = make_string_from_utf16(ctx, data, length as usize);
        intern_ctx::<V8String>(ctx, v)
    }
}

// ===================================================================
// String inspection
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__IsOneByte(this: *const V8String) -> bool {
    // True if every code unit fits in Latin-1 (<= 0xFF). JSC strings are UTF-16
    // internally; scan the code units.
    if this.is_null() {
        return true;
    }
    let ctx = current_ctx();
    if ctx.is_null() {
        return true;
    }
    unsafe {
        let s = JSValueToStringCopy(ctx, jsval(this), ptr::null_mut());
        if s.is_null() {
            return true;
        }
        let len = JSStringGetLength(s);
        let chars = JSStringGetCharactersPtr(s);
        let mut one_byte = true;
        if !chars.is_null() {
            for i in 0..len {
                if *chars.add(i) > 0xFF {
                    one_byte = false;
                    break;
                }
            }
        }
        JSStringRelease(s);
        one_byte
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__Length(this: *const V8String) -> int {
    if this.is_null() {
        return 0;
    }
    let ctx = current_ctx();
    unsafe {
        let s = JSValueToStringCopy(ctx, jsval(this), ptr::null_mut());
        if s.is_null() {
            return 0;
        }
        let len = JSStringGetLength(s);
        JSStringRelease(s);
        len as int
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__Utf8Length(
    this: *const V8String,
    isolate: *mut RealIsolate,
) -> int {
    if this.is_null() {
        return 0;
    }
    let ctx = current_ctx();
    unsafe {
        let s = JSValueToStringCopy(ctx, jsval(this), ptr::null_mut());
        if s.is_null() {
            return 0;
        }
        // JSStringGetMaximumUTF8CStringSize includes the trailing NUL; the real
        // UTF-8 byte length is obtained by an actual conversion.
        let cap = JSStringGetMaximumUTF8CStringSize(s);
        let mut buf: Vec<u8> = vec![0; cap];
        let written = JSStringGetUTF8CString(s, buf.as_mut_ptr() as *mut char, cap);
        JSStringRelease(s);
        // `written` includes the trailing NUL byte; subtract it.
        (written.saturating_sub(1)) as int
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ContainsOnlyOneByte(this: *const V8String) -> bool {
    if this.is_null() {
        return true;
    }
    let ctx = current_ctx();
    unsafe {
        let s = JSValueToStringCopy(ctx, jsval(this), ptr::null_mut());
        if s.is_null() {
            return true;
        }
        let len = JSStringGetLength(s);
        let chars = JSStringGetCharactersPtr(s);
        let mut only_one_byte = true;
        if !chars.is_null() {
            let slice = std::slice::from_raw_parts(chars, len);
            only_one_byte = slice.iter().all(|&c| c <= 0xFF);
        }
        JSStringRelease(s);
        only_one_byte
    }
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
    let null_terminate = crate::binding::v8_String_WriteFlags_kNullTerminate as int;
    let ctx = current_ctx();
    unsafe {
        let s = JSValueToStringCopy(ctx, jsval(this), ptr::null_mut());
        if s.is_null() {
            return;
        }
        let total = JSStringGetLength(s);
        let chars = JSStringGetCharactersPtr(s);
        let start = offset as usize;
        let mut n = 0usize;
        if !chars.is_null() && start < total {
            let avail = total - start;
            n = avail.min(length as usize);
            ptr::copy_nonoverlapping(chars.add(start), buffer, n);
        }
        JSStringRelease(s);
        if (flags & null_terminate) != 0 && (n as u32) < length {
            *buffer.add(n) = 0;
        }
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
    let null_terminate = crate::binding::v8_String_WriteFlags_kNullTerminate as int;
    let ctx = current_ctx();
    unsafe {
        let s = JSValueToStringCopy(ctx, jsval(this), ptr::null_mut());
        if s.is_null() {
            return;
        }
        let total = JSStringGetLength(s);
        let chars = JSStringGetCharactersPtr(s);
        let start = offset as usize;
        let mut n = 0usize;
        if !chars.is_null() && start < total {
            let avail = total - start;
            n = avail.min(length as usize);
            for i in 0..n {
                *buffer.add(i) = (*chars.add(start + i)) as u8;
            }
        }
        JSStringRelease(s);
        if (flags & null_terminate) != 0 && (n as u32) < length {
            *buffer.add(n) = 0;
        }
    }
}

/// Given a complete UTF-8 byte slice, return `(byte_count, utf16_units)` for the
/// longest prefix made of WHOLE code points that fits within `cap` bytes. A
/// 4-byte (astral) sequence counts as 2 UTF-16 code units.
fn utf8_complete_prefix(s: &[u8], cap: usize) -> (usize, size_t) {
    let mut i = 0usize;
    let mut units: size_t = 0;
    while i < s.len() {
        let b = s[i];
        let seq = if b < 0x80 {
            1
        } else if b < 0xE0 {
            2
        } else if b < 0xF0 {
            3
        } else {
            4
        };
        if i + seq > cap {
            break;
        }
        units += if seq == 4 { 2 } else { 1 };
        i += seq;
    }
    (i, units)
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
    if this.is_null() || buffer.is_null() {
        if !processed_characters_return.is_null() {
            unsafe { *processed_characters_return = 0 };
        }
        return 0;
    }
    let ctx = current_ctx();
    unsafe {
        let s = JSValueToStringCopy(ctx, jsval(this), ptr::null_mut());
        if s.is_null() {
            if !processed_characters_return.is_null() {
                *processed_characters_return = 0;
            }
            return 0;
        }
        let char_count = JSStringGetLength(s);
        // Convert the whole string to UTF-8 into a scratch buffer, then copy as
        // many full bytes as fit into the caller's buffer.
        let max = JSStringGetMaximumUTF8CStringSize(s);
        let mut scratch: Vec<u8> = vec![0; max];
        let written = JSStringGetUTF8CString(s, scratch.as_mut_ptr() as *mut char, max);
        JSStringRelease(s);
        // `written` counts the trailing NUL.
        let utf8_len = written.saturating_sub(1);

        // Copy whole UTF-8 sequences only. If the destination can't hold the full
        // encoding we must NOT truncate mid-character: V8's `WriteUtf8` writes
        // only complete code points and reports `processed_characters` as the
        // number of UTF-16 code units it fully encoded. Reporting a byte count (or
        // counting a half-written char) makes deno's TextEncoder think the whole
        // string was consumed and skip the retry-with-bigger-buffer, corrupting
        // any multi-byte text.
        let (copy, units) = if utf8_len <= capacity {
            (utf8_len, char_count)
        } else {
            utf8_complete_prefix(&scratch[..utf8_len], capacity)
        };
        if copy > 0 {
            ptr::copy_nonoverlapping(scratch.as_ptr(), buffer as *mut u8, copy);
        }
        // NUL-terminate if there is room (v8 always NUL-terminates when it fits).
        if copy < capacity {
            *(buffer as *mut u8).add(copy) = 0;
        }
        if !processed_characters_return.is_null() {
            *processed_characters_return = units;
        }
        copy as int
    }
}

// ===================================================================
// External strings — JSC does not expose externally-backed strings.
// We materialize them as ordinary JS strings so content is preserved,
// but report them as non-external.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewExternalOneByteConst(
    isolate: *mut RealIsolate,
    onebyte_const: *const crate::string::OneByteConst,
) -> *const V8String {
    if onebyte_const.is_null() {
        return ptr::null();
    }
    let ctx = current_ctx();
    unsafe {
        let s: &str = (*onebyte_const).as_str();
        let bytes = s.as_bytes();
        let utf16: Vec<u16> = bytes.iter().map(|&b| b as u16).collect();
        let v = make_string_from_utf16(ctx, utf16.as_ptr(), utf16.len());
        intern_ctx::<V8String>(ctx, v)
    }
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
    let ctx = current_ctx();
    unsafe {
        let bytes = std::slice::from_raw_parts(buffer as *const u8, length as usize);
        let utf16: Vec<u16> = bytes.iter().map(|&b| b as u16).collect();
        let v = make_string_from_utf16(ctx, utf16.as_ptr(), utf16.len());
        intern_ctx::<V8String>(ctx, v)
    }
}

// TODO(v82jsc): JSC has no externally-backed strings; report none present.
#[unsafe(no_mangle)]
pub extern "C" fn v8__String__GetExternalStringResourceBase(
    this: *const V8String,
    encoding: *mut Encoding,
) -> *mut crate::string::ExternalStringResourceBase {
    if !encoding.is_null() {
        unsafe { *encoding = Encoding::Unknown };
    }
    ptr::null_mut()
}

// ===================================================================
// String::ValueView
// ===================================================================
//
// We back the opaque [u8; SIZE] buffer with our own small repr that owns a
// retained JSStringRef plus a cached data pointer / length / encoding flag.

// NOTE: This must fit in `v8__String__ValueView_SIZE` (32 bytes). Four 8-byte
// fields exactly fill it; the `is_one_byte` flag is packed into the high bit of
// `length` (string lengths never approach 2^63).
#[repr(C)]
struct ValueViewRepr {
    js_string: JSStringRef, // retained; released on DESTRUCT
    data: *const c_void,    // -> u16 for two-byte, -> u8 (latin1) for one-byte
    /// Low 63 bits: length. High bit: is_one_byte.
    length_and_flag: usize,
    // Owned latin1 byte buffer for the one-byte case (kept alive via Box).
    onebyte_owned: *mut u8,
}

const ONE_BYTE_FLAG: usize = 1usize << 63;

impl ValueViewRepr {
    #[inline]
    fn pack(len: usize, is_one_byte: bool) -> usize {
        (len & !ONE_BYTE_FLAG) | if is_one_byte { ONE_BYTE_FLAG } else { 0 }
    }
    #[inline]
    fn length(&self) -> usize {
        self.length_and_flag & !ONE_BYTE_FLAG
    }
    #[inline]
    fn is_one_byte(&self) -> bool {
        self.length_and_flag & ONE_BYTE_FLAG != 0
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ValueView__CONSTRUCT(
    buf: *mut crate::string::ValueView,
    isolate: *mut RealIsolate,
    string: *const V8String,
) {
    let repr = buf as *mut ValueViewRepr;
    let ctx = current_ctx();
    unsafe {
        if string.is_null() {
            (*repr) = ValueViewRepr {
                js_string: ptr::null_mut(),
                data: ptr::null(),
                length_and_flag: ValueViewRepr::pack(0, true),
                onebyte_owned: ptr::null_mut(),
            };
            return;
        }
        let s = JSValueToStringCopy(ctx, jsval(string), ptr::null_mut());
        if s.is_null() {
            (*repr) = ValueViewRepr {
                js_string: ptr::null_mut(),
                data: ptr::null(),
                length_and_flag: ValueViewRepr::pack(0, true),
                onebyte_owned: ptr::null_mut(),
            };
            return;
        }
        let len = JSStringGetLength(s);
        let chars = JSStringGetCharactersPtr(s);
        let only_one_byte = if chars.is_null() {
            true
        } else {
            std::slice::from_raw_parts(chars, len).iter().all(|&c| c <= 0xFF)
        };

        if only_one_byte {
            // Materialize a latin1 byte buffer we own.
            let mut owned: Vec<u8> = Vec::with_capacity(len);
            if !chars.is_null() {
                for i in 0..len {
                    owned.push((*chars.add(i)) as u8);
                }
            } else {
                owned.resize(len, 0);
            }
            let boxed = owned.into_boxed_slice();
            let ptr = Box::into_raw(boxed) as *mut u8;
            (*repr) = ValueViewRepr {
                js_string: s, // keep retained too (released on destruct)
                data: ptr as *const c_void,
                length_and_flag: ValueViewRepr::pack(len, true),
                onebyte_owned: ptr,
            };
        } else {
            // Two-byte: the JSStringRef owns the characters; keep it alive.
            (*repr) = ValueViewRepr {
                js_string: s,
                data: chars as *const c_void,
                length_and_flag: ValueViewRepr::pack(len, false),
                onebyte_owned: ptr::null_mut(),
            };
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ValueView__DESTRUCT(this: *mut crate::string::ValueView) {
    let repr = this as *mut ValueViewRepr;
    unsafe {
        if !(*repr).onebyte_owned.is_null() {
            let len = (*repr).length();
            let slice = ptr::slice_from_raw_parts_mut((*repr).onebyte_owned, len);
            drop(Box::from_raw(slice));
            (*repr).onebyte_owned = ptr::null_mut();
        }
        if !(*repr).js_string.is_null() {
            JSStringRelease((*repr).js_string);
            (*repr).js_string = ptr::null_mut();
        }
        (*repr).data = ptr::null();
        (*repr).length_and_flag = 0;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ValueView__is_one_byte(
    this: *const crate::string::ValueView,
) -> bool {
    let repr = this as *const ValueViewRepr;
    unsafe { (*repr).is_one_byte() }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ValueView__data(
    this: *const crate::string::ValueView,
) -> *const c_void {
    let repr = this as *const ValueViewRepr;
    unsafe { (*repr).data }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ValueView__length(
    this: *const crate::string::ValueView,
) -> int {
    let repr = this as *const ValueViewRepr;
    unsafe { (*repr).length() as int }
}

// ===================================================================
// PrimitiveArray — backed by a JS Array object.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__PrimitiveArray__New(
    isolate: *mut RealIsolate,
    length: int,
) -> *const PrimitiveArray {
    let ctx = current_ctx();
    let n = if length < 0 { 0 } else { length as usize };
    unsafe {
        // Pre-fill with `undefined` so the array has the requested length.
        let undef = JSValueMakeUndefined(ctx);
        let args: Vec<JSValueRef> = vec![undef; n];
        let arr = JSObjectMakeArray(
            ctx,
            n,
            if n == 0 { ptr::null() } else { args.as_ptr() },
            ptr::null_mut(),
        );
        intern_ctx::<PrimitiveArray>(ctx, arr as JSValueRef)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PrimitiveArray__Length(this: *const PrimitiveArray) -> int {
    if this.is_null() {
        return 0;
    }
    let ctx = current_ctx();
    unsafe {
        let obj = jsval(this) as JSObjectRef;
        let name = JSStringCreateWithUTF8CString(c"length".as_ptr());
        let v = JSObjectGetProperty(ctx, obj, name, ptr::null_mut());
        JSStringRelease(name);
        let n = JSValueToNumber(ctx, v, ptr::null_mut());
        if n.is_nan() { 0 } else { n as int }
    }
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
    let ctx = current_ctx();
    unsafe {
        let obj = jsval(this) as JSObjectRef;
        let val = if item.is_null() {
            JSValueMakeUndefined(ctx)
        } else {
            jsval(item)
        };
        JSObjectSetPropertyAtIndex(ctx, obj, index as u32, val, ptr::null_mut());
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PrimitiveArray__Get(
    this: *const PrimitiveArray,
    isolate: *mut RealIsolate,
    index: int,
) -> *const Primitive {
    let ctx = current_ctx();
    if this.is_null() || index < 0 {
        return intern_ctx::<Primitive>(ctx, unsafe { JSValueMakeUndefined(ctx) });
    }
    unsafe {
        let obj = jsval(this) as JSObjectRef;
        let v = JSObjectGetPropertyAtIndex(ctx, obj, index as u32, ptr::null_mut());
        intern_ctx::<Primitive>(ctx, v)
    }
}

// ===================================================================
// External strings — V8 keeps the embedder's buffer alive and calls `free`
// when the string dies. JSC has no external-string C API, so we materialize a
// copy and immediately invoke `free` (we no longer reference the buffer).
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewExternalOneByte(
    isolate: *mut RealIsolate,
    buffer: *mut char,
    length: size_t,
    free: unsafe extern "C" fn(*mut char, size_t),
) -> *const V8String {
    let ctx = current_ctx();
    if buffer.is_null() {
        return ptr::null();
    }
    let bytes = unsafe { std::slice::from_raw_parts(buffer as *const u8, length as usize) };
    let utf16: Vec<u16> = bytes.iter().map(|&b| b as u16).collect();
    let v = unsafe { make_string_from_utf16(ctx, utf16.as_ptr(), utf16.len()) };
    let out = intern_ctx::<V8String>(ctx, v);
    // We copied; relinquish the embedder buffer.
    unsafe { free(buffer, length) };
    out
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewExternalTwoByte(
    isolate: *mut RealIsolate,
    buffer: *mut u16,
    length: size_t,
    free: unsafe extern "C" fn(*mut u16, size_t),
) -> *const V8String {
    let ctx = current_ctx();
    if buffer.is_null() {
        return ptr::null();
    }
    let v = unsafe { make_string_from_utf16(ctx, buffer, length as usize) };
    let out = intern_ctx::<V8String>(ctx, v);
    unsafe { free(buffer, length) };
    out
}
