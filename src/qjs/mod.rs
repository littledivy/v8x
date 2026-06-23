//! QuickJS-ng engine backend for the v8 C-ABI compat layer.
//!
//! Mirrors the JSC backend's structure: `quickjs_sys` holds the raw QuickJS-ng
//! FFI (adapted from denoland/deno#34033's qjs_v8_compat), `shims` holds the
//! `v8__*` C-ABI definitions (currently auto-generated stubs; real
//! implementations land incrementally, same pattern as the JSC backend).

pub(crate) mod quickjs_sys;
mod shims;

#[cfg(test)]
mod raw_smoke_test {
    use super::quickjs_sys::*;
    use std::ffi::CString;
    use std::ptr;

    /// Lowest-level proof: compile+link QuickJS-ng, eval `1+1`, read back `2.0`.
    /// Only runs when actually linked against QuickJS.
    #[test]
    #[cfg(feature = "link_quickjs")]
    fn eval_one_plus_one_raw() {
        unsafe {
            let rt = JS_NewRuntime();
            assert!(!rt.is_null());
            let ctx = JS_NewContext(rt);
            assert!(!ctx.is_null());

            let code = CString::new("1 + 1").unwrap();
            let fname = CString::new("<eval>").unwrap();
            let result = JS_Eval(
                ctx,
                code.as_ptr(),
                4,
                fname.as_ptr(),
                JS_EVAL_TYPE_GLOBAL,
            );
            assert!(result.tag != JS_TAG_EXCEPTION, "eval threw");

            let mut n: f64 = 0.0;
            JS_ToFloat64(ctx, &mut n, result);
            assert_eq!(n, 2.0);

            JS_FreeValue(ctx, result);
            JS_FreeContext(ctx);
            JS_FreeRuntime(rt);
            let _ = ptr::null::<u8>();
        }
    }
}
