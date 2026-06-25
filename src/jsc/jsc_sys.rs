//! Raw FFI bindings to Apple's JavaScriptCore C API.
//!
//! These are generated at build time by `bindgen` over the SDK's
//! `<JavaScriptCore/JavaScript.h>` umbrella header (see `generate_jsc_bindings`
//! in `build.rs`). The generated names match the C API exactly
//! (`JSValueRef`, `JSContextRef`, `JSEvaluateScript`, ...), so the shim code is
//! unaffected. The full JSC C API is available here, so the backend no longer
//! needs scattered hand-written `extern "C"` blocks.
#![allow(non_camel_case_types)]
#![allow(non_upper_case_globals)]
#![allow(non_snake_case)]
#![allow(dead_code)]
#![allow(clippy::all)]

include!(concat!(env!("OUT_DIR"), "/jsc_bindings.rs"));

// ---------------------------------------------------------------------------
// Compatibility aliases.
//
// bindgen emits the `JSType` / `JSTypedArrayType` C enums as integer typedefs
// with prefixed constants (`JSType_kJSTypeNumber`, ...). The shim code refers
// to the unprefixed C spelling (`kJSTypeNumber`), so re-export those names.
// ---------------------------------------------------------------------------
pub const kJSTypeUndefined: JSType = JSType_kJSTypeUndefined;
pub const kJSTypeNull: JSType = JSType_kJSTypeNull;
pub const kJSTypeBoolean: JSType = JSType_kJSTypeBoolean;
pub const kJSTypeNumber: JSType = JSType_kJSTypeNumber;
pub const kJSTypeString: JSType = JSType_kJSTypeString;
pub const kJSTypeObject: JSType = JSType_kJSTypeObject;
pub const kJSTypeSymbol: JSType = JSType_kJSTypeSymbol;
pub const kJSTypeBigInt: JSType = JSType_kJSTypeBigInt;

pub const kJSTypedArrayTypeInt8Array: JSTypedArrayType =
    JSTypedArrayType_kJSTypedArrayTypeInt8Array;
pub const kJSTypedArrayTypeInt16Array: JSTypedArrayType =
    JSTypedArrayType_kJSTypedArrayTypeInt16Array;
pub const kJSTypedArrayTypeInt32Array: JSTypedArrayType =
    JSTypedArrayType_kJSTypedArrayTypeInt32Array;
pub const kJSTypedArrayTypeUint8Array: JSTypedArrayType =
    JSTypedArrayType_kJSTypedArrayTypeUint8Array;
pub const kJSTypedArrayTypeUint8ClampedArray: JSTypedArrayType =
    JSTypedArrayType_kJSTypedArrayTypeUint8ClampedArray;
pub const kJSTypedArrayTypeUint16Array: JSTypedArrayType =
    JSTypedArrayType_kJSTypedArrayTypeUint16Array;
pub const kJSTypedArrayTypeUint32Array: JSTypedArrayType =
    JSTypedArrayType_kJSTypedArrayTypeUint32Array;
pub const kJSTypedArrayTypeFloat32Array: JSTypedArrayType =
    JSTypedArrayType_kJSTypedArrayTypeFloat32Array;
pub const kJSTypedArrayTypeFloat64Array: JSTypedArrayType =
    JSTypedArrayType_kJSTypedArrayTypeFloat64Array;
pub const kJSTypedArrayTypeArrayBuffer: JSTypedArrayType =
    JSTypedArrayType_kJSTypedArrayTypeArrayBuffer;
pub const kJSTypedArrayTypeNone: JSTypedArrayType =
    JSTypedArrayType_kJSTypedArrayTypeNone;
pub const kJSTypedArrayTypeBigInt64Array: JSTypedArrayType =
    JSTypedArrayType_kJSTypedArrayTypeBigInt64Array;
pub const kJSTypedArrayTypeBigUint64Array: JSTypedArrayType =
    JSTypedArrayType_kJSTypedArrayTypeBigUint64Array;

#[cfg(test)]
mod jsc_eval_smoke {
    use super::*;
    use std::ffi::CString;
    use std::ptr;

    /// Prove the linked JSC (system OR vendored framework, depending on the
    /// vendor_jsc feature) actually evaluates JS through our FFI.
    #[test]
    fn eval_one_plus_one() {
        unsafe {
            let group = JSContextGroupCreate();
            let ctx = JSGlobalContextCreateInGroup(group, ptr::null_mut());
            let src = CString::new("1 + 1").unwrap();
            let js = JSStringCreateWithUTF8CString(src.as_ptr());
            let mut exc: JSValueRef = ptr::null();
            let r = JSEvaluateScript(ctx, js, ptr::null_mut(), ptr::null_mut(), 0, &mut exc);
            JSStringRelease(js);
            assert!(exc.is_null());
            assert_eq!(JSValueToNumber(ctx, r, ptr::null_mut()), 2.0);
            JSGlobalContextRelease(ctx);
            JSContextGroupRelease(group);
        }
    }
}
