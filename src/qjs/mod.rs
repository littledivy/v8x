//! QuickJS-ng engine backend for the v8 C-ABI compat layer.
//!
//! Mirrors the JSC backend's structure: `quickjs_sys` holds the raw QuickJS-ng
//! FFI (adapted from denoland/deno#34033's qjs_v8_compat), `shims` holds the
//! `v8__*` C-ABI definitions (currently auto-generated stubs; real
//! implementations land incrementally, same pattern as the JSC backend).

pub(crate) mod quickjs_sys;
mod shim_arraybuffer;
mod shim_core;
mod shim_impl;
mod shim_string;
mod shim_value;
mod shim_simdutf;
mod shim_inspector;
mod fam_arraybuffer;
mod fam_exception;
mod fam_function;
mod fam_isolate;
mod fam_misc;
mod fam_module;
mod fam_object;
mod fam_primitive;
mod fam_property;
mod fam_serializer;
mod fam_string;
mod fam_value;
mod shim_cli_extra;
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

/// End-to-end proof that the vendored v8 (rusty_v8) API surface evaluates
/// `1 + 1` through the QuickJS shim_core. Exercises the real `v8::` types —
/// the same surface deno_core uses — modeled on the doctest in `src/lib.rs`.
#[cfg(test)]
#[cfg(feature = "link_quickjs")]
mod api_test {
    use crate as v8;

    #[test]
    fn qjs_api_eval() {
        let platform = v8::new_default_platform(0, false).make_shared();
        v8::V8::initialize_platform(platform);
        v8::V8::initialize();

        {
            let isolate = &mut v8::Isolate::new(Default::default());

            let scope = std::pin::pin!(v8::HandleScope::new(isolate));
            let scope = &mut scope.init();
            let context = v8::Context::new(scope, Default::default());
            let scope = &mut v8::ContextScope::new(scope, context);

            let code = v8::String::new(scope, "1 + 1").unwrap();
            let script = v8::Script::compile(scope, code, None).unwrap();
            let result = script.run(scope).unwrap();
            assert_eq!(result.to_rust_string_lossy(scope), "2");
        }

        unsafe {
            v8::V8::dispose();
        }
        v8::V8::dispose_platform();
    }
}
