//! QuickJS-backed String shims: construction and the `ValueView` read path
//! used by `to_rust_string_lossy`.
#![allow(non_snake_case)]

use super::quickjs_sys::*;
use super::shim_core::{current_ctx, intern, jsval_of};
use crate::{RealIsolate, String as V8String};
use std::os::raw::{c_char, c_int};
use std::ptr;

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

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewFromUtf8(
    isolate: *mut RealIsolate,
    data: *const c_char,
    _new_type: c_int,
    length: c_int,
) -> *const V8String {
    let ctx = ctx_for(isolate);
    if ctx.is_null() {
        return ptr::null();
    }
    let len = if length < 0 {
        // v8 treats -1 as "NUL-terminated"; compute the length.
        if data.is_null() {
            0
        } else {
            unsafe { std::ffi::CStr::from_ptr(data) }.to_bytes().len()
        }
    } else {
        length as usize
    };
    let v = unsafe { JS_NewStringLen(ctx, data, len) };
    if v.tag == JS_TAG_EXCEPTION {
        return ptr::null();
    }
    intern::<V8String>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewFromOneByte(
    isolate: *mut RealIsolate,
    data: *const u8,
    _new_type: c_int,
    length: c_int,
) -> *const V8String {
    let ctx = ctx_for(isolate);
    if ctx.is_null() {
        return ptr::null();
    }
    let len = if length < 0 { 0 } else { length as usize };
    // Latin-1 bytes -> UTF-8 so QuickJS stores correct code points.
    let bytes: &[u8] = if data.is_null() || len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(data, len) }
    };
    let utf8: std::string::String = bytes.iter().map(|&b| b as char).collect();
    let v = unsafe { JS_NewStringLen(ctx, utf8.as_ptr() as *const c_char, utf8.len()) };
    if v.tag == JS_TAG_EXCEPTION {
        return ptr::null();
    }
    intern::<V8String>(v)
}

/// Number of UTF-16 code units in the string. No context param, so we use the
/// current thread-local context.
#[unsafe(no_mangle)]
pub extern "C" fn v8__String__Length(this: *const V8String) -> c_int {
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return 0;
    }
    let v = jsval_of(this);
    let mut len: usize = 0;
    let cstr = unsafe { JS_ToCStringLen(ctx, &mut len, v) };
    if cstr.is_null() {
        return 0;
    }
    let bytes = unsafe { std::slice::from_raw_parts(cstr as *const u8, len) };
    let count = std::string::String::from_utf8_lossy(bytes)
        .encode_utf16()
        .count();
    unsafe { JS_FreeCString(ctx, cstr) };
    count as c_int
}

// ===================================================================
// ValueView — the read path for `String::to_rust_string_lossy`.
//
// The vendored `ValueView` is a 32-byte (`v8__String__ValueView_SIZE`) buffer
// we fully own the layout of. We transcode the QuickJS string to UTF-16 once at
// CONSTRUCT time, stash an owned `Box<[u16]>` plus its length in the buffer, and
// report TwoByte so the vendored `wtf16_to_string` reproduces it losslessly.
// DESTRUCT reclaims the box.
// ===================================================================

#[repr(C)]
struct ViewState {
    /// Owned UTF-16 code units (Box raw parts).
    data: *mut u16,
    len: usize,
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ValueView__CONSTRUCT(
    buf: *mut ViewState,
    isolate: *mut RealIsolate,
    string: *const V8String,
) {
    let ctx = ctx_for(isolate);
    let units: Vec<u16> = if ctx.is_null() || string.is_null() {
        Vec::new()
    } else {
        let v = jsval_of(string);
        let mut len: usize = 0;
        let cstr = unsafe { JS_ToCStringLen(ctx, &mut len, v) };
        if cstr.is_null() {
            Vec::new()
        } else {
            let bytes = unsafe { std::slice::from_raw_parts(cstr as *const u8, len) };
            let s = std::string::String::from_utf8_lossy(bytes);
            let out: Vec<u16> = s.encode_utf16().collect();
            unsafe { JS_FreeCString(ctx, cstr) };
            out
        }
    };
    let boxed = units.into_boxed_slice();
    let len = boxed.len();
    let data = Box::into_raw(boxed) as *mut u16;
    unsafe {
        (*buf).data = data;
        (*buf).len = len;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ValueView__DESTRUCT(this: *mut ViewState) {
    unsafe {
        let st = &mut *this;
        if !st.data.is_null() {
            let slice = ptr::slice_from_raw_parts_mut(st.data, st.len);
            drop(Box::from_raw(slice));
            st.data = ptr::null_mut();
            st.len = 0;
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ValueView__is_one_byte(_this: *const ViewState) -> bool {
    // Always report two-byte (UTF-16); see module note.
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ValueView__data(this: *const ViewState) -> *const std::ffi::c_void {
    unsafe { (*this).data as *const std::ffi::c_void }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ValueView__length(this: *const ViewState) -> c_int {
    unsafe { (*this).len as c_int }
}
