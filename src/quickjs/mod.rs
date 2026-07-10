//! QuickJS-ng engine backend for the v8 C-ABI compat layer.
//!
//! Mirrors the JSC backend's structure: `quickjs_sys` holds the raw QuickJS-ng
//! FFI (adapted from denoland/deno#34033's qjs_v8_compat), `shims` holds the
//! `v8__*` C-ABI definitions (currently auto-generated stubs; real
//! implementations land incrementally, same pattern as the JSC backend).

mod allocator;
mod arraybuffer;
mod cli_extra;
mod core;
mod exception;
mod function;
mod init;
mod inspector;
mod isolate;
mod misc;
mod module;
mod object;
mod primitive;
mod property;
pub(crate) mod quickjs_sys;
mod runtime;
mod serializer;
mod shims;
mod simdutf;
pub(crate) mod snapshot;
mod string;
mod value;
mod wasm;

#[cfg(test)]
mod raw_smoke_test {
  use super::quickjs_sys::*;
  use std::ffi::CString;
  use std::ptr;

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
      let result =
        JS_Eval(ctx, code.as_ptr(), 4, fname.as_ptr(), JS_EVAL_TYPE_GLOBAL);
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

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn callsite_preserves_receiver_type_and_method() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source = CString::new(
        "Error.prepareStackTrace = (_, sites) => {\n\
         const site = sites[0];\n\
         return [site.getTypeName(), site.getFunctionName(),\n\
                 site.getMethodName(), site.getThis()[Symbol.toStringTag]].join(':');\n\
         };\n\
         const receiver = {\n\
           [Symbol.toStringTag]: 'BenchContext',\n\
           start() { return new Error().stack; },\n\
         };\n\
         receiver.start();",
      )
      .unwrap();
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.as_bytes().len(),
        c"callsite-test.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "eval threw");

      let mut len = 0;
      let text = JS_ToCStringLen(ctx, &mut len, result);
      assert!(!text.is_null());
      let actual =
        std::str::from_utf8(std::slice::from_raw_parts(text as *const u8, len))
          .unwrap();
      assert_eq!(actual, "BenchContext:start:start:BenchContext");

      JS_FreeCString(ctx, text);
      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn property_access_errors_match_v8() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source =
        CString::new("try { undefined.fn } catch (error) { error.message }")
          .unwrap();
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.as_bytes().len(),
        c"typeerror-test.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "eval threw");

      let mut len = 0;
      let text = JS_ToCStringLen(ctx, &mut len, result);
      assert!(!text.is_null());
      let actual =
        std::str::from_utf8(std::slice::from_raw_parts(text as *const u8, len))
          .unwrap();
      assert_eq!(actual, "Cannot read properties of undefined (reading 'fn')");

      JS_FreeCString(ctx, text);
      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }
}

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

    assert_snapshot_context_data_preserves_global_identity();

    unsafe {
      v8::V8::dispose();
    }
    v8::V8::dispose_platform();
  }

  fn assert_snapshot_context_data_preserves_global_identity() {
    let context_data_index;
    let startup_data = {
      let mut creator = v8::Isolate::snapshot_creator(None, None);
      {
        let scope = std::pin::pin!(v8::HandleScope::new(&mut creator));
        let scope = &mut scope.init();
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        let code = v8::String::new(
          scope,
          "globalThis.sharedSnapshotValue = { marker: 1 }; sharedSnapshotValue",
        )
        .unwrap();
        let script = v8::Script::compile(scope, code, None).unwrap();
        let shared = script.run(scope).unwrap();
        scope.set_default_context(context);
        context_data_index = scope.add_context_data(context, shared);
      }
      creator.create_blob(v8::FunctionCodeHandling::Keep).unwrap()
    };

    {
      let params = v8::Isolate::create_params().snapshot_blob(startup_data);
      let isolate = &mut v8::Isolate::new(params);
      let scope = std::pin::pin!(v8::HandleScope::new(isolate));
      let scope = &mut scope.init();
      let context = v8::Context::new(scope, Default::default());
      let scope = &mut v8::ContextScope::new(scope, context);
      let restored = scope
        .get_context_data_from_snapshot_once::<v8::Value>(context_data_index)
        .unwrap();
      let code =
        v8::String::new(scope, "globalThis.sharedSnapshotValue").unwrap();
      let script = v8::Script::compile(scope, code, None).unwrap();
      let global_value = script.run(scope).unwrap();
      assert!(restored.strict_equals(global_value));
    }
  }
}
