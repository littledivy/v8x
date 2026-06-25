//! QuickJS-backed Value shims.
#![allow(non_snake_case)]

use super::quickjs_sys::*;
use super::shim_core::{ctx_of, intern, jsval_of};
use crate::{Context, String as V8String, Value};
use std::ptr;

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToString(
    this: *const Value,
    context: *const Context,
) -> *const V8String {
    let ctx = ctx_of(context);
    if ctx.is_null() || this.is_null() {
        return ptr::null();
    }
    let v = jsval_of(this);
    // JS_ToString returns an owned (+1) string value. QuickJS-ng exposes it as
    // a JS_ToCString round-trip would lose the value; instead build the string
    // via JS_NewStringLen from the C-string form, which is owned by us.
    let mut len: usize = 0;
    let cstr = unsafe { JS_ToCStringLen(ctx, &mut len, v) };
    if cstr.is_null() {
        // Conversion threw; clear and report failure.
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return ptr::null();
    }
    let s = unsafe { JS_NewStringLen(ctx, cstr, len) };
    unsafe { JS_FreeCString(ctx, cstr) };
    if s.tag == JS_TAG_EXCEPTION {
        return ptr::null();
    }
    intern::<V8String>(s)
}
