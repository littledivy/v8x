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
  use std::ffi::CStr;
  use std::ffi::CString;
  use std::ptr;
  use std::sync::atomic::AtomicUsize;
  use std::sync::atomic::Ordering;

  static PREPARE_STACK_CALLS: AtomicUsize = AtomicUsize::new(0);
  static UNHANDLED_REJECTIONS: AtomicUsize = AtomicUsize::new(0);
  static HANDLED_REJECTIONS: AtomicUsize = AtomicUsize::new(0);

  unsafe extern "C" fn test_prepare_stack_trace(
    ctx: *mut JSContext,
    _this: JSValue,
    _argc: std::ffi::c_int,
    _argv: *mut JSValue,
  ) -> JSValue {
    PREPARE_STACK_CALLS.fetch_add(1, Ordering::SeqCst);
    unsafe { JS_NewString(ctx, c"embedder stack".as_ptr()) }
  }

  unsafe extern "C" fn test_promise_rejection_tracker(
    _ctx: *mut JSContext,
    _promise: JSValue,
    _reason: JSValue,
    is_handled: std::ffi::c_int,
    _opaque: *mut std::ffi::c_void,
  ) {
    if is_handled == 0 {
      UNHANDLED_REJECTIONS.fetch_add(1, Ordering::SeqCst);
    } else {
      HANDLED_REJECTIONS.fetch_add(1, Ordering::SeqCst);
    }
  }

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
  fn synthetic_module_namespace_has_module_semantics() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let namespace = v82jsc_new_module_namespace(ctx);
      assert!(namespace.tag != JS_TAG_EXCEPTION);
      assert_eq!(
        v82jsc_module_namespace_set(
          ctx,
          namespace,
          c"default".as_ptr(),
          jsv_int32(1),
        ),
        0
      );

      let global = JS_GetGlobalObject(ctx);
      assert_eq!(
        JS_SetPropertyStr(
          ctx,
          global,
          c"namespace".as_ptr(),
          JS_DupValue(ctx, namespace),
        ),
        1
      );
      JS_FreeValue(ctx, global);

      let source = c"Object.preventExtensions(namespace);
        [Object.prototype.toString.call(namespace),
         Object.getPrototypeOf(namespace) === null,
         Object.isExtensible(namespace),
         Reflect.set(namespace, 'default', 2),
         namespace.default,
         Object.getOwnPropertyDescriptor(namespace, 'default').writable
        ].join(':')";
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.to_bytes().len(),
        c"synthetic-module-namespace.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "namespace inspection threw");
      let mut len = 0;
      let text = JS_ToCStringLen(ctx, &mut len, result);
      assert!(!text.is_null());
      let actual =
        std::str::from_utf8(std::slice::from_raw_parts(text as *const u8, len))
          .unwrap();
      assert_eq!(actual, "[object Module]:true:false:false:1:true");

      JS_FreeCString(ctx, text);
      JS_FreeValue(ctx, result);
      JS_FreeValue(ctx, namespace);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn embedder_prepare_stack_is_lazy_and_separate_from_user_hook() {
    unsafe {
      PREPARE_STACK_CALLS.store(0, Ordering::SeqCst);
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let callback = JS_NewCFunction(
        ctx,
        test_prepare_stack_trace,
        c"prepareStackTrace".as_ptr(),
        2,
      );
      JS_SetPrepareStackTraceCallback(ctx, callback);
      JS_FreeValue(ctx, callback);

      let setup = c"Error.prepareStackTrace = () => 'user stack'; globalThis.error = new Error('boom'); typeof Error.prepareStackTrace";
      let result = JS_Eval(
        ctx,
        setup.as_ptr(),
        setup.to_bytes().len(),
        c"prepare-stack-test.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "setup threw");
      assert_eq!(PREPARE_STACK_CALLS.load(Ordering::SeqCst), 0);
      JS_FreeValue(ctx, result);

      let result = JS_Eval(
        ctx,
        c"error.stack".as_ptr(),
        c"error.stack".to_bytes().len(),
        c"prepare-stack-test.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "stack access threw");
      let mut len = 0;
      let text = JS_ToCStringLen(ctx, &mut len, result);
      assert!(!text.is_null());
      let actual =
        std::str::from_utf8(std::slice::from_raw_parts(text as *const u8, len))
          .unwrap();
      assert_eq!(actual, "embedder stack");
      assert_eq!(PREPARE_STACK_CALLS.load(Ordering::SeqCst), 1);

      JS_FreeCString(ctx, text);
      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn async_rejection_preserves_custom_stack_getter() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source = c"let getter = () => 'custom stack';
        globalThis.result = 'pending';
        (async function outer() {
          const error = new Error('boom');
          Object.defineProperty(error, 'stack', { get: getter });
          try {
            await Promise.reject(error);
          } catch (error) {
            const descriptor = Object.getOwnPropertyDescriptor(error, 'stack');
            globalThis.result = `${descriptor.get === getter}:${error.stack}`;
          }
        })();";
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.to_bytes().len(),
        c"custom-async-stack.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "eval threw");
      JS_FreeValue(ctx, result);

      let mut job_ctx = ptr::null_mut();
      while JS_IsJobPending(rt) {
        assert!(JS_ExecutePendingJob(rt, &mut job_ctx) >= 0);
      }

      let result = JS_Eval(
        ctx,
        c"result".as_ptr(),
        c"result".to_bytes().len(),
        c"custom-async-stack.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "result access threw");
      let mut len = 0;
      let text = JS_ToCStringLen(ctx, &mut len, result);
      assert!(!text.is_null());
      let actual =
        std::str::from_utf8(std::slice::from_raw_parts(text as *const u8, len))
          .unwrap();
      assert_eq!(actual, "true:custom stack");

      JS_FreeCString(ctx, text);
      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn marking_rejected_promise_handled_notifies_tracker() {
    unsafe {
      UNHANDLED_REJECTIONS.store(0, Ordering::SeqCst);
      HANDLED_REJECTIONS.store(0, Ordering::SeqCst);
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      JS_SetHostPromiseRejectionTracker(
        rt,
        Some(test_promise_rejection_tracker),
        ptr::null_mut(),
      );
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source = c"Promise.reject('boom')";
      let promise = JS_Eval(
        ctx,
        source.as_ptr(),
        source.to_bytes().len(),
        c"mark-promise-handled.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(promise.tag != JS_TAG_EXCEPTION, "eval threw");
      assert_eq!(UNHANDLED_REJECTIONS.load(Ordering::SeqCst), 1);
      assert_eq!(HANDLED_REJECTIONS.load(Ordering::SeqCst), 0);

      JS_PromiseMarkAsHandled(ctx, promise);
      JS_PromiseMarkAsHandled(ctx, promise);
      assert_eq!(UNHANDLED_REJECTIONS.load(Ordering::SeqCst), 1);
      assert_eq!(HANDLED_REJECTIONS.load(Ordering::SeqCst), 1);

      JS_FreeValue(ctx, promise);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn module_evaluation_exception_remains_available() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source = c"throw new Error('boom')";
      let module = JS_Eval(
        ctx,
        source.as_ptr(),
        source.to_bytes().len(),
        c"throwing-module.js".as_ptr(),
        JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_COMPILE_ONLY,
      );
      assert_eq!(module.tag, JS_TAG_MODULE);
      let module_def = module.u.ptr.cast::<JSModuleDef>();

      let result = JS_EvalFunction(ctx, module);
      assert!(JS_IsPromise(result));
      assert_eq!(JS_PromiseState(ctx, result), 2);

      let exception = v82jsc_module_get_exception(ctx, module_def);
      assert_eq!(exception.tag, JS_TAG_OBJECT);
      let message = JS_GetPropertyStr(ctx, exception, c"message".as_ptr());
      let message_text = JS_ToCString(ctx, message);
      assert!(!message_text.is_null());
      assert_eq!(CStr::from_ptr(message_text).to_bytes(), b"boom");

      JS_FreeCString(ctx, message_text);
      JS_FreeValue(ctx, message);
      JS_FreeValue(ctx, exception);
      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn missing_member_call_names_callee() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source = c"globalThis.Deno = {}; Deno.openKv();";
      super::core::register_script_source(
        "missing-member.js",
        source.to_str().unwrap(),
      );
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.to_bytes().len(),
        c"missing-member.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert_eq!(result.tag, JS_TAG_EXCEPTION);

      let exception = JS_GetException(ctx);
      let message = JS_GetPropertyStr(ctx, exception, c"message".as_ptr());
      let message_text = JS_ToCString(ctx, message);
      assert!(!message_text.is_null());
      assert_eq!(
        CStr::from_ptr(message_text).to_bytes(),
        b"Deno.openKv is not a function"
      );

      JS_FreeCString(ctx, message_text);
      JS_FreeValue(ctx, message);
      JS_FreeValue(ctx, exception);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn eval_preserves_embedded_nul_raw() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source = b"globalThis.value = 1; // virtual\0module\n\
                     globalThis.value = 2; value;\0";
      let result = JS_Eval(
        ctx,
        source.as_ptr() as *const std::ffi::c_char,
        source.len() - 1,
        c"nul-source-test.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "eval threw");

      let mut actual = 0;
      assert_eq!(JS_ToInt32(ctx, &mut actual, result), 0);
      assert_eq!(actual, 2);

      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn strict_class_method_allows_reserved_property_name() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source = CString::new(
        "'use strict'; class Scheduler { yield() { return 1; } } new Scheduler().yield();",
      )
      .unwrap();
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.as_bytes().len(),
        c"strict-class-method.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "eval threw");

      let mut actual = 0;
      assert_eq!(JS_ToInt32(ctx, &mut actual, result), 0);
      assert_eq!(actual, 1);

      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn class_method_name_does_not_shadow_outer_binding() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source = CString::new(
        "function record(value) { return value + 1; }\n\
         class Gauge {\n\
           #value = 1;\n\
           record() { return record(this.#value); }\n\
         }\n\
         new Gauge().record();",
      )
      .unwrap();
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.as_bytes().len(),
        c"method-outer-binding.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "eval threw");

      let mut actual = 0;
      assert_eq!(JS_ToInt32(ctx, &mut actual, result), 0);
      assert_eq!(actual, 2);

      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
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
  fn callsite_reports_global_receiver_as_toplevel() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source = CString::new(
        "Error.prepareStackTrace = (_, sites) => {\n\
         const site = sites[0];\n\
         return `${site.getFunctionName()}:${site.isToplevel()}`;\n\
         };\n\
         globalThis.callback = function callback() { return new Error().stack; };\n\
         globalThis.callback();",
      )
      .unwrap();
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.as_bytes().len(),
        c"callsite-toplevel-test.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "eval threw");

      let mut len = 0;
      let text = JS_ToCStringLen(ctx, &mut len, result);
      assert!(!text.is_null());
      let actual =
        std::str::from_utf8(std::slice::from_raw_parts(text as *const u8, len))
          .unwrap();
      assert_eq!(actual, "callback:true");

      JS_FreeCString(ctx, text);
      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn callsite_reports_native_constructor() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source = CString::new(
        "Error.prepareStackTrace = (_, sites) => {
         const site = sites.find((site) => site.getFunctionName() === 'Promise');
         return `${site.getFunctionName()}:${site.isConstructor()}:${site.getLineNumber()}:${site.getColumnNumber()}`;
         };
         let error;
         new Promise(() => { error = new Error('fail'); });
         error.stack;",
      )
      .unwrap();
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.as_bytes().len(),
        c"callsite-constructor-test.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "eval threw");

      let mut len = 0;
      let text = JS_ToCStringLen(ctx, &mut len, result);
      assert!(!text.is_null());
      let actual =
        std::str::from_utf8(std::slice::from_raw_parts(text as *const u8, len))
          .unwrap();
      assert_eq!(actual, "Promise:true:null:null");

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

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn error_stack_accepts_arbitrary_values() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source = CString::new(
        "const error = new Error('x');
         error.stack = undefined;
         const hasOwnStack = Object.hasOwn(error, 'stack');
         Error.prototype.stack = 42;
         `${error.stack}:${hasOwnStack}:${Error.prototype.stack}`;",
      )
      .unwrap();
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.as_bytes().len(),
        c"error-stack-value-test.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "eval threw");

      let mut len = 0;
      let text = JS_ToCStringLen(ctx, &mut len, result);
      assert!(!text.is_null());
      let actual =
        std::str::from_utf8(std::slice::from_raw_parts(text as *const u8, len))
          .unwrap();
      assert_eq!(actual, "undefined:true:42");

      JS_FreeCString(ctx, text);
      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  #[cfg(feature = "link_quickjs")]
  fn function_constructor_preserves_caller_resource_name() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let source =
        CString::new("new Function('return new Error().stack')()").unwrap();
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.as_bytes().len(),
        c"function-origin-test.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "eval threw");

      let mut len = 0;
      let text = JS_ToCStringLen(ctx, &mut len, result);
      assert!(!text.is_null());
      let actual =
        std::str::from_utf8(std::slice::from_raw_parts(text as *const u8, len))
          .unwrap();
      assert!(
        actual.contains("function-origin-test.js"),
        "generated function stack did not preserve caller resource name: {actual}"
      );
      assert!(
        !actual.contains("<input>"),
        "unexpected synthetic name: {actual}"
      );

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

    assert_continuation_data_survives_await();
    assert_snapshot_context_data_preserves_global_identity();

    unsafe {
      v8::V8::dispose();
    }
    v8::V8::dispose_platform();
  }

  fn assert_continuation_data_survives_await() {
    let isolate = &mut v8::Isolate::new(Default::default());
    let scope = std::pin::pin!(v8::HandleScope::new(isolate));
    let scope = &mut scope.init();
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);

    let extras = context.get_extras_binding_object(scope);
    let extras_name = v8::String::new(scope, "__extras").unwrap();
    context
      .global(scope)
      .set(scope, extras_name.into(), extras.into())
      .unwrap();

    let code = v8::String::new(
      scope,
      "const alreadyResolved = Promise.resolve();\n\
       const inner = { name: 'inner' };\n\
       const outer = { name: 'outer' };\n\
       __extras.setContinuationPreservedEmbedderData(inner);\n\
       async function run() {\n\
         await alreadyResolved;\n\
         globalThis.observedContinuation =\n\
           __extras.getContinuationPreservedEmbedderData().name;\n\
       }\n\
       run();\n\
       __extras.setContinuationPreservedEmbedderData(outer);",
    )
    .unwrap();
    let script = v8::Script::compile(scope, code, None).unwrap();
    script.run(scope).unwrap();
    scope.perform_microtask_checkpoint();

    let check = v8::String::new(
      scope,
      "observedContinuation + ':' +\n\
       (__extras.getContinuationPreservedEmbedderData() === undefined)",
    )
    .unwrap();
    let script = v8::Script::compile(scope, check, None).unwrap();
    let result = script.run(scope).unwrap();
    assert_eq!(result.to_rust_string_lossy(scope), "inner:true");
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
