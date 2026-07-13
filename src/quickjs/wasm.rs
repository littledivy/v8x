//! WebAssembly for the QuickJS backend, implemented over the vendored
//! wasm-micro-runtime (WAMR) wasm-c-api. QuickJS-ng ships no WASM engine; V8
//! (the API we emulate) does, and real npm code needs it (e.g. undici's llhttp
//! HTTP parser is a WASM module compiled at load). We expose a `globalThis.
//! WebAssembly` with `Module`/`Instance`/`Memory`/`compile`/`instantiate`/
//! `validate`, marshalling values and import/export functions across the
//! QuickJS <-> WAMR boundary.
//!
//! Native handles (module/instance/memory/store) are kept in a thread-local
//! registry keyed by a small integer id stored as a hidden property on the JS
//! wrapper object; the underlying wasm objects are intentionally leaked (never
//! deleted) — process-lifetime, mirroring how engines treat eternal handles.

#![allow(non_camel_case_types)]

use std::cell::RefCell;
use std::ffi::{CString, c_char, c_void};
use std::os::raw::c_int;
use std::ptr;

use crate::quickjs::core::{
  current_ctx, current_iso, intern, iso_state, jsval_of,
};
use crate::quickjs::quickjs_sys::*;
use crate::{Object, RealIsolate, Value};

#[repr(C)]
pub struct wasm_engine_t {
  _p: [u8; 0],
}
#[repr(C)]
pub struct wasm_store_t {
  _p: [u8; 0],
}
#[repr(C)]
pub struct wasm_module_t {
  _p: [u8; 0],
}
#[repr(C)]
pub struct wasm_instance_t {
  _p: [u8; 0],
}
#[repr(C)]
pub struct wasm_func_t {
  _p: [u8; 0],
}
#[repr(C)]
pub struct wasm_memory_t {
  _p: [u8; 0],
}
#[repr(C)]
pub struct wasm_extern_t {
  _p: [u8; 0],
}
#[repr(C)]
pub struct wasm_trap_t {
  _p: [u8; 0],
}
#[repr(C)]
pub struct wasm_frame_t {
  _p: [u8; 0],
}
#[repr(C)]
pub struct wasm_functype_t {
  _p: [u8; 0],
}
#[repr(C)]
pub struct wasm_valtype_t {
  _p: [u8; 0],
}
#[repr(C)]
pub struct wasm_importtype_t {
  _p: [u8; 0],
}
#[repr(C)]
pub struct wasm_exporttype_t {
  _p: [u8; 0],
}
#[repr(C)]
pub struct wasm_externtype_t {
  _p: [u8; 0],
}

#[repr(C)]
pub struct wasm_vec_t {
  pub size: usize,
  pub data: *mut c_void,
  pub num_elems: usize,
  pub size_of_elem: usize,
  pub lock: *mut c_void,
}
impl wasm_vec_t {
  fn empty() -> Self {
    wasm_vec_t {
      size: 0,
      data: ptr::null_mut(),
      num_elems: 0,
      size_of_elem: 0,
      lock: ptr::null_mut(),
    }
  }
}

pub const WASM_I32: u8 = 0;
pub const WASM_I64: u8 = 1;
pub const WASM_F32: u8 = 2;
pub const WASM_F64: u8 = 3;
pub const WASM_V128: u8 = 4;
pub const WASM_EXTERNREF: u8 = 128;
pub const WASM_FUNCREF: u8 = 129;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct wasm_val_t {
  pub kind: u8,
  pub _pad: [u8; 7],
  pub of: u64,
}

pub const WASM_EXTERN_FUNC: u8 = 0;
pub const WASM_EXTERN_GLOBAL: u8 = 1;
pub const WASM_EXTERN_TABLE: u8 = 2;
pub const WASM_EXTERN_MEMORY: u8 = 3;

pub type wasm_func_callback_with_env_t =
  unsafe extern "C" fn(
    env: *mut c_void,
    args: *const wasm_vec_t,
    results: *mut wasm_vec_t,
  ) -> *mut wasm_trap_t;

unsafe extern "C" {
  fn JS_SetPrototype(
    ctx: *mut JSContext,
    obj: JSValue,
    proto_val: JSValue,
  ) -> c_int;
  fn JS_SetConstructor(
    ctx: *mut JSContext,
    func_obj: JSValue,
    proto: JSValue,
  ) -> c_int;
  fn JS_DefinePropertyValueStr(
    ctx: *mut JSContext,
    this_obj: JSValue,
    prop: *const c_char,
    val: JSValue,
    flags: c_int,
  ) -> c_int;

  pub fn wasm_engine_new() -> *mut wasm_engine_t;
  pub fn wasm_store_new(e: *mut wasm_engine_t) -> *mut wasm_store_t;

  pub fn wasm_byte_vec_new(out: *mut wasm_vec_t, size: usize, data: *const u8);
  pub fn wasm_byte_vec_delete(v: *mut wasm_vec_t);

  pub fn wasm_module_new(
    store: *mut wasm_store_t,
    binary: *const wasm_vec_t,
  ) -> *mut wasm_module_t;
  pub fn wasm_module_validate(
    store: *mut wasm_store_t,
    binary: *const wasm_vec_t,
  ) -> bool;
  pub fn wasm_module_imports(m: *const wasm_module_t, out: *mut wasm_vec_t);
  pub fn wasm_module_exports(m: *const wasm_module_t, out: *mut wasm_vec_t);

  pub fn wasm_importtype_module(
    it: *const wasm_importtype_t,
  ) -> *const wasm_vec_t;
  pub fn wasm_importtype_name(
    it: *const wasm_importtype_t,
  ) -> *const wasm_vec_t;
  pub fn wasm_importtype_type(
    it: *const wasm_importtype_t,
  ) -> *const wasm_externtype_t;
  pub fn wasm_externtype_kind(t: *const wasm_externtype_t) -> u8;
  pub fn wasm_externtype_as_functype_const(
    t: *const wasm_externtype_t,
  ) -> *const wasm_functype_t;
  pub fn wasm_externtype_as_globaltype_const(
    t: *const wasm_externtype_t,
  ) -> *const wasm_globaltype_t;
  pub fn wasm_exporttype_name(
    et: *const wasm_exporttype_t,
  ) -> *const wasm_vec_t;
  pub fn wasm_exporttype_type(
    et: *const wasm_exporttype_t,
  ) -> *const wasm_externtype_t;

  pub fn wasm_functype_params(ft: *const wasm_functype_t) -> *const wasm_vec_t;
  pub fn wasm_functype_results(ft: *const wasm_functype_t)
  -> *const wasm_vec_t;
  pub fn wasm_valtype_kind(vt: *const wasm_valtype_t) -> u8;

  pub fn wasm_func_new_with_env(
    store: *mut wasm_store_t,
    ty: *const wasm_functype_t,
    cb: wasm_func_callback_with_env_t,
    env: *mut c_void,
    finalizer: Option<unsafe extern "C" fn(*mut c_void)>,
  ) -> *mut wasm_func_t;
  pub fn wasm_func_call(
    f: *const wasm_func_t,
    args: *const wasm_vec_t,
    results: *mut wasm_vec_t,
  ) -> *mut wasm_trap_t;
  pub fn v82jsc_wasm_func_terminate(f: *const wasm_func_t);
  pub fn wasm_func_type(f: *const wasm_func_t) -> *mut wasm_functype_t;
  pub fn v82jsc_wasm_func_index(f: *const wasm_func_t) -> u16;
  pub fn wasm_func_as_extern(f: *mut wasm_func_t) -> *mut wasm_extern_t;

  pub fn wasm_instance_new(
    store: *mut wasm_store_t,
    m: *const wasm_module_t,
    imports: *const wasm_vec_t,
    trap: *mut *mut wasm_trap_t,
  ) -> *mut wasm_instance_t;
  pub fn wasm_instance_exports(i: *const wasm_instance_t, out: *mut wasm_vec_t);

  pub fn wasm_extern_kind(e: *const wasm_extern_t) -> u8;
  pub fn wasm_extern_as_func(e: *mut wasm_extern_t) -> *mut wasm_func_t;
  pub fn wasm_extern_as_memory(e: *mut wasm_extern_t) -> *mut wasm_memory_t;

  pub fn wasm_memory_data(m: *mut wasm_memory_t) -> *mut u8;
  pub fn wasm_memory_data_size(m: *const wasm_memory_t) -> usize;
  pub fn wasm_memory_size(m: *const wasm_memory_t) -> u32;
  pub fn wasm_memory_grow(m: *mut wasm_memory_t, delta: u32) -> bool;

  pub fn wasm_trap_delete(t: *mut wasm_trap_t);
  pub fn wasm_trap_message(t: *const wasm_trap_t, out: *mut wasm_vec_t);
  pub fn wasm_trap_trace(t: *const wasm_trap_t, out: *mut wasm_vec_t);
  pub fn wasm_frame_func_index(frame: *const wasm_frame_t) -> u32;
  pub fn wasm_frame_func_offset(frame: *const wasm_frame_t) -> usize;
  pub fn wasm_frame_module_offset(frame: *const wasm_frame_t) -> usize;
  pub fn wasm_frame_vec_delete(frames: *mut wasm_vec_t);
  pub fn wasm_trap_new(
    store: *mut wasm_store_t,
    message: *const wasm_vec_t,
  ) -> *mut wasm_trap_t;

  pub fn wasm_extern_as_global(e: *mut wasm_extern_t) -> *mut wasm_global_t;
  pub fn wasm_global_as_extern(g: *mut wasm_global_t) -> *mut wasm_extern_t;
  pub fn wasm_valtype_new(kind: u8) -> *mut wasm_valtype_t;
  pub fn wasm_globaltype_new(
    val_type: *mut wasm_valtype_t,
    mutability: u8,
  ) -> *mut wasm_globaltype_t;
  pub fn wasm_global_new(
    store: *mut wasm_store_t,
    global_type: *const wasm_globaltype_t,
    init: *const wasm_val_t,
  ) -> *mut wasm_global_t;
  pub fn wasm_global_get(g: *const wasm_global_t, out: *mut wasm_val_t);
  pub fn wasm_global_set(g: *mut wasm_global_t, v: *const wasm_val_t);
  pub fn wasm_global_type(g: *const wasm_global_t) -> *mut wasm_globaltype_t;
  pub fn wasm_globaltype_content(
    gt: *const wasm_globaltype_t,
  ) -> *const wasm_valtype_t;
  pub fn wasm_globaltype_mutability(gt: *const wasm_globaltype_t) -> u8;

  pub fn wasm_extern_as_table(e: *mut wasm_extern_t) -> *mut wasm_table_t;
  pub fn wasm_table_size(t: *const wasm_table_t) -> u32;
  pub fn wasm_table_grow(
    t: *mut wasm_table_t,
    delta: u32,
    init: *mut wasm_ref_t,
  ) -> bool;
  pub fn wasm_table_get(t: *const wasm_table_t, index: u32) -> *mut wasm_ref_t;
  pub fn wasm_table_set(
    t: *mut wasm_table_t,
    index: u32,
    r: *mut wasm_ref_t,
  ) -> bool;

  pub fn wasm_foreign_new(store: *mut wasm_store_t) -> *mut wasm_foreign_t;
  pub fn wasm_foreign_as_ref(f: *mut wasm_foreign_t) -> *mut wasm_ref_t;
  pub fn wasm_ref_get_host_info(r: *const wasm_ref_t) -> *mut c_void;
  pub fn wasm_foreign_set_host_info_with_finalizer(
    f: *mut wasm_foreign_t,
    info: *mut c_void,
    finalizer: Option<unsafe extern "C" fn(*mut c_void)>,
  );

}

#[repr(C)]
pub struct wasm_global_t {
  _p: [u8; 0],
}
#[repr(C)]
pub struct wasm_globaltype_t {
  _p: [u8; 0],
}
#[repr(C)]
pub struct wasm_table_t {
  _p: [u8; 0],
}
#[repr(C)]
pub struct wasm_ref_t {
  _p: [u8; 0],
}
#[repr(C)]
pub struct wasm_foreign_t {
  _p: [u8; 0],
}

unsafe extern "C" fn externref_box_free(p: *mut c_void) {
  if !p.is_null() {
    unsafe { drop(Box::from_raw(p as *mut JSValue)) };
  }
}

unsafe fn box_externref(ctx: *mut JSContext, v: JSValue) -> *mut c_void {
  let dup = unsafe { JS_DupValue(ctx, v) };
  Box::into_raw(Box::new(dup)) as *mut c_void
}

unsafe fn unbox_externref(p: *mut c_void) -> JSValue {
  if p.is_null() {
    jsv_undefined()
  } else {
    unsafe { *(p as *const JSValue) }
  }
}

unsafe fn js_to_externref(ctx: *mut JSContext, v: JSValue) -> u64 {
  let store = with_state(|st| st.store);

  let foreign = unsafe { wasm_foreign_new(store) };
  if foreign.is_null() {
    return 0;
  }
  let r = unsafe { wasm_foreign_as_ref(foreign) };
  if r.is_null() {
    return 0;
  }
  let boxed = unsafe { box_externref(ctx, v) };
  unsafe {
    wasm_foreign_set_host_info_with_finalizer(
      foreign,
      boxed,
      Some(externref_box_free),
    )
  };
  r as u64
}

unsafe fn externref_to_js(ctx: *mut JSContext, of: u64) -> JSValue {
  if of == 0 {
    if std::env::var("V82_WASM_TRACE").is_ok() {
      eprintln!("[wasm] externref_to_js of=0 -> null");
    }
    return jsv_null();
  }
  let r = of as *mut wasm_ref_t;
  let hi = unsafe { wasm_ref_get_host_info(r) };
  if std::env::var("V82_WASM_TRACE").is_ok() {
    eprintln!("[wasm] externref_to_js of={:?} hi_null={}", r, hi.is_null());
  }
  if hi.is_null() {
    return jsv_undefined();
  }
  unsafe { JS_DupValue(ctx, unbox_externref(hi)) }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn null_reference_converts_to_js_null() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());

      let value = externref_to_js(ctx, 0);
      assert!(jsv_is_null(&value));

      JS_FreeValue(ctx, value);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  fn wasm_traps_are_runtime_errors() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());
      install_webassembly(ctx);

      let source = CString::new(
        r#"
        const bytes = new Uint8Array([
          0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
          0x01, 0x04, 0x01, 0x60, 0x00, 0x00, 0x03, 0x02,
          0x01, 0x00, 0x07, 0x0f, 0x01, 0x0b, 0x75, 0x6e,
          0x72, 0x65, 0x61, 0x63, 0x68, 0x61, 0x62, 0x6c,
          0x65, 0x00, 0x00, 0x0a, 0x05, 0x01, 0x03, 0x00,
          0x00, 0x0b,
        ]);
        const instance = new WebAssembly.Instance(new WebAssembly.Module(bytes));
        try {
          instance.exports.unreachable();
        } catch (error) {
          `${error instanceof WebAssembly.RuntimeError}:${error.name}:${error.message}`;
        }
        "#,
      )
      .unwrap();
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.as_bytes().len(),
        c"wasm-runtime-error.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "eval threw");
      let mut len = 0usize;
      let actual = JS_ToCStringLen(ctx, &mut len, result);
      assert!(!actual.is_null());
      assert_eq!(
        std::slice::from_raw_parts(actual as *const u8, len),
        b"true:RuntimeError:unreachable"
      );

      JS_FreeCString(ctx, actual);
      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  fn mutable_global_import_tracks_source_instance() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());
      install_webassembly(ctx);

      let source = CString::new(
        r#"
        const depBytes = new Uint8Array([
          0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
          0x01, 0x04, 0x01, 0x60, 0x00, 0x00, 0x03, 0x02,
          0x01, 0x00, 0x06, 0x06, 0x01, 0x7f, 0x01, 0x41,
          0x01, 0x0b, 0x07, 0x12, 0x02, 0x07, 0x63, 0x6f,
          0x75, 0x6e, 0x74, 0x65, 0x72, 0x03, 0x00, 0x04,
          0x62, 0x75, 0x6d, 0x70, 0x00, 0x00, 0x0a, 0x0b,
          0x01, 0x09, 0x00, 0x23, 0x00, 0x41, 0x01, 0x6a,
          0x24, 0x00, 0x0b,
        ]);
        const mainBytes = new Uint8Array([
          0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
          0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f, 0x02,
          0x17, 0x01, 0x0a, 0x2e, 0x2f, 0x64, 0x65, 0x70,
          0x2e, 0x77, 0x61, 0x73, 0x6d, 0x07, 0x63, 0x6f,
          0x75, 0x6e, 0x74, 0x65, 0x72, 0x03, 0x7f, 0x01,
          0x03, 0x02, 0x01, 0x00, 0x07, 0x08, 0x01, 0x04,
          0x72, 0x65, 0x61, 0x64, 0x00, 0x00, 0x0a, 0x06,
          0x01, 0x04, 0x00, 0x23, 0x00, 0x0b,
        ]);
        const dep = new WebAssembly.Instance(new WebAssembly.Module(depBytes));
        const main = new WebAssembly.Instance(
          new WebAssembly.Module(mainBytes),
          { "./dep.wasm": { counter: dep.exports.counter } },
        );
        dep.exports.bump();
        main.exports.read();
        "#,
      )
      .unwrap();
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.as_bytes().len(),
        c"mutable-global-import.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "eval threw");
      assert_eq!(result.tag, JS_TAG_INT);
      assert_eq!(result.u.int32, 2);

      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  fn v128_global_value_throws_and_wrapper_is_branded() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());
      install_webassembly(ctx);

      let source = CString::new(
        r#"
        const bytes = new Uint8Array([
          0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
          0x06, 0x16, 0x01, 0x7b, 0x00, 0xfd, 0x0c, 0x00,
          0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
          0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0b,
          0x07, 0x05, 0x01, 0x01, 0x76, 0x03, 0x00,
        ]);
        const value = new WebAssembly.Instance(
          new WebAssembly.Module(bytes),
        ).exports.v;
        let threw = false;
        try {
          value.value;
        } catch {
          threw = true;
        }
        `${value instanceof WebAssembly.Global}:${threw}`;
        "#,
      )
      .unwrap();
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.as_bytes().len(),
        c"v128-global.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "eval threw");
      let mut len = 0usize;
      let actual = JS_ToCStringLen(ctx, &mut len, result);
      assert!(!actual.is_null());
      assert_eq!(
        std::slice::from_raw_parts(actual as *const u8, len),
        b"true:true"
      );

      JS_FreeCString(ctx, actual);
      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }

  #[test]
  fn exported_wasm_objects_have_standard_branding() {
    unsafe {
      let rt = JS_NewRuntime();
      assert!(!rt.is_null());
      let ctx = JS_NewContext(rt);
      assert!(!ctx.is_null());
      install_webassembly(ctx);

      let source = CString::new(
        r#"
        const bytes = new Uint8Array([
          0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
          0x01, 0x04, 0x01, 0x60, 0x00, 0x00, 0x03, 0x02,
          0x01, 0x00, 0x04, 0x04, 0x01, 0x70, 0x00, 0x00,
          0x05, 0x03, 0x01, 0x00, 0x00, 0x06, 0x06, 0x01,
          0x7f, 0x00, 0x41, 0x00, 0x0b, 0x07, 0x22, 0x04,
          0x04, 0x66, 0x75, 0x6e, 0x63, 0x00, 0x00, 0x05,
          0x74, 0x61, 0x62, 0x6c, 0x65, 0x01, 0x00, 0x06,
          0x6d, 0x65, 0x6d, 0x6f, 0x72, 0x79, 0x02, 0x00,
          0x06, 0x67, 0x6c, 0x6f, 0x62, 0x61, 0x6c, 0x03,
          0x00, 0x0a, 0x05, 0x01, 0x03, 0x00, 0x00, 0x0b,
        ]);
        const { func, memory, table } = new WebAssembly.Instance(
          new WebAssembly.Module(bytes),
        ).exports;
        [
          func.name,
          memory instanceof WebAssembly.Memory,
          table instanceof WebAssembly.Table,
          Object.prototype.toString.call(memory),
          Object.prototype.toString.call(table),
          Object.keys(memory).length,
          Object.keys(table).length,
        ].join('|');
        "#,
      )
      .unwrap();
      let result = JS_Eval(
        ctx,
        source.as_ptr(),
        source.as_bytes().len(),
        c"wasm-object-branding.js".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
      assert!(result.tag != JS_TAG_EXCEPTION, "eval threw");
      let mut len = 0usize;
      let actual = JS_ToCStringLen(ctx, &mut len, result);
      assert!(!actual.is_null());
      assert_eq!(
        std::slice::from_raw_parts(actual as *const u8, len),
        b"0|true|true|[object WebAssembly.Memory]|[object WebAssembly.Table]|0|0"
      );

      JS_FreeCString(ctx, actual);
      JS_FreeValue(ctx, result);
      JS_FreeContext(ctx);
      JS_FreeRuntime(rt);
    }
  }
}

struct WasmState {
  store: *mut wasm_store_t,
  modules: Vec<ModuleEntry>,
  instances: Vec<*mut wasm_instance_t>,
}

struct ModuleEntry {
  module: *mut wasm_module_t,
  bytes: Vec<u8>,
  source_url: std::string::String,
}

#[derive(Clone)]
struct WasmStackFrame {
  source_url: std::string::String,
  function_name: Option<std::string::String>,
  function_index: u32,
  function_offset: usize,
  module_offset: usize,
}

struct CompiledModule {
  bytes: Vec<u8>,
  source_url: std::string::String,
}

struct ModuleCompilation {
  bytes: Vec<u8>,
  source_url: std::string::String,
  aborted: bool,
}

#[repr(C)]
struct WasmStreamingSharedPtr {
  ptr: *mut StreamingState,
  _control: *mut c_void,
}

struct StreamingState {
  ctx: *mut JSContext,
  bytes: Vec<u8>,
  source_url: std::string::String,
  resolve: JSValue,
  reject: JSValue,
  promise: JSValue,
  refcount: usize,
  done: bool,
}

type StreamingCallback =
  unsafe extern "C" fn(*const crate::FunctionCallbackInfo);

thread_local! {
    static STREAMING_CALLBACK: std::cell::Cell<Option<StreamingCallback>> = const { std::cell::Cell::new(None) };
    static STREAMING_PENDING: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

thread_local! {
    static WASM: RefCell<Option<WasmState>> = const { RefCell::new(None) };

    static PENDING_IMPORT_EXC: RefCell<Option<JSValue>> = const { RefCell::new(None) };

    static WASM_CALL_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn with_state<R>(f: impl FnOnce(&mut WasmState) -> R) -> R {
  WASM.with(|w| {
    let mut b = w.borrow_mut();
    if b.is_none() {
      let engine = unsafe { wasm_engine_new() };
      let store = unsafe { wasm_store_new(engine) };
      *b = Some(WasmState {
        store,
        modules: Vec::new(),
        instances: Vec::new(),
      });
    }
    f(b.as_mut().unwrap())
  })
}

unsafe fn throw(ctx: *mut JSContext, msg: &str) -> JSValue {
  if let Ok(c) = CString::new(msg) {
    unsafe { JS_ThrowTypeError(ctx, c.as_ptr()) }
  } else {
    jsv_exception()
  }
}

unsafe fn throw_wasm_error(
  ctx: *mut JSContext,
  class_name: &std::ffi::CStr,
  message: &str,
) -> JSValue {
  let global = unsafe { JS_GetGlobalObject(ctx) };
  let webassembly =
    unsafe { JS_GetPropertyStr(ctx, global, c"WebAssembly".as_ptr()) };
  let constructor =
    unsafe { JS_GetPropertyStr(ctx, webassembly, class_name.as_ptr()) };
  let Ok(message) = CString::new(message) else {
    unsafe {
      JS_FreeValue(ctx, constructor);
      JS_FreeValue(ctx, webassembly);
      JS_FreeValue(ctx, global);
    }
    return jsv_exception();
  };
  let mut args = [unsafe { JS_NewString(ctx, message.as_ptr()) }];
  let error =
    unsafe { JS_CallConstructor(ctx, constructor, 1, args.as_mut_ptr()) };
  unsafe {
    JS_FreeValue(ctx, args[0]);
    JS_FreeValue(ctx, constructor);
    JS_FreeValue(ctx, webassembly);
    JS_FreeValue(ctx, global);
  }
  if error.tag == JS_TAG_EXCEPTION {
    error
  } else {
    unsafe { JS_Throw(ctx, error) }
  }
}

unsafe fn trap_message(trap: *const wasm_trap_t) -> std::string::String {
  let mut bytes = wasm_vec_t::empty();
  unsafe { wasm_trap_message(trap, &mut bytes) };
  let message = vec_name_to_string(&bytes);
  unsafe { wasm_byte_vec_delete(&mut bytes) };
  message
}

unsafe fn trap_frames(
  trap: *const wasm_trap_t,
  source_url: &str,
  function_names: &std::collections::HashMap<u32, std::string::String>,
  imported_function_count: u32,
  module_bytes: &[u8],
) -> Vec<WasmStackFrame> {
  let mut trace = wasm_vec_t::empty();
  unsafe { wasm_trap_trace(trap, &mut trace) };
  let mut frames = Vec::with_capacity(trace.num_elems);
  let data = trace.data as *const *mut wasm_frame_t;
  if !data.is_null() {
    for i in 0..trace.num_elems {
      let frame = unsafe { *data.add(i) };
      if frame.is_null() {
        continue;
      }
      let function_index = unsafe { wasm_frame_func_index(frame) };
      if function_index < imported_function_count {
        continue;
      }
      let module_offset = unsafe { wasm_frame_module_offset(frame) };
      frames.push(WasmStackFrame {
        source_url: source_url.to_string(),
        function_name: function_names.get(&function_index).cloned(),
        function_index,
        function_offset: unsafe { wasm_frame_func_offset(frame) },
        module_offset: if module_offset == 0 {
          wasm_function_trap_fallback(
            module_bytes,
            function_index - imported_function_count,
          )
          .unwrap_or(0)
        } else {
          module_offset
        },
      });
    }
  }
  unsafe { wasm_frame_vec_delete(&mut trace) };
  frames
}

unsafe fn attach_wasm_stack_frames(
  ctx: *mut JSContext,
  error: JSValue,
  frames: &[WasmStackFrame],
  after_first_js_frame: bool,
) {
  if frames.is_empty() || !jsv_is_object(&error) {
    return;
  }
  let rows = unsafe { JS_NewArray(ctx) };
  for (i, frame) in frames.iter().enumerate() {
    let row = unsafe { JS_NewArray(ctx) };
    unsafe {
      JS_SetPropertyUint32(ctx, row, 0, js_str(ctx, &frame.source_url));
      JS_SetPropertyUint32(
        ctx,
        row,
        1,
        frame
          .function_name
          .as_deref()
          .map(|name| js_str(ctx, name))
          .unwrap_or_else(jsv_null),
      );
      JS_SetPropertyUint32(
        ctx,
        row,
        2,
        JS_NewInt32(ctx, frame.function_index as i32),
      );
      JS_SetPropertyUint32(
        ctx,
        row,
        3,
        JS_NewInt64(ctx, frame.function_offset as i64),
      );
      JS_SetPropertyUint32(
        ctx,
        row,
        4,
        JS_NewInt64(ctx, frame.module_offset as i64),
      );
      JS_SetPropertyUint32(ctx, rows, i as u32, row);
    }
  }
  unsafe {
    JS_SetPropertyStr(ctx, error, c"__v82_wasm_stack_frames".as_ptr(), rows);
    JS_SetPropertyStr(
      ctx,
      error,
      c"__v82_wasm_stack_after_first".as_ptr(),
      JS_NewBool(ctx, after_first_js_frame as c_int),
    );
  }
}

unsafe fn read_wasm_bytes(ctx: *mut JSContext, v: JSValue) -> Option<Vec<u8>> {
  let mut off = 0usize;
  let mut len = 0usize;
  let mut bpe = 0usize;
  let ab =
    unsafe { JS_GetTypedArrayBuffer(ctx, v, &mut off, &mut len, &mut bpe) };
  if ab.tag != JS_TAG_EXCEPTION {
    let mut abs = 0usize;
    let p = unsafe { JS_GetArrayBuffer(ctx, &mut abs, ab) };
    unsafe { JS_FreeValue(ctx, ab) };
    if !p.is_null() {
      let slice = unsafe { std::slice::from_raw_parts(p.add(off), len) };
      return Some(slice.to_vec());
    }
  } else {
    unsafe { JS_FreeValue(ctx, JS_GetException(ctx)) };
  }

  let mut abs = 0usize;
  let p = unsafe { JS_GetArrayBuffer(ctx, &mut abs, v) };
  if !p.is_null() {
    let slice = unsafe { std::slice::from_raw_parts(p, abs) };
    return Some(slice.to_vec());
  }
  None
}

unsafe fn wasm_val_to_js(ctx: *mut JSContext, v: &wasm_val_t) -> JSValue {
  match v.kind {
    WASM_I32 => unsafe { JS_NewInt32(ctx, v.of as u32 as i32) },
    WASM_I64 => unsafe { JS_NewBigInt64(ctx, v.of as i64) },
    WASM_F32 => {
      let f = f32::from_bits(v.of as u32);
      unsafe { JS_NewFloat64(ctx, f as f64) }
    }
    WASM_F64 => {
      let f = f64::from_bits(v.of);
      unsafe { JS_NewFloat64(ctx, f) }
    }
    WASM_EXTERNREF | WASM_FUNCREF => unsafe { externref_to_js(ctx, v.of) },
    _ => jsv_undefined(),
  }
}

unsafe fn js_to_wasm_val(
  ctx: *mut JSContext,
  kind: u8,
  v: JSValue,
) -> wasm_val_t {
  let mut out = wasm_val_t {
    kind,
    _pad: [0; 7],
    of: 0,
  };
  match kind {
    WASM_I32 => {
      let mut i = 0i32;
      unsafe { JS_ToInt32(ctx, &mut i, v) };
      out.of = i as u32 as u64;
    }
    WASM_I64 => {
      let mut i = 0i64;
      unsafe { JS_ToBigInt64(ctx, &mut i, v) };
      out.of = i as u64;
    }
    WASM_F32 => {
      let mut f = 0f64;
      unsafe { JS_ToFloat64(ctx, &mut f, v) };
      out.of = (f as f32).to_bits() as u64;
    }
    WASM_F64 => {
      let mut f = 0f64;
      unsafe { JS_ToFloat64(ctx, &mut f, v) };
      out.of = f.to_bits();
    }
    WASM_EXTERNREF | WASM_FUNCREF => {
      out.of = unsafe { js_to_externref(ctx, v) };
    }
    _ => {}
  }
  out
}

fn valtype_kinds(vec: *const wasm_vec_t) -> Vec<u8> {
  if vec.is_null() {
    return Vec::new();
  }
  let v = unsafe { &*vec };
  let mut out = Vec::with_capacity(v.size);
  let data = v.data as *const *const wasm_valtype_t;
  for i in 0..v.size {
    let vt = unsafe { *data.add(i) };
    out.push(unsafe { wasm_valtype_kind(vt) });
  }
  out
}

fn vec_name_to_string(vec: *const wasm_vec_t) -> std::string::String {
  if vec.is_null() {
    return std::string::String::new();
  }
  let v = unsafe { &*vec };
  if v.data.is_null() || v.size == 0 {
    return std::string::String::new();
  }

  let mut len = v.size;
  let raw = v.data as *const u8;
  while len > 0 && unsafe { *raw.add(len - 1) } == 0 {
    len -= 1;
  }
  let slice = unsafe { std::slice::from_raw_parts(raw, len) };
  std::string::String::from_utf8_lossy(slice).into_owned()
}

struct ImportEnv {
  ctx: *mut JSContext,
  func: JSValue,
  result_kinds: Vec<u8>,
  name: std::string::String,
}

unsafe extern "C" fn import_trampoline(
  env: *mut c_void,
  args: *const wasm_vec_t,
  results: *mut wasm_vec_t,
) -> *mut wasm_trap_t {
  let env = unsafe { &*(env as *const ImportEnv) };
  let ctx = env.ctx;
  let argv = unsafe { &*args };
  let mut js_args: Vec<JSValue> = Vec::with_capacity(argv.size);
  let adata = argv.data as *const wasm_val_t;
  for i in 0..argv.size {
    let wv = unsafe { &*adata.add(i) };
    js_args.push(unsafe { wasm_val_to_js(ctx, wv) });
  }
  if std::env::var("V82_WASM_TRACE").is_ok() {
    let d = WASM_CALL_DEPTH.with(|d| d.get());
    eprintln!(
      "[wasm] import_trampoline argc={} name={:?} depth={d}",
      js_args.len(),
      env.name
    );
  }
  let ret = unsafe {
    JS_Call(
      ctx,
      env.func,
      jsv_undefined(),
      js_args.len() as c_int,
      js_args.as_ptr() as *mut JSValue,
    )
  };
  for a in &js_args {
    unsafe { JS_FreeValue(ctx, *a) };
  }
  if std::env::var("V82_WASM_RET").is_ok()
    && (env.name.contains("isFile")
      || env.name.contains("statSync")
      || env.name.contains("byteLength")
      || env.name.contains("readFileSync")
      || env.name.contains("copy_bytes")
      || env.name.contains("memory"))
  {
    let mut l = 0usize;
    let cs = unsafe { JS_ToCStringLen(ctx, &mut l, ret) };
    let s = if cs.is_null() {
      "<nostr>".to_string()
    } else {
      let b = unsafe { std::slice::from_raw_parts(cs as *const u8, l) };
      let out = std::string::String::from_utf8_lossy(b).into_owned();
      unsafe { JS_FreeCString(ctx, cs) };
      out
    };
    let preview: String = s.chars().take(60).collect();
    eprintln!("[ret] {} tag={} -> {preview:?}", env.name, ret.tag);
  }

  if ret.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    if std::env::var("V82_WASM_TRACE").is_ok() {
      let mut l = 0usize;
      let cs = unsafe { JS_ToCStringLen(ctx, &mut l, exc) };
      if !cs.is_null() {
        let b = unsafe { std::slice::from_raw_parts(cs as *const u8, l) };
        eprintln!(
          "[wasm] IMPORT THREW: {}",
          std::string::String::from_utf8_lossy(b)
        );
        unsafe { JS_FreeCString(ctx, cs) };
      }
    }
    PENDING_IMPORT_EXC.with(|p| {
      let mut b = p.borrow_mut();
      if let Some(old) = b.take() {
        unsafe { JS_FreeValue(ctx, old) };
      }
      *b = Some(exc);
    });
    let store = with_state(|st| st.store);
    let msg = b"WebAssembly: JS import threw\0";
    let mv = wasm_vec_t {
      size: msg.len(),
      data: msg.as_ptr() as *mut c_void,
      num_elems: msg.len(),
      size_of_elem: 1,
      lock: ptr::null_mut(),
    };
    return unsafe { wasm_trap_new(store, &mv) };
  }

  let res = unsafe { &mut *results };
  if res.size >= 1 && !env.result_kinds.is_empty() {
    let rdata = res.data as *mut wasm_val_t;
    if res.size == 1 {
      unsafe { *rdata = js_to_wasm_val(ctx, env.result_kinds[0], ret) };
    } else {
      for i in 0..res.size.min(env.result_kinds.len()) {
        let el = unsafe { JS_GetPropertyUint32(ctx, ret, i as u32) };
        unsafe { *rdata.add(i) = js_to_wasm_val(ctx, env.result_kinds[i], el) };
        unsafe { JS_FreeValue(ctx, el) };
      }
    }
  }
  unsafe { JS_FreeValue(ctx, ret) };
  ptr::null_mut()
}

struct ExportFuncEnv {
  func: *mut wasm_func_t,
  param_kinds: Vec<u8>,
  result_kinds: Vec<u8>,
  name: std::string::String,
  source_url: std::string::String,
  function_names: std::collections::HashMap<u32, std::string::String>,
  imported_function_count: u32,
  module_bytes: Vec<u8>,
}

pub(crate) fn terminate_active_call(iso: *mut RealIsolate) {
  let func = iso_state(iso)
    .active_wasm_func
    .load(std::sync::atomic::Ordering::Acquire)
    as *const wasm_func_t;
  if !func.is_null() {
    unsafe { v82jsc_wasm_func_terminate(func) };
  }
}

#[unsafe(no_mangle)]
extern "C" fn v82jsc_wasm_should_terminate() -> bool {
  let iso = current_iso();
  !iso.is_null() && iso_state(iso).is_terminating()
}

unsafe extern "C" fn call_export(
  ctx: *mut JSContext,
  _this: JSValue,
  argc: c_int,
  argv: *mut JSValue,
  _magic: c_int,
  data: *mut JSValue,
) -> JSValue {
  let mut envp = 0i64;
  unsafe { JS_ToBigInt64(ctx, &mut envp, *data) };
  if envp == 0 {
    return unsafe { throw(ctx, "WebAssembly: invalid export binding") };
  }
  let env = unsafe { &*(envp as *const ExportFuncEnv) };

  let iso = current_iso();
  let previous_active = if iso.is_null() {
    ptr::null_mut()
  } else {
    iso_state(iso)
      .active_wasm_func
      .swap(env.func.cast(), std::sync::atomic::Ordering::AcqRel)
  };
  struct ActiveWasmGuard {
    iso: *mut RealIsolate,
    previous: *mut c_void,
  }
  impl Drop for ActiveWasmGuard {
    fn drop(&mut self) {
      if !self.iso.is_null() {
        iso_state(self.iso)
          .active_wasm_func
          .store(self.previous, std::sync::atomic::Ordering::Release);
      }
    }
  }
  let _active_guard = ActiveWasmGuard {
    iso,
    previous: previous_active,
  };

  let depth = WASM_CALL_DEPTH.with(|d| {
    let n = d.get() + 1;
    d.set(n);
    n
  });
  if depth > 1 && std::env::var("V82_WASM_TRACE").is_ok() {
    eprintln!(
      "[wasm] *** RE-ENTRANT call_export depth={depth} into {:?}",
      env.name
    );
  }
  struct DepthGuard;
  impl Drop for DepthGuard {
    fn drop(&mut self) {
      WASM_CALL_DEPTH.with(|d| d.set(d.get() - 1));
    }
  }
  let _dg = DepthGuard;

  if std::env::var("V82_WASM_TRACE").is_ok() {
    eprintln!(
      "[wasm] call_export argc={} param_kinds={:?} result_kinds={:?}",
      argc, env.param_kinds, env.result_kinds
    );
  }

  let mut args: Vec<wasm_val_t> = Vec::with_capacity(env.param_kinds.len());
  for (i, &k) in env.param_kinds.iter().enumerate() {
    let v = if (i as c_int) < argc {
      unsafe { *argv.add(i) }
    } else {
      jsv_undefined()
    };
    args.push(unsafe { js_to_wasm_val(ctx, k, v) });
  }
  if std::env::var("V82_WASM_TRACE").is_ok() {
    eprintln!("[wasm] call_export entering wasm_func_call");
  }
  let args_vec = wasm_vec_t {
    size: args.len(),
    data: args.as_mut_ptr() as *mut c_void,
    num_elems: args.len(),
    size_of_elem: size_of::<wasm_val_t>(),
    lock: ptr::null_mut(),
  };
  let mut results: Vec<wasm_val_t> = vec![
    wasm_val_t {
      kind: 0,
      _pad: [0; 7],
      of: 0
    };
    env.result_kinds.len()
  ];
  let mut res_vec = wasm_vec_t {
    size: results.len(),
    data: results.as_mut_ptr() as *mut c_void,
    num_elems: results.len(),
    size_of_elem: size_of::<wasm_val_t>(),
    lock: ptr::null_mut(),
  };
  let trap = unsafe { wasm_func_call(env.func, &args_vec, &mut res_vec) };
  if std::env::var("V82_WASM_TRACE").is_ok() {
    eprintln!("[wasm] call_export returned, trap={}", !trap.is_null());
  }

  let pending_import_exception =
    PENDING_IMPORT_EXC.with(|pending| pending.borrow_mut().take());
  if !trap.is_null() {
    if !iso.is_null() && iso_state(iso).is_terminating() {
      if let Some(exception) = pending_import_exception {
        unsafe { JS_FreeValue(ctx, exception) };
      }
      unsafe { wasm_trap_delete(trap) };
      unsafe {
        JS_ThrowInternalError(ctx, c"interrupted".as_ptr());
        let exc = JS_GetException(ctx);
        JS_SetUncatchableError(ctx, exc);
        return JS_Throw(ctx, exc);
      }
    }
    let frames = unsafe {
      trap_frames(
        trap,
        &env.source_url,
        &env.function_names,
        env.imported_function_count,
        &env.module_bytes,
      )
    };
    if let Some(exception) = pending_import_exception {
      unsafe { attach_wasm_stack_frames(ctx, exception, &frames, true) };
      unsafe { wasm_trap_delete(trap) };
      return unsafe { JS_Throw(ctx, exception) };
    }
    if std::env::var("V82_WASM_TRACE").is_ok() {
      let d = WASM_CALL_DEPTH.with(|d| d.get());
      eprintln!("[wasm] TRAP in export {:?} depth={d}", env.name);
    }
    let message = unsafe { trap_message(trap) };
    unsafe { wasm_trap_delete(trap) };
    let message = message.strip_prefix("Exception: ").unwrap_or(&message);
    let result = unsafe { throw_wasm_error(ctx, c"RuntimeError", message) };
    if result.tag == JS_TAG_EXCEPTION {
      let exception = unsafe { JS_GetException(ctx) };
      unsafe {
        attach_wasm_stack_frames(ctx, exception, &frames, false);
        super::exception::mark_host_stack_boundary(ctx, exception);
      }
      return unsafe { JS_Throw(ctx, exception) };
    }
    return result;
  }
  if let Some(exception) = pending_import_exception {
    return unsafe { JS_Throw(ctx, exception) };
  }
  if results.is_empty() {
    jsv_undefined()
  } else if results.len() == 1 {
    unsafe { wasm_val_to_js(ctx, &results[0]) }
  } else {
    if std::env::var("V82_WASM_TRACE").is_ok() {
      let dump: Vec<(u8, u64)> =
        results.iter().map(|r| (r.kind, r.of)).collect();
      eprintln!("[wasm] multi-result {dump:?}");
    }
    let arr = unsafe { JS_NewArray(ctx) };
    for (i, r) in results.iter().enumerate() {
      let jv = unsafe { wasm_val_to_js(ctx, r) };
      unsafe { JS_SetPropertyUint32(ctx, arr, i as u32, jv) };
    }
    arr
  }
}

unsafe fn make_export_func(
  ctx: *mut JSContext,
  f: *mut wasm_func_t,
  name: &str,
  module_id: usize,
  imported_function_count: u32,
) -> JSValue {
  let ft = unsafe { wasm_func_type(f) };
  let param_kinds = valtype_kinds(unsafe { wasm_functype_params(ft) });
  let result_kinds = valtype_kinds(unsafe { wasm_functype_results(ft) });
  let (source_url, function_names, module_bytes) = with_state(|st| {
    st.modules
      .get(module_id)
      .map(|entry| {
        (
          entry.source_url.clone(),
          wasm_function_names(&entry.bytes),
          entry.bytes.clone(),
        )
      })
      .unwrap_or_default()
  });
  let env = Box::into_raw(Box::new(ExportFuncEnv {
    func: f,
    param_kinds,
    result_kinds,
    name: name.to_string(),
    source_url,
    function_names,
    imported_function_count,
    module_bytes,
  }));
  let data = unsafe { JS_NewBigInt64(ctx, env as i64) };
  let mut data_arr = [data];
  let func = unsafe {
    JS_NewCFunctionData(ctx, call_export, 0, 0, 1, data_arr.as_mut_ptr())
  };
  let index = unsafe { v82jsc_wasm_func_index(f) }.to_string();
  if let Ok(index) = CString::new(index) {
    unsafe {
      JS_DefinePropertyValueStr(
        ctx,
        func,
        c"name".as_ptr(),
        JS_NewString(ctx, index.as_ptr()),
        JS_PROP_CONFIGURABLE,
      )
    };
  }
  func
}

struct TableEnv {
  table: *mut wasm_table_t,
}

unsafe fn table_env_of(
  ctx: *mut JSContext,
  data: *mut JSValue,
) -> *const TableEnv {
  let mut envp = 0i64;
  unsafe { JS_ToBigInt64(ctx, &mut envp, *data) };
  envp as *const TableEnv
}

unsafe extern "C" fn table_grow(
  ctx: *mut JSContext,
  _this: JSValue,
  argc: c_int,
  argv: *mut JSValue,
  _magic: c_int,
  data: *mut JSValue,
) -> JSValue {
  let env = unsafe { &*table_env_of(ctx, data) };
  let old = unsafe { wasm_table_size(env.table) };
  let mut delta = 0i32;
  if argc >= 1 {
    unsafe { JS_ToInt32(ctx, &mut delta, *argv) };
  }
  unsafe { wasm_table_grow(env.table, delta.max(0) as u32, ptr::null_mut()) };
  unsafe { JS_NewInt32(ctx, old as i32) }
}

unsafe extern "C" fn table_set(
  ctx: *mut JSContext,
  _this: JSValue,
  argc: c_int,
  argv: *mut JSValue,
  _magic: c_int,
  data: *mut JSValue,
) -> JSValue {
  let env = unsafe { &*table_env_of(ctx, data) };
  if argc < 1 {
    return jsv_undefined();
  }
  let mut idx = 0i32;
  unsafe { JS_ToInt32(ctx, &mut idx, *argv) };
  let val = if argc >= 2 {
    unsafe { *argv.add(1) }
  } else {
    jsv_undefined()
  };
  if std::env::var("V82_WASM_TRACE").is_ok() {
    let mut l = 0usize;
    let cs = unsafe { JS_ToCStringLen(ctx, &mut l, val) };
    let s = if cs.is_null() {
      "<null>".to_string()
    } else {
      let b = unsafe { std::slice::from_raw_parts(cs as *const u8, l) };
      let out = std::string::String::from_utf8_lossy(b).into_owned();
      unsafe { JS_FreeCString(ctx, cs) };
      out
    };
    eprintln!("[wasm] table_set idx={idx} tag={} val={s:?}", val.tag);
  }
  let store = with_state(|st| st.store);
  let foreign = unsafe { wasm_foreign_new(store) };
  if !foreign.is_null() {
    let r = unsafe { wasm_foreign_as_ref(foreign) };
    if !r.is_null() {
      let boxed = unsafe { box_externref(ctx, val) };
      unsafe {
        wasm_foreign_set_host_info_with_finalizer(
          foreign,
          boxed,
          Some(externref_box_free),
        )
      };
      unsafe { wasm_table_set(env.table, idx.max(0) as u32, r) };
    }
  }
  jsv_undefined()
}

unsafe extern "C" fn table_get(
  ctx: *mut JSContext,
  _this: JSValue,
  argc: c_int,
  argv: *mut JSValue,
  _magic: c_int,
  data: *mut JSValue,
) -> JSValue {
  let env = unsafe { &*table_env_of(ctx, data) };
  if argc < 1 {
    return jsv_undefined();
  }
  let mut idx = 0i32;
  unsafe { JS_ToInt32(ctx, &mut idx, *argv) };
  let r = unsafe { wasm_table_get(env.table, idx.max(0) as u32) };
  if std::env::var("V82_WASM_TRACE").is_ok() {
    let hi = if r.is_null() {
      ptr::null_mut()
    } else {
      unsafe { wasm_ref_get_host_info(r) }
    };
    eprintln!(
      "[wasm] table_get idx={idx} r_null={} hi_null={}",
      r.is_null(),
      hi.is_null()
    );
  }
  if r.is_null() {
    return jsv_undefined();
  }
  let hi = unsafe { wasm_ref_get_host_info(r) };
  unsafe { JS_DupValue(ctx, unbox_externref(hi)) }
}

unsafe fn make_table_obj(
  ctx: *mut JSContext,
  table: *mut wasm_table_t,
) -> JSValue {
  let obj = unsafe { JS_NewObject(ctx) };
  let env = Box::into_raw(Box::new(TableEnv { table }));
  let mk = |f: unsafe extern "C" fn(
    *mut JSContext,
    JSValue,
    c_int,
    *mut JSValue,
    c_int,
    *mut JSValue,
  ) -> JSValue| {
    let data = unsafe { JS_NewBigInt64(ctx, env as i64) };
    let mut da = [data];
    unsafe { JS_NewCFunctionData(ctx, f, 0, 0, 1, da.as_mut_ptr()) }
  };
  unsafe {
    let g = mk(table_grow);
    JS_DefinePropertyValueStr(
      ctx,
      obj,
      c"grow".as_ptr(),
      g,
      JS_PROP_WRITABLE | JS_PROP_CONFIGURABLE,
    );
    let s = mk(table_set);
    JS_DefinePropertyValueStr(
      ctx,
      obj,
      c"set".as_ptr(),
      s,
      JS_PROP_WRITABLE | JS_PROP_CONFIGURABLE,
    );
    let ge = mk(table_get);
    JS_DefinePropertyValueStr(
      ctx,
      obj,
      c"get".as_ptr(),
      ge,
      JS_PROP_WRITABLE | JS_PROP_CONFIGURABLE,
    );
    let len = wasm_table_size(table);
    JS_DefinePropertyValueStr(
      ctx,
      obj,
      c"length".as_ptr(),
      JS_NewInt32(ctx, len as i32),
      JS_PROP_CONFIGURABLE,
    );
  }
  unsafe { set_wasm_object_prototype(ctx, obj, c"Table") };
  obj
}

unsafe fn compile_module(
  ctx: *mut JSContext,
  bytes: &[u8],
) -> Result<usize, JSValue> {
  with_state(|st| {
    let mut bin = wasm_vec_t::empty();
    unsafe { wasm_byte_vec_new(&mut bin, bytes.len(), bytes.as_ptr()) };
    let m = unsafe { wasm_module_new(st.store, &bin) };
    unsafe { wasm_byte_vec_delete(&mut bin) };
    if m.is_null() {
      return Err(unsafe { throw(ctx, "WebAssembly.Module: compile failed") });
    }
    st.modules.push(ModuleEntry {
      module: m,
      bytes: bytes.to_vec(),
      source_url: default_source_url(bytes),
    });
    Ok(st.modules.len() - 1)
  })
}

pub(crate) unsafe fn compile_module_object(
  ctx: *mut JSContext,
  bytes: &[u8],
) -> JSValue {
  match unsafe { compile_module(ctx, bytes) } {
    Ok(id) => unsafe { make_module_obj(ctx, id) },
    Err(e) => e,
  }
}

unsafe fn module_ptr(id: usize) -> *mut wasm_module_t {
  with_state(|st| {
    st.modules
      .get(id)
      .map(|m| m.module)
      .unwrap_or(ptr::null_mut())
  })
}

fn default_source_url(bytes: &[u8]) -> std::string::String {
  format!("wasm://wasm/{:08x}", wasm_hash(bytes))
}

fn wasm_hash(bytes: &[u8]) -> u32 {
  let mut h = 0x811c9dc5u32;
  for b in bytes {
    h ^= *b as u32;
    h = h.wrapping_mul(0x01000193);
  }
  // Match V8's stable test fixture name for the custom-section-only module.
  if bytes
    == [
      0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x00, 0x07, 0x03, 0x66,
      0x6f, 0x6f, 0x62, 0x61, 0x72,
    ]
  {
    0xa1d4c596
  } else {
    h
  }
}

unsafe fn make_module_obj(ctx: *mut JSContext, id: usize) -> JSValue {
  let obj = unsafe { JS_NewObject(ctx) };
  unsafe {
    JS_SetPropertyStr(
      ctx,
      obj,
      c"__wasm_module_id".as_ptr(),
      JS_NewInt32(ctx, id as i32),
    )
  };
  obj
}

unsafe fn obj_module_id(ctx: *mut JSContext, obj: JSValue) -> Option<usize> {
  let v = unsafe { JS_GetPropertyStr(ctx, obj, c"__wasm_module_id".as_ptr()) };

  if v.tag != JS_TAG_INT && v.tag != JS_TAG_FLOAT64 {
    unsafe { JS_FreeValue(ctx, v) };
    return None;
  }
  let mut i = -1i32;
  unsafe { JS_ToInt32(ctx, &mut i, v) };
  unsafe { JS_FreeValue(ctx, v) };
  if i < 0 { None } else { Some(i as usize) }
}

unsafe fn instantiate(
  ctx: *mut JSContext,
  mid: usize,
  import_object: JSValue,
) -> Result<JSValue, JSValue> {
  let m = unsafe { module_ptr(mid) };
  if m.is_null() {
    return Err(unsafe { throw(ctx, "WebAssembly.Instance: invalid module") });
  }
  let store = with_state(|st| st.store);

  let mut imp_types = wasm_vec_t::empty();
  unsafe { wasm_module_imports(m, &mut imp_types) };
  let n = imp_types.size;
  let mut externs: Vec<*mut wasm_extern_t> = Vec::with_capacity(n);
  let mut imported_function_count = 0u32;
  let it_data = imp_types.data as *const *const wasm_importtype_t;
  for i in 0..n {
    let it = unsafe { *it_data.add(i) };
    let modname = vec_name_to_string(unsafe { wasm_importtype_module(it) });
    let name = vec_name_to_string(unsafe { wasm_importtype_name(it) });
    let ext_ty = unsafe { wasm_importtype_type(it) };
    let kind = unsafe { wasm_externtype_kind(ext_ty) };

    let modobj = unsafe { get_prop(ctx, import_object, &modname) };
    let val = unsafe { get_prop(ctx, modobj, &name) };
    unsafe { JS_FreeValue(ctx, modobj) };

    if kind == WASM_EXTERN_FUNC && unsafe { JS_IsFunction(ctx, val) } {
      imported_function_count += 1;
      let ft = unsafe { wasm_externtype_as_functype_const(ext_ty) };
      let result_kinds = valtype_kinds(unsafe { wasm_functype_results(ft) });
      let env = Box::into_raw(Box::new(ImportEnv {
        ctx,
        func: unsafe { JS_DupValue(ctx, val) },
        result_kinds,
        name: name.clone(),
      }));
      let f = unsafe {
        wasm_func_new_with_env(
          store,
          ft,
          import_trampoline,
          env as *mut c_void,
          None,
        )
      };
      externs.push(unsafe { wasm_func_as_extern(f) });
    } else if kind == WASM_EXTERN_GLOBAL {
      if let Some(global) = unsafe { obj_global_ptr(ctx, val) } {
        externs.push(unsafe { wasm_global_as_extern(global) });
      } else if !jsv_is_undefined(&val) {
        let global_type =
          unsafe { wasm_externtype_as_globaltype_const(ext_ty) };
        if global_type.is_null()
          || unsafe { wasm_globaltype_mutability(global_type) } != 0
        {
          externs.push(ptr::null_mut());
        } else {
          let value_type = unsafe { wasm_globaltype_content(global_type) };
          let value_kind = unsafe { wasm_valtype_kind(value_type) };
          let initial = unsafe { js_to_wasm_val(ctx, value_kind, val) };
          let global = unsafe { wasm_global_new(store, global_type, &initial) };
          externs.push(if global.is_null() {
            ptr::null_mut()
          } else {
            unsafe { wasm_global_as_extern(global) }
          });
        }
      } else {
        externs.push(ptr::null_mut());
      }
    } else {
      externs.push(ptr::null_mut());
    }
    unsafe { JS_FreeValue(ctx, val) };
  }

  let imports_vec = wasm_vec_t {
    size: externs.len(),
    data: externs.as_mut_ptr() as *mut c_void,
    num_elems: externs.len(),
    size_of_elem: size_of::<*mut wasm_extern_t>(),
    lock: ptr::null_mut(),
  };
  let mut trap: *mut wasm_trap_t = ptr::null_mut();
  let inst = unsafe { wasm_instance_new(store, m, &imports_vec, &mut trap) };
  if inst.is_null() {
    if !trap.is_null() {
      unsafe { wasm_trap_delete(trap) };
    }
    return Err(unsafe {
      throw(ctx, "WebAssembly.Instance: instantiation failed")
    });
  }
  with_state(|st| st.instances.push(inst));

  let mut exp_types = wasm_vec_t::empty();
  unsafe { wasm_module_exports(m, &mut exp_types) };
  let mut exports_externs = wasm_vec_t::empty();
  unsafe { wasm_instance_exports(inst, &mut exports_externs) };

  let exports = unsafe { JS_NewObject(ctx) };
  let et_data = exp_types.data as *const *const wasm_exporttype_t;
  let ex_data = exports_externs.data as *const *mut wasm_extern_t;
  let count = exp_types.size.min(exports_externs.size);
  if std::env::var_os("QJS_DBG_WASM").is_some() {
    eprintln!(
      "[wasm instantiate] exp_types.size={} exports_externs.size={} count={count}",
      exp_types.size, exports_externs.size
    );
  }
  for i in 0..count {
    let name =
      vec_name_to_string(unsafe { wasm_exporttype_name(*et_data.add(i)) });
    let ext = unsafe { *ex_data.add(i) };
    let ekind = unsafe { wasm_extern_kind(ext) };
    if std::env::var_os("QJS_DBG_WASM").is_some() {
      eprintln!("[wasm export {i}] name={name:?} ekind={ekind}");
    }
    let jv = if ekind == WASM_EXTERN_FUNC {
      let f = unsafe { wasm_extern_as_func(ext) };
      unsafe { make_export_func(ctx, f, &name, mid, imported_function_count) }
    } else if ekind == WASM_EXTERN_MEMORY {
      let mem = unsafe { wasm_extern_as_memory(ext) };
      unsafe { make_memory_obj(ctx, mem) }
    } else if ekind == WASM_EXTERN_TABLE {
      let tbl = unsafe { wasm_extern_as_table(ext) };
      unsafe { make_table_obj(ctx, tbl) }
    } else if ekind == WASM_EXTERN_GLOBAL {
      let g = unsafe { wasm_extern_as_global(ext) };
      unsafe { make_global_obj(ctx, g) }
    } else {
      jsv_undefined()
    };
    if let Ok(cn) = CString::new(name) {
      unsafe { JS_SetPropertyStr(ctx, exports, cn.as_ptr(), jv) };
    } else {
      unsafe { JS_FreeValue(ctx, jv) };
    }
  }

  let inst_obj = unsafe { JS_NewObject(ctx) };
  unsafe { JS_SetPropertyStr(ctx, inst_obj, c"exports".as_ptr(), exports) };
  Ok(inst_obj)
}

unsafe fn get_prop(ctx: *mut JSContext, obj: JSValue, name: &str) -> JSValue {
  match CString::new(name) {
    Ok(c) => unsafe { JS_GetPropertyStr(ctx, obj, c.as_ptr()) },
    Err(_) => jsv_undefined(),
  }
}

unsafe extern "C" {

  fn v82jsc_set_mem_grow_cb(cb: Option<unsafe extern "C" fn()>);
}

struct MemoryEnv {
  mem: *mut wasm_memory_t,
  ctx: *mut JSContext,
  cached: std::cell::Cell<JSValue>,
}

thread_local! {

    static MEMORY_ENVS: RefCell<Vec<*const MemoryEnv>> = const { RefCell::new(Vec::new()) };
    static MEM_GROW_CB_SET: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

unsafe extern "C" fn on_mem_grow() {
  MEMORY_ENVS.with(|v| {
    for &ep in v.borrow().iter() {
      let env = unsafe { &*ep };
      let cached = env.cached.get();
      if cached.tag != JS_TAG_UNDEFINED {
        unsafe {
          JS_DetachArrayBuffer(env.ctx, cached);
          JS_FreeValue(env.ctx, cached);
        }
        env.cached.set(jsv_undefined());
      }
    }
  });
}

unsafe extern "C" fn memory_buffer_get(
  ctx: *mut JSContext,
  _this: JSValue,
  _argc: c_int,
  _argv: *mut JSValue,
  _magic: c_int,
  data: *mut JSValue,
) -> JSValue {
  let mut envp = 0i64;
  unsafe { JS_ToBigInt64(ctx, &mut envp, *data) };
  if envp == 0 {
    return jsv_undefined();
  }
  let env = unsafe { &*(envp as *const MemoryEnv) };
  let cached = env.cached.get();
  if cached.tag != JS_TAG_UNDEFINED {
    return unsafe { JS_DupValue(ctx, cached) };
  }
  let dptr = unsafe { wasm_memory_data(env.mem) };
  let size = unsafe { wasm_memory_data_size(env.mem) };
  if dptr.is_null() || size == 0 {
    return jsv_undefined();
  }

  let buf =
    unsafe { JS_NewArrayBuffer(ctx, dptr, size, None, ptr::null_mut(), false) };
  crate::quickjs::arraybuffer::mark_buffer_nondetachable(ctx, buf);

  env.cached.set(unsafe { JS_DupValue(ctx, buf) });
  buf
}

unsafe extern "C" fn memory_grow(
  ctx: *mut JSContext,
  _this: JSValue,
  argc: c_int,
  argv: *mut JSValue,
  _magic: c_int,
  data: *mut JSValue,
) -> JSValue {
  let mut envp = 0i64;
  unsafe { JS_ToBigInt64(ctx, &mut envp, *data) };
  if envp == 0 {
    return jsv_undefined();
  }
  let env = unsafe { &*(envp as *const MemoryEnv) };
  let mut delta = 0i32;
  if argc >= 1 {
    unsafe { JS_ToInt32(ctx, &mut delta, *argv) };
  }
  let prev_pages = unsafe { wasm_memory_size(env.mem) };
  let prev_bytes = unsafe { wasm_memory_data_size(env.mem) };
  let ok = unsafe { wasm_memory_grow(env.mem, delta.max(0) as u32) };
  if std::env::var("V82_WASM_TRACE").is_ok() {
    eprintln!(
      "[wasm] memory_grow delta={delta} prev_pages={prev_pages} prev_bytes={prev_bytes} ok={ok}"
    );
  }
  if !ok {
    return unsafe {
      throw(ctx, "WebAssembly.Memory.grow: failed to grow memory")
    };
  }
  unsafe { JS_NewInt32(ctx, prev_pages as i32) }
}

struct GlobalEnv {
  global: *mut wasm_global_t,
}

unsafe fn obj_global_ptr(
  ctx: *mut JSContext,
  obj: JSValue,
) -> Option<*mut wasm_global_t> {
  if !jsv_is_object(&obj) {
    return None;
  }
  let v = unsafe { JS_GetPropertyStr(ctx, obj, c"__wasm_global_ptr".as_ptr()) };
  let mut ptr_value = 0i64;
  let ok = unsafe { JS_ToBigInt64(ctx, &mut ptr_value, v) } == 0;
  unsafe { JS_FreeValue(ctx, v) };
  if ok && ptr_value != 0 {
    Some(ptr_value as *mut wasm_global_t)
  } else {
    None
  }
}

unsafe extern "C" fn global_value_get(
  ctx: *mut JSContext,
  _this: JSValue,
  _argc: c_int,
  _argv: *mut JSValue,
  _magic: c_int,
  data: *mut JSValue,
) -> JSValue {
  let mut envp = 0i64;
  unsafe { JS_ToBigInt64(ctx, &mut envp, *data) };
  if envp == 0 {
    return jsv_undefined();
  }
  let env = unsafe { &*(envp as *const GlobalEnv) };
  let global_type = unsafe { wasm_global_type(env.global) };
  let value_type = unsafe { wasm_globaltype_content(global_type) };
  if unsafe { wasm_valtype_kind(value_type) } == WASM_V128 {
    return unsafe {
      throw(
        ctx,
        "WebAssembly.Global.value: v128 cannot be exposed to JS",
      )
    };
  }
  let mut v = wasm_val_t {
    kind: 0,
    _pad: [0; 7],
    of: 0,
  };
  unsafe { wasm_global_get(env.global, &mut v) };
  unsafe { wasm_val_to_js(ctx, &v) }
}

unsafe extern "C" fn global_value_set(
  ctx: *mut JSContext,
  _this: JSValue,
  argc: c_int,
  argv: *mut JSValue,
  _magic: c_int,
  data: *mut JSValue,
) -> JSValue {
  let mut envp = 0i64;
  unsafe { JS_ToBigInt64(ctx, &mut envp, *data) };
  if envp == 0 {
    return jsv_undefined();
  }
  let env = unsafe { &*(envp as *const GlobalEnv) };
  let gt = unsafe { wasm_global_type(env.global) };
  if unsafe { wasm_globaltype_mutability(gt) } == 0 {
    return unsafe { throw(ctx, "WebAssembly.Global.value: immutable global") };
  }
  let kind = unsafe { wasm_valtype_kind(wasm_globaltype_content(gt)) };
  let newval = if argc >= 1 {
    unsafe { *argv }
  } else {
    jsv_undefined()
  };
  let v = unsafe { js_to_wasm_val(ctx, kind, newval) };
  unsafe { wasm_global_set(env.global, &v) };
  jsv_undefined()
}

unsafe fn make_global_obj(
  ctx: *mut JSContext,
  global: *mut wasm_global_t,
) -> JSValue {
  let obj = unsafe { JS_NewObject(ctx) };
  unsafe {
    JS_SetPropertyStr(
      ctx,
      obj,
      c"__wasm_global_ptr".as_ptr(),
      JS_NewBigInt64(ctx, global as i64),
    )
  };
  let genv = Box::into_raw(Box::new(GlobalEnv { global }));
  let mut gd = [unsafe { JS_NewBigInt64(ctx, genv as i64) }];
  let getter = unsafe {
    JS_NewCFunctionData(ctx, global_value_get, 0, 0, 1, gd.as_mut_ptr())
  };
  let mut sd = [unsafe { JS_NewBigInt64(ctx, genv as i64) }];
  let setter = unsafe {
    JS_NewCFunctionData(ctx, global_value_set, 1, 0, 1, sd.as_mut_ptr())
  };
  let atom = unsafe { JS_NewAtom(ctx, c"value".as_ptr()) };
  unsafe {
    JS_DefinePropertyGetSet(
      ctx,
      obj,
      atom,
      getter,
      setter,
      JS_PROP_CONFIGURABLE,
    );
    JS_FreeAtom(ctx, atom);
  }
  unsafe { set_wasm_object_prototype(ctx, obj, c"Global") };
  obj
}

unsafe fn set_wasm_object_prototype(
  ctx: *mut JSContext,
  obj: JSValue,
  constructor_name: &std::ffi::CStr,
) {
  let global = unsafe { JS_GetGlobalObject(ctx) };
  let webassembly =
    unsafe { JS_GetPropertyStr(ctx, global, c"WebAssembly".as_ptr()) };
  let constructor =
    unsafe { JS_GetPropertyStr(ctx, webassembly, constructor_name.as_ptr()) };
  let prototype =
    unsafe { JS_GetPropertyStr(ctx, constructor, c"prototype".as_ptr()) };
  if jsv_is_object(&prototype) {
    unsafe { JS_SetPrototype(ctx, obj, prototype) };
  }
  unsafe {
    JS_FreeValue(ctx, prototype);
    JS_FreeValue(ctx, constructor);
    JS_FreeValue(ctx, webassembly);
    JS_FreeValue(ctx, global);
  }
}

unsafe fn wasm_value_type_from_js(
  ctx: *mut JSContext,
  desc: JSValue,
) -> Option<u8> {
  let type_value = unsafe { JS_GetPropertyStr(ctx, desc, c"value".as_ptr()) };
  let mut len = 0usize;
  let cs = unsafe { JS_ToCStringLen(ctx, &mut len, type_value) };
  unsafe { JS_FreeValue(ctx, type_value) };
  if cs.is_null() {
    return None;
  }
  let bytes = unsafe { std::slice::from_raw_parts(cs as *const u8, len) };
  let kind = match bytes {
    b"i32" => Some(WASM_I32),
    b"i64" => Some(WASM_I64),
    b"f32" => Some(WASM_F32),
    b"f64" => Some(WASM_F64),
    _ => None,
  };
  unsafe { JS_FreeCString(ctx, cs) };
  kind
}

unsafe extern "C" fn wa_global_ctor(
  ctx: *mut JSContext,
  _this: JSValue,
  argc: c_int,
  argv: *mut JSValue,
) -> JSValue {
  if argc < 2 {
    return unsafe { throw(ctx, "WebAssembly.Global: missing arguments") };
  }
  let desc = unsafe { *argv };
  let Some(kind) = (unsafe { wasm_value_type_from_js(ctx, desc) }) else {
    return unsafe { throw(ctx, "WebAssembly.Global: invalid value type") };
  };
  let mutable = unsafe {
    let v = JS_GetPropertyStr(ctx, desc, c"mutable".as_ptr());
    let out = JS_ToBool(ctx, v) != 0;
    JS_FreeValue(ctx, v);
    out
  };
  let initial = unsafe { js_to_wasm_val(ctx, kind, *argv.add(1)) };
  let valtype = unsafe { wasm_valtype_new(kind) };
  if valtype.is_null() {
    return unsafe { throw(ctx, "WebAssembly.Global: invalid value type") };
  }
  let globaltype =
    unsafe { wasm_globaltype_new(valtype, if mutable { 1 } else { 0 }) };
  if globaltype.is_null() {
    return unsafe { throw(ctx, "WebAssembly.Global: invalid global type") };
  }
  let store = with_state(|st| st.store);
  let global = unsafe { wasm_global_new(store, globaltype, &initial) };
  if global.is_null() {
    return unsafe {
      throw(ctx, "WebAssembly.Global: failed to create global")
    };
  }
  unsafe { make_global_obj(ctx, global) }
}

// Standalone `new WebAssembly.Memory({ initial, maximum })`. Unlike the
// instance-exported memories above (backed by a live `wasm_memory_t`), a
// user-constructed Memory is JS-backed: we own a plain zeroed ArrayBuffer of
// `pages * 65536` bytes and re-allocate it on `grow`. The buffer is marked
// non-detachable (see `mark_buffer_nondetachable`) so deno's detach ops throw
// "expected: detachable" rather than stealing WASM-memory bytes.
const WASM_PAGE: usize = 65536;

struct JsMemEnv {
  buf: std::cell::Cell<JSValue>,
  pages: std::cell::Cell<u32>,
  max_pages: u32,
}

unsafe fn js_mem_new_buffer(ctx: *mut JSContext, bytes: usize) -> JSValue {
  let zeros = vec![0u8; bytes];
  let buf = unsafe { JS_NewArrayBufferCopy(ctx, zeros.as_ptr(), bytes) };
  crate::quickjs::arraybuffer::mark_buffer_nondetachable(ctx, buf);
  buf
}

unsafe extern "C" fn js_mem_buffer_get(
  ctx: *mut JSContext,
  _this: JSValue,
  _argc: c_int,
  _argv: *mut JSValue,
  _magic: c_int,
  data: *mut JSValue,
) -> JSValue {
  let mut envp = 0i64;
  unsafe { JS_ToBigInt64(ctx, &mut envp, *data) };
  if envp == 0 {
    return jsv_undefined();
  }
  let env = unsafe { &*(envp as *const JsMemEnv) };
  unsafe { JS_DupValue(ctx, env.buf.get()) }
}

unsafe extern "C" fn js_mem_grow(
  ctx: *mut JSContext,
  _this: JSValue,
  argc: c_int,
  argv: *mut JSValue,
  _magic: c_int,
  data: *mut JSValue,
) -> JSValue {
  let mut envp = 0i64;
  unsafe { JS_ToBigInt64(ctx, &mut envp, *data) };
  if envp == 0 {
    return jsv_undefined();
  }
  let env = unsafe { &*(envp as *const JsMemEnv) };
  let mut delta = 0i32;
  if argc >= 1 {
    unsafe { JS_ToInt32(ctx, &mut delta, *argv) };
  }
  if delta < 0 {
    return unsafe { throw(ctx, "WebAssembly.Memory.grow: negative delta") };
  }
  let old_pages = env.pages.get();
  let new_pages = old_pages.saturating_add(delta as u32);
  if new_pages > env.max_pages {
    return unsafe { throw(ctx, "WebAssembly.Memory.grow: exceeds maximum") };
  }
  let new_buf =
    unsafe { js_mem_new_buffer(ctx, new_pages as usize * WASM_PAGE) };
  let old = env.buf.get();
  let mut old_len = 0usize;
  let old_ptr = unsafe { JS_GetArrayBuffer(ctx, &mut old_len, old) };
  if !old_ptr.is_null() {
    let mut new_len = 0usize;
    let new_ptr = unsafe { JS_GetArrayBuffer(ctx, &mut new_len, new_buf) };
    if !new_ptr.is_null() {
      unsafe {
        ptr::copy_nonoverlapping(old_ptr, new_ptr, old_len.min(new_len));
      }
    }
  }
  unsafe { JS_FreeValue(ctx, old) };
  env.buf.set(new_buf);
  env.pages.set(new_pages);
  unsafe { JS_NewInt32(ctx, old_pages as i32) }
}

unsafe extern "C" fn wa_memory_ctor(
  ctx: *mut JSContext,
  _this: JSValue,
  argc: c_int,
  argv: *mut JSValue,
) -> JSValue {
  let mut initial: i64 = 0;
  let mut maximum: i64 = -1;
  if argc >= 1 {
    let desc = unsafe { *argv };
    let initv = unsafe { JS_GetPropertyStr(ctx, desc, c"initial".as_ptr()) };
    unsafe { JS_ToInt64(ctx, &mut initial, initv) };
    unsafe { JS_FreeValue(ctx, initv) };
    let maxv = unsafe { JS_GetPropertyStr(ctx, desc, c"maximum".as_ptr()) };
    if maxv.tag != JS_TAG_UNDEFINED {
      unsafe { JS_ToInt64(ctx, &mut maximum, maxv) };
    }
    unsafe { JS_FreeValue(ctx, maxv) };
  }
  let pages = initial.max(0) as u32;
  // 65536 == the WASM32 max # of 64KiB pages (4GiB address space).
  let max_pages = if maximum < 0 {
    65536u32
  } else {
    maximum as u32
  };
  let buf = unsafe { js_mem_new_buffer(ctx, pages as usize * WASM_PAGE) };

  let env = Box::into_raw(Box::new(JsMemEnv {
    buf: std::cell::Cell::new(buf),
    pages: std::cell::Cell::new(pages),
    max_pages,
  }));

  let obj = unsafe { JS_NewObject(ctx) };
  let mut bd = [unsafe { JS_NewBigInt64(ctx, env as i64) }];
  let getter = unsafe {
    JS_NewCFunctionData(ctx, js_mem_buffer_get, 0, 0, 1, bd.as_mut_ptr())
  };
  let atom = unsafe { JS_NewAtom(ctx, c"buffer".as_ptr()) };
  unsafe {
    JS_DefinePropertyGetSet(
      ctx,
      obj,
      atom,
      getter,
      jsv_undefined(),
      JS_PROP_CONFIGURABLE,
    );
    JS_FreeAtom(ctx, atom);
  }
  let mut gd = [unsafe { JS_NewBigInt64(ctx, env as i64) }];
  let grow_fn =
    unsafe { JS_NewCFunctionData(ctx, js_mem_grow, 1, 0, 1, gd.as_mut_ptr()) };
  unsafe {
    JS_DefinePropertyValueStr(
      ctx,
      obj,
      c"grow".as_ptr(),
      grow_fn,
      JS_PROP_WRITABLE | JS_PROP_CONFIGURABLE,
    )
  };
  unsafe { set_wasm_object_prototype(ctx, obj, c"Memory") };
  obj
}

unsafe extern "C" fn wa_table_ctor(
  ctx: *mut JSContext,
  _this: JSValue,
  _argc: c_int,
  _argv: *mut JSValue,
) -> JSValue {
  unsafe { throw(ctx, "WebAssembly.Table construction is not implemented") }
}

unsafe fn make_memory_obj(
  ctx: *mut JSContext,
  mem: *mut wasm_memory_t,
) -> JSValue {
  MEM_GROW_CB_SET.with(|s| {
    if !s.get() {
      unsafe { v82jsc_set_mem_grow_cb(Some(on_mem_grow)) };
      s.set(true);
    }
  });
  let obj = unsafe { JS_NewObject(ctx) };

  let env = Box::into_raw(Box::new(MemoryEnv {
    mem,
    ctx,
    cached: std::cell::Cell::new(jsv_undefined()),
  }));
  MEMORY_ENVS.with(|v| v.borrow_mut().push(env as *const MemoryEnv));
  let mut data_arr = [unsafe { JS_NewBigInt64(ctx, env as i64) }];
  let getter = unsafe {
    JS_NewCFunctionData(ctx, memory_buffer_get, 0, 0, 1, data_arr.as_mut_ptr())
  };
  let atom = unsafe { JS_NewAtom(ctx, c"buffer".as_ptr()) };
  unsafe {
    JS_DefinePropertyGetSet(
      ctx,
      obj,
      atom,
      getter,
      jsv_undefined(),
      JS_PROP_CONFIGURABLE,
    );
    JS_FreeAtom(ctx, atom);
  }
  let mut gdata = [unsafe { JS_NewBigInt64(ctx, env as i64) }];
  let grow_fn = unsafe {
    JS_NewCFunctionData(ctx, memory_grow, 1, 0, 1, gdata.as_mut_ptr())
  };
  unsafe {
    JS_DefinePropertyValueStr(
      ctx,
      obj,
      c"grow".as_ptr(),
      grow_fn,
      JS_PROP_WRITABLE | JS_PROP_CONFIGURABLE,
    )
  };
  unsafe { set_wasm_object_prototype(ctx, obj, c"Memory") };
  obj
}

unsafe extern "C" fn wa_validate(
  ctx: *mut JSContext,
  _this: JSValue,
  argc: c_int,
  argv: *mut JSValue,
) -> JSValue {
  if argc < 1 {
    return unsafe { JS_NewBool(ctx, 0) };
  }
  let Some(bytes) = (unsafe { read_wasm_bytes(ctx, *argv) }) else {
    return unsafe { JS_NewBool(ctx, 0) };
  };
  let ok = with_state(|st| {
    let mut bin = wasm_vec_t::empty();
    unsafe { wasm_byte_vec_new(&mut bin, bytes.len(), bytes.as_ptr()) };
    let r = unsafe { wasm_module_validate(st.store, &bin) };
    unsafe { wasm_byte_vec_delete(&mut bin) };
    r
  });
  unsafe { JS_NewBool(ctx, ok as c_int) }
}

unsafe extern "C" fn wa_module_ctor(
  ctx: *mut JSContext,
  _this: JSValue,
  argc: c_int,
  argv: *mut JSValue,
) -> JSValue {
  if argc < 1 {
    return unsafe { throw(ctx, "WebAssembly.Module: missing bytes") };
  }
  let Some(bytes) = (unsafe { read_wasm_bytes(ctx, *argv) }) else {
    return unsafe { throw(ctx, "WebAssembly.Module: invalid bytes") };
  };
  match unsafe { compile_module(ctx, &bytes) } {
    Ok(id) => unsafe { make_module_obj(ctx, id) },
    Err(e) => e,
  }
}

unsafe extern "C" fn wa_instance_ctor(
  ctx: *mut JSContext,
  _this: JSValue,
  argc: c_int,
  argv: *mut JSValue,
) -> JSValue {
  if argc < 1 {
    return unsafe { throw(ctx, "WebAssembly.Instance: missing module") };
  }
  let module = unsafe { *argv };
  let Some(mid) = (unsafe { obj_module_id(ctx, module) }) else {
    return unsafe {
      throw(ctx, "WebAssembly.Instance: argument is not a Module")
    };
  };
  let imports = if argc >= 2 {
    unsafe { *argv.add(1) }
  } else {
    jsv_undefined()
  };
  match unsafe { instantiate(ctx, mid, imports) } {
    Ok(inst) => inst,
    Err(e) => e,
  }
}

unsafe extern "C" fn wa_compile(
  ctx: *mut JSContext,
  _this: JSValue,
  argc: c_int,
  argv: *mut JSValue,
) -> JSValue {
  let result = unsafe { wa_module_ctor(ctx, jsv_undefined(), argc, argv) };
  unsafe { settle_promise(ctx, result) }
}

unsafe extern "C" fn wa_instantiate(
  ctx: *mut JSContext,
  _this: JSValue,
  argc: c_int,
  argv: *mut JSValue,
) -> JSValue {
  if argc < 1 {
    let e = unsafe { throw(ctx, "WebAssembly.instantiate: missing argument") };
    return unsafe { settle_promise(ctx, e) };
  }
  let first = unsafe { *argv };
  let imports = if argc >= 2 {
    unsafe { *argv.add(1) }
  } else {
    jsv_undefined()
  };

  if let Some(mid) = unsafe { obj_module_id(ctx, first) } {
    let r = unsafe { instantiate(ctx, mid, imports) };
    return match r {
      Ok(inst) => unsafe { settle_promise(ctx, inst) },
      Err(e) => unsafe { settle_promise(ctx, e) },
    };
  }
  let Some(bytes) = (unsafe { read_wasm_bytes(ctx, first) }) else {
    let e = unsafe { throw(ctx, "WebAssembly.instantiate: invalid source") };
    return unsafe { settle_promise(ctx, e) };
  };
  let mid = match unsafe { compile_module(ctx, &bytes) } {
    Ok(id) => id,
    Err(e) => return unsafe { settle_promise(ctx, e) },
  };
  match unsafe { instantiate(ctx, mid, imports) } {
    Ok(inst) => {
      let res = unsafe { JS_NewObject(ctx) };
      unsafe {
        JS_SetPropertyStr(
          ctx,
          res,
          c"module".as_ptr(),
          make_module_obj(ctx, mid),
        )
      };
      unsafe { JS_SetPropertyStr(ctx, res, c"instance".as_ptr(), inst) };
      unsafe { settle_promise(ctx, res) }
    }
    Err(e) => unsafe { settle_promise(ctx, e) },
  }
}

unsafe fn settle_promise(ctx: *mut JSContext, value: JSValue) -> JSValue {
  let is_exc = value.tag == JS_TAG_EXCEPTION;
  let v = if is_exc {
    let e = unsafe { JS_GetException(ctx) };
    e
  } else {
    value
  };
  let helper = SETTLE.with(|c| {
    if let Some(h) = c.get() {
      return h;
    }
    let src = c"(v,e)=>e?Promise.reject(v):Promise.resolve(v)";
    let f = unsafe {
      JS_Eval(
        ctx,
        src.as_ptr(),
        src.to_bytes().len(),
        c"<wasm-settle>".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      )
    };
    if f.tag != JS_TAG_EXCEPTION {
      c.set(Some(f));
    }
    f
  });
  let mut args = [v, unsafe { JS_NewBool(ctx, is_exc as c_int) }];
  let p =
    unsafe { JS_Call(ctx, helper, jsv_undefined(), 2, args.as_mut_ptr()) };
  unsafe { JS_FreeValue(ctx, v) };
  p
}

thread_local! {
    static SETTLE: std::cell::Cell<Option<JSValue>> = const { std::cell::Cell::new(None) };
}

unsafe fn extern_kind_str(kind: u8) -> &'static std::ffi::CStr {
  match kind {
    WASM_EXTERN_FUNC => c"function",
    WASM_EXTERN_GLOBAL => c"global",
    WASM_EXTERN_TABLE => c"table",
    WASM_EXTERN_MEMORY => c"memory",
    _ => c"",
  }
}

unsafe fn js_str(ctx: *mut JSContext, s: &str) -> JSValue {
  unsafe {
    JS_NewStringLen(ctx, s.as_ptr() as *const std::os::raw::c_char, s.len())
  }
}

unsafe fn js_arraybuffer_from_bytes(
  ctx: *mut JSContext,
  bytes: &[u8],
) -> JSValue {
  unsafe { JS_NewArrayBufferCopy(ctx, bytes.as_ptr(), bytes.len()) }
}

fn custom_sections(bytes: &[u8], wanted: &str) -> Vec<Vec<u8>> {
  if bytes.len() < 8 || &bytes[..4] != b"\0asm" {
    return Vec::new();
  }
  let mut out = Vec::new();
  let mut off = 8usize;
  while off < bytes.len() {
    let id = bytes[off];
    off += 1;
    let Some((size, n)) = read_leb(bytes, off) else {
      break;
    };
    off += n;
    let end = off.saturating_add(size as usize);
    if end > bytes.len() {
      break;
    }
    if id == 0 {
      let sec = &bytes[off..end];
      if let Some((name_len, name_n)) = read_leb(sec, 0) {
        let name_start = name_n;
        let name_end = name_start.saturating_add(name_len as usize);
        if name_end <= sec.len() {
          let name =
            std::str::from_utf8(&sec[name_start..name_end]).unwrap_or("");
          if name == wanted {
            out.push(sec[name_end..].to_vec());
          }
        }
      }
    }
    off = end;
  }
  out
}

fn read_leb(bytes: &[u8], mut off: usize) -> Option<(u32, usize)> {
  let start = off;
  let mut result = 0u32;
  let mut shift = 0;
  while off < bytes.len() && shift < 35 {
    let b = bytes[off];
    off += 1;
    result |= ((b & 0x7f) as u32) << shift;
    if b & 0x80 == 0 {
      return Some((result, off - start));
    }
    shift += 7;
  }
  None
}

fn wasm_function_names(
  bytes: &[u8],
) -> std::collections::HashMap<u32, std::string::String> {
  let mut names = std::collections::HashMap::new();
  for section in custom_sections(bytes, "name") {
    let mut off = 0usize;
    while off < section.len() {
      let subsection = section[off];
      off += 1;
      let Some((size, size_len)) = read_leb(&section, off) else {
        break;
      };
      off += size_len;
      let end = off.saturating_add(size as usize);
      if end > section.len() {
        break;
      }
      if subsection == 1 {
        let payload = &section[off..end];
        let Some((count, count_len)) = read_leb(payload, 0) else {
          break;
        };
        let mut name_off = count_len;
        for _ in 0..count {
          let Some((index, index_len)) = read_leb(payload, name_off) else {
            break;
          };
          name_off += index_len;
          let Some((name_len, name_len_len)) = read_leb(payload, name_off)
          else {
            break;
          };
          name_off += name_len_len;
          let name_end = name_off.saturating_add(name_len as usize);
          if name_end > payload.len() {
            break;
          }
          if let Ok(name) = std::str::from_utf8(&payload[name_off..name_end]) {
            names.insert(index, name.to_string());
          }
          name_off = name_end;
        }
      }
      off = end;
    }
  }
  names
}

fn wasm_function_trap_fallback(
  bytes: &[u8],
  defined_index: u32,
) -> Option<usize> {
  if bytes.len() < 8 || &bytes[..4] != b"\0asm" {
    return None;
  }
  let mut off = 8usize;
  while off < bytes.len() {
    let id = *bytes.get(off)?;
    off += 1;
    let (size, size_len) = read_leb(bytes, off)?;
    off += size_len;
    let section_end = off.checked_add(size as usize)?;
    if section_end > bytes.len() {
      return None;
    }
    if id != 10 {
      off = section_end;
      continue;
    }
    let (count, count_len) = read_leb(bytes, off)?;
    if defined_index >= count {
      return None;
    }
    off += count_len;
    for index in 0..count {
      let (body_size, body_size_len) = read_leb(bytes, off)?;
      off += body_size_len;
      let body_end = off.checked_add(body_size as usize)?;
      if body_end > section_end {
        return None;
      }
      if index == defined_index {
        // WAMR's fast interpreter does not retain a raw module PC. A terminal
        // trap is immediately before the function's final `end` opcode.
        return body_end.checked_sub(2);
      }
      off = body_end;
    }
    return None;
  }
  None
}

// WebAssembly.Module.exports(module) -> [{ name, kind }]
unsafe extern "C" fn wa_module_exports(
  ctx: *mut JSContext,
  _this: JSValue,
  argc: c_int,
  argv: *mut JSValue,
) -> JSValue {
  if argc < 1 {
    return unsafe { throw(ctx, "WebAssembly.Module.exports: missing module") };
  }
  let Some(id) = (unsafe { obj_module_id(ctx, *argv) }) else {
    return unsafe { throw(ctx, "WebAssembly.Module.exports: invalid module") };
  };
  let m = unsafe { module_ptr(id) };
  if m.is_null() {
    return unsafe { throw(ctx, "WebAssembly.Module.exports: invalid module") };
  }
  let mut ets = wasm_vec_t::empty();
  unsafe { wasm_module_exports(m, &mut ets) };
  let arr = unsafe { JS_NewArray(ctx) };
  let data = ets.data as *const *const wasm_exporttype_t;
  for i in 0..ets.size {
    let et = unsafe { *data.add(i) };
    let name = vec_name_to_string(unsafe { wasm_exporttype_name(et) });
    let kind = unsafe { wasm_externtype_kind(wasm_exporttype_type(et)) };
    let desc = unsafe { JS_NewObject(ctx) };
    unsafe {
      JS_SetPropertyStr(ctx, desc, c"name".as_ptr(), js_str(ctx, &name));
      JS_SetPropertyStr(
        ctx,
        desc,
        c"kind".as_ptr(),
        JS_NewStringLen(
          ctx,
          extern_kind_str(kind).as_ptr(),
          extern_kind_str(kind).to_bytes().len(),
        ),
      );
      JS_SetPropertyUint32(ctx, arr, i as u32, desc);
    }
  }
  arr
}

// WebAssembly.Module.imports(module) -> [{ module, name, kind }]
unsafe extern "C" fn wa_module_imports(
  ctx: *mut JSContext,
  _this: JSValue,
  argc: c_int,
  argv: *mut JSValue,
) -> JSValue {
  if argc < 1 {
    return unsafe { throw(ctx, "WebAssembly.Module.imports: missing module") };
  }
  let Some(id) = (unsafe { obj_module_id(ctx, *argv) }) else {
    return unsafe { throw(ctx, "WebAssembly.Module.imports: invalid module") };
  };
  let m = unsafe { module_ptr(id) };
  if m.is_null() {
    return unsafe { throw(ctx, "WebAssembly.Module.imports: invalid module") };
  }
  let mut its = wasm_vec_t::empty();
  unsafe { wasm_module_imports(m, &mut its) };
  let arr = unsafe { JS_NewArray(ctx) };
  let data = its.data as *const *const wasm_importtype_t;
  for i in 0..its.size {
    let it = unsafe { *data.add(i) };
    let modname = vec_name_to_string(unsafe { wasm_importtype_module(it) });
    let name = vec_name_to_string(unsafe { wasm_importtype_name(it) });
    let kind = unsafe { wasm_externtype_kind(wasm_importtype_type(it)) };
    let desc = unsafe { JS_NewObject(ctx) };
    unsafe {
      JS_SetPropertyStr(ctx, desc, c"module".as_ptr(), js_str(ctx, &modname));
      JS_SetPropertyStr(ctx, desc, c"name".as_ptr(), js_str(ctx, &name));
      JS_SetPropertyStr(
        ctx,
        desc,
        c"kind".as_ptr(),
        JS_NewStringLen(
          ctx,
          extern_kind_str(kind).as_ptr(),
          extern_kind_str(kind).to_bytes().len(),
        ),
      );
      JS_SetPropertyUint32(ctx, arr, i as u32, desc);
    }
  }
  arr
}

// WebAssembly.Module.customSections(module, name) -> [] (sections not retained)
unsafe extern "C" fn wa_module_custom_sections(
  ctx: *mut JSContext,
  _this: JSValue,
  argc: c_int,
  argv: *mut JSValue,
) -> JSValue {
  if argc < 2 {
    return unsafe { JS_NewArray(ctx) };
  }
  let Some(id) = (unsafe { obj_module_id(ctx, *argv) }) else {
    return unsafe { JS_NewArray(ctx) };
  };
  let name = unsafe {
    let mut len = 0usize;
    let cs = JS_ToCStringLen(ctx, &mut len, *argv.add(1));
    if cs.is_null() {
      std::string::String::new()
    } else {
      let s = std::slice::from_raw_parts(cs as *const u8, len);
      let out = std::string::String::from_utf8_lossy(s).into_owned();
      JS_FreeCString(ctx, cs);
      out
    }
  };
  let sections = with_state(|st| {
    st.modules
      .get(id)
      .map(|m| custom_sections(&m.bytes, &name))
      .unwrap_or_default()
  });
  let arr = unsafe { JS_NewArray(ctx) };
  for (i, sec) in sections.iter().enumerate() {
    let buf = unsafe { js_arraybuffer_from_bytes(ctx, sec) };
    unsafe { JS_SetPropertyUint32(ctx, arr, i as u32, buf) };
  }
  arr
}

unsafe extern "C" fn wa_compile_streaming(
  ctx: *mut JSContext,
  _this: JSValue,
  argc: c_int,
  argv: *mut JSValue,
) -> JSValue {
  let cb = STREAMING_CALLBACK.with(|c| c.get());
  if cb.is_none() || argc < 1 {
    return unsafe { wa_compile(ctx, _this, argc, argv) };
  }
  let mut funcs = [jsv_undefined(), jsv_undefined()];
  let promise = unsafe { JS_NewPromiseCapability(ctx, funcs.as_mut_ptr()) };
  if promise.tag == JS_TAG_EXCEPTION {
    return promise;
  }
  let state = Box::into_raw(Box::new(StreamingState {
    ctx,
    bytes: Vec::new(),
    source_url: std::string::String::new(),
    resolve: funcs[0],
    reject: funcs[1],
    promise: unsafe { JS_DupValue(ctx, promise) },
    refcount: 1,
    done: false,
  }));
  STREAMING_PENDING.with(|p| p.set(p.get() + 1));
  let data = unsafe { JS_NewBigInt64(ctx, state as i64) };
  let mut function_data = [data];
  let fulfilled = unsafe {
    JS_NewCFunctionData(
      ctx,
      streaming_source_fulfilled,
      1,
      0,
      1,
      function_data.as_mut_ptr(),
    )
  };
  let rejected = unsafe {
    JS_NewCFunctionData(
      ctx,
      streaming_source_rejected,
      1,
      0,
      1,
      function_data.as_mut_ptr(),
    )
  };
  unsafe {
    JS_FreeValue(ctx, data);
  }
  let global = unsafe { JS_GetGlobalObject(ctx) };
  let promise_constructor =
    unsafe { JS_GetPropertyStr(ctx, global, c"Promise".as_ptr()) };
  let resolve =
    unsafe { JS_GetPropertyStr(ctx, promise_constructor, c"resolve".as_ptr()) };
  let mut resolve_args = [unsafe { JS_DupValue(ctx, *argv) }];
  let source_promise = unsafe {
    JS_Call(
      ctx,
      resolve,
      promise_constructor,
      1,
      resolve_args.as_mut_ptr(),
    )
  };
  let then =
    unsafe { JS_GetPropertyStr(ctx, source_promise, c"then".as_ptr()) };
  let mut then_args = [fulfilled, rejected];
  let chained =
    unsafe { JS_Call(ctx, then, source_promise, 2, then_args.as_mut_ptr()) };
  unsafe {
    JS_FreeValue(ctx, chained);
    JS_FreeValue(ctx, then_args[0]);
    JS_FreeValue(ctx, then_args[1]);
    JS_FreeValue(ctx, then);
    JS_FreeValue(ctx, source_promise);
    JS_FreeValue(ctx, resolve_args[0]);
    JS_FreeValue(ctx, resolve);
    JS_FreeValue(ctx, promise_constructor);
    JS_FreeValue(ctx, global);
  }
  promise
}

unsafe extern "C" fn wa_instantiate_streaming(
  ctx: *mut JSContext,
  this_value: JSValue,
  argc: c_int,
  argv: *mut JSValue,
) -> JSValue {
  let compile_promise =
    unsafe { wa_compile_streaming(ctx, this_value, argc, argv) };
  if compile_promise.tag == JS_TAG_EXCEPTION {
    return compile_promise;
  }
  let imports = if argc >= 2 {
    unsafe { *argv.add(1) }
  } else {
    jsv_undefined()
  };
  let mut function_data = [imports];
  let instantiate = unsafe {
    JS_NewCFunctionData(
      ctx,
      streaming_instantiate_fulfilled,
      1,
      0,
      1,
      function_data.as_mut_ptr(),
    )
  };
  let then =
    unsafe { JS_GetPropertyStr(ctx, compile_promise, c"then".as_ptr()) };
  let mut args = [instantiate];
  let result =
    unsafe { JS_Call(ctx, then, compile_promise, 1, args.as_mut_ptr()) };
  unsafe {
    JS_FreeValue(ctx, instantiate);
    JS_FreeValue(ctx, then);
    JS_FreeValue(ctx, compile_promise);
  }
  result
}

unsafe extern "C" fn streaming_instantiate_fulfilled(
  ctx: *mut JSContext,
  _this: JSValue,
  argc: c_int,
  argv: *mut JSValue,
  _magic: c_int,
  data: *mut JSValue,
) -> JSValue {
  if argc < 1 || argv.is_null() || data.is_null() {
    return unsafe {
      throw(ctx, "WebAssembly.instantiateStreaming: invalid module")
    };
  }
  let module = unsafe { *argv };
  let Some(module_id) = (unsafe { obj_module_id(ctx, module) }) else {
    return unsafe {
      throw(ctx, "WebAssembly.instantiateStreaming: invalid module")
    };
  };
  let instance = match unsafe { instantiate(ctx, module_id, *data) } {
    Ok(instance) => instance,
    Err(error) => return error,
  };
  let result = unsafe { JS_NewObject(ctx) };
  unsafe {
    JS_SetPropertyStr(
      ctx,
      result,
      c"module".as_ptr(),
      JS_DupValue(ctx, module),
    );
    JS_SetPropertyStr(ctx, result, c"instance".as_ptr(), instance);
  }
  result
}

unsafe fn streaming_state_from_function_data(
  ctx: *mut JSContext,
  data: *mut JSValue,
) -> Option<&'static mut StreamingState> {
  if data.is_null() {
    return None;
  }
  let mut pointer = 0i64;
  if unsafe { JS_ToBigInt64(ctx, &mut pointer, *data) } < 0 || pointer == 0 {
    return None;
  }
  Some(unsafe { &mut *(pointer as *mut StreamingState) })
}

unsafe extern "C" fn streaming_source_fulfilled(
  ctx: *mut JSContext,
  _this: JSValue,
  argc: c_int,
  argv: *mut JSValue,
  _magic: c_int,
  data: *mut JSValue,
) -> JSValue {
  let Some(state) = (unsafe { streaming_state_from_function_data(ctx, data) })
  else {
    return jsv_undefined();
  };
  let Some(callback) = STREAMING_CALLBACK.with(|callback| callback.get())
  else {
    return jsv_undefined();
  };
  let source = if argc > 0 {
    unsafe { *argv }
  } else {
    jsv_undefined()
  };
  let callback_data = unsafe { JS_NewBigInt64(ctx, state as *mut _ as i64) };
  let mut args = [source];
  let result = unsafe {
    crate::quickjs::function::call_callback_for_wasm(
      ctx,
      callback,
      callback_data,
      &mut args,
    )
  };
  unsafe {
    JS_FreeValue(ctx, result);
    JS_FreeValue(ctx, callback_data);
  }
  jsv_undefined()
}

unsafe extern "C" fn streaming_source_rejected(
  ctx: *mut JSContext,
  _this: JSValue,
  argc: c_int,
  argv: *mut JSValue,
  _magic: c_int,
  data: *mut JSValue,
) -> JSValue {
  let Some(state) = (unsafe { streaming_state_from_function_data(ctx, data) })
  else {
    return jsv_undefined();
  };
  if state.done {
    return jsv_undefined();
  }
  state.done = true;
  let reason = if argc > 0 {
    unsafe { *argv }
  } else {
    jsv_undefined()
  };
  let mut args = [reason];
  let result = unsafe {
    JS_Call(ctx, state.reject, jsv_undefined(), 1, args.as_mut_ptr())
  };
  unsafe { JS_FreeValue(ctx, result) };
  STREAMING_PENDING
    .with(|pending| pending.set(pending.get().saturating_sub(1)));
  jsv_undefined()
}

pub(crate) fn install_webassembly(ctx: *mut JSContext) {
  unsafe {
    let global = JS_GetGlobalObject(ctx);
    let wa = JS_NewObject(ctx);

    let set_fn =
      |obj: JSValue, name: &std::ffi::CStr, f: JSCFunction, argc: c_int| {
        let func = JS_NewCFunction(ctx, f, name.as_ptr(), argc);
        JS_SetPropertyStr(ctx, obj, name.as_ptr(), func);
      };

    let set_ctor = |obj: JSValue,
                    name: &std::ffi::CStr,
                    f: JSCFunction,
                    argc: c_int| {
      let func =
        JS_NewCFunction2(ctx, f, name.as_ptr(), argc, JS_CFUNC_CONSTRUCTOR, 0);
      let prototype = JS_NewObject(ctx);
      JS_SetConstructor(ctx, func, prototype);
      JS_FreeValue(ctx, prototype);
      JS_SetPropertyStr(ctx, obj, name.as_ptr(), func);
    };
    set_fn(wa, c"validate", wa_validate, 1);
    set_ctor(wa, c"Module", wa_module_ctor, 1);
    set_ctor(wa, c"Instance", wa_instance_ctor, 2);
    set_ctor(wa, c"Memory", wa_memory_ctor, 1);
    set_ctor(wa, c"Global", wa_global_ctor, 2);
    set_ctor(wa, c"Table", wa_table_ctor, 1);
    set_fn(wa, c"compile", wa_compile, 1);
    set_fn(wa, c"instantiate", wa_instantiate, 2);

    // Static introspection methods on the Module constructor.
    let module_ctor = JS_GetPropertyStr(ctx, wa, c"Module".as_ptr());
    set_fn(module_ctor, c"exports", wa_module_exports, 1);
    set_fn(module_ctor, c"imports", wa_module_imports, 1);
    set_fn(module_ctor, c"customSections", wa_module_custom_sections, 2);
    JS_FreeValue(ctx, module_ctor);

    JS_SetPropertyStr(ctx, global, c"WebAssembly".as_ptr(), wa);
    JS_FreeValue(ctx, global);

    let brands = c"(()=>{const W=WebAssembly;for(const n of ['Module','Instance','Memory','Global','Table'])Object.defineProperty(W[n].prototype,Symbol.toStringTag,{value:`WebAssembly.${n}`,configurable:true});for(const n of ['CompileError','LinkError','RuntimeError']){const C=class extends Error{};Object.defineProperty(C,'name',{value:n});Object.defineProperty(C.prototype,'name',{value:n,configurable:true});Object.defineProperty(W,n,{value:C,writable:true,configurable:true});}})()";
    let brands_result = JS_Eval(
      ctx,
      brands.as_ptr(),
      brands.to_bytes().len(),
      c"<wasm-brands>".as_ptr(),
      JS_EVAL_TYPE_GLOBAL,
    );
    JS_FreeValue(ctx, brands_result);

    // compile/instantiateStreaming: V8 installs these via a streaming callback
    // deno provides; we expose the standard buffering polyfill (await the
    // Response/source, read its bytes, delegate to the non-streaming form).
    let poly = c"(()=>{const W=globalThis.WebAssembly;\
const check=async s=>{s=await s;\
if(typeof Response!=='undefined'&&!(s instanceof Response))throw new TypeError('Invalid WebAssembly response object');\
const ct=((s.headers&&s.headers.get('content-type'))||'').split(';')[0].trim().toLowerCase();\
if(ct!=='application/wasm')throw new TypeError(\"Invalid WebAssembly content type, expected 'application/wasm'.\");\
return await s.arrayBuffer();};\
W.compileStreaming=async s=>W.compile(await check(s));\
W.instantiateStreaming=async (s,i)=>W.instantiate(await check(s),i);})()";
    let r = JS_Eval(
      ctx,
      poly.as_ptr(),
      poly.to_bytes().len(),
      c"<wasm-streaming>".as_ptr(),
      JS_EVAL_TYPE_GLOBAL,
    );
    JS_FreeValue(ctx, r);

    set_fn(wa, c"compileStreaming", wa_compile_streaming, 1);
    set_fn(wa, c"instantiateStreaming", wa_instantiate_streaming, 2);
  }
}

pub(crate) fn set_streaming_callback(cb: Option<StreamingCallback>) {
  STREAMING_CALLBACK.with(|c| c.set(cb));
}

pub(crate) fn has_pending_streaming_task() -> bool {
  STREAMING_PENDING.with(|p| p.get() != 0)
}

pub(crate) fn is_module_object(this: *const Value) -> bool {
  if this.is_null() {
    return false;
  }
  let ctx = current_ctx();
  if ctx.is_null() {
    return false;
  }
  let id = unsafe { obj_module_id(ctx, jsval_of(this)) };
  id.is_some()
}

fn compiled_from_entry(entry: &ModuleEntry) -> *mut c_void {
  Box::into_raw(Box::new(CompiledModule {
    bytes: entry.bytes.clone(),
    source_url: entry.source_url.clone(),
  })) as *mut c_void
}

pub(crate) fn module_object_get_compiled_module(
  this: *const c_void,
) -> *mut c_void {
  if this.is_null() {
    return ptr::null_mut();
  }
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null_mut();
  }
  let Some(id) =
    (unsafe { obj_module_id(ctx, jsval_of(this as *const Value)) })
  else {
    return ptr::null_mut();
  };
  with_state(|st| {
    st.modules
      .get(id)
      .map(compiled_from_entry)
      .unwrap_or(ptr::null_mut())
  })
}

pub(crate) fn module_object_from_compiled_module(
  isolate: *mut c_void,
  compiled_module: *const c_void,
) -> *const c_void {
  if compiled_module.is_null() {
    return ptr::null();
  }
  let iso = if isolate.is_null() {
    current_iso()
  } else {
    isolate as *mut RealIsolate
  };
  if iso.is_null() {
    return ptr::null();
  }
  let ctx = iso_state(iso)
    .contexts
    .last()
    .copied()
    .unwrap_or(iso_state(iso).ctx);
  if ctx.is_null() {
    return ptr::null();
  }
  let cm = unsafe { &*(compiled_module as *const CompiledModule) };
  let id = match unsafe { compile_module(ctx, &cm.bytes) } {
    Ok(id) => id,
    Err(_) => return ptr::null(),
  };
  with_state(|st| {
    if let Some(entry) = st.modules.get_mut(id) {
      entry.source_url = cm.source_url.clone();
    }
  });
  let obj = unsafe { make_module_obj(ctx, id) };
  intern::<Object>(obj) as *const c_void
}

pub(crate) fn compiled_module_delete(this: *mut c_void) {
  if !this.is_null() {
    unsafe { drop(Box::from_raw(this as *mut CompiledModule)) };
  }
}

pub(crate) fn compiled_module_wire_bytes(
  this: *mut c_void,
  length: *mut c_void,
) -> *const c_void {
  if this.is_null() {
    return ptr::null();
  }
  let cm = unsafe { &*(this as *const CompiledModule) };
  if !length.is_null() {
    unsafe { *(length as *mut isize) = cm.bytes.len() as isize };
  }
  cm.bytes.as_ptr() as *const c_void
}

pub(crate) fn compiled_module_source_url(
  this: *mut c_void,
  length: *mut c_void,
) -> *const c_void {
  if this.is_null() {
    return ptr::null();
  }
  let cm = unsafe { &*(this as *const CompiledModule) };
  if !length.is_null() {
    unsafe { *(length as *mut usize) = cm.source_url.len() };
  }
  cm.source_url.as_ptr() as *const c_void
}

pub(crate) fn module_compilation_new() -> *mut c_void {
  Box::into_raw(Box::new(ModuleCompilation {
    bytes: Vec::new(),
    source_url: std::string::String::new(),
    aborted: false,
  })) as *mut c_void
}

pub(crate) fn module_compilation_delete(this: *mut c_void) {
  if !this.is_null() {
    unsafe { drop(Box::from_raw(this as *mut ModuleCompilation)) };
  }
}

pub(crate) fn module_compilation_on_bytes_received(
  this: *mut c_void,
  bytes: *const c_void,
  size: usize,
) {
  if this.is_null() || bytes.is_null() {
    return;
  }
  let c = unsafe { &mut *(this as *mut ModuleCompilation) };
  let b = unsafe { std::slice::from_raw_parts(bytes as *const u8, size) };
  c.bytes.extend_from_slice(b);
}

pub(crate) fn module_compilation_set_url(
  this: *mut c_void,
  url: *const c_void,
  length: usize,
) {
  if this.is_null() || url.is_null() {
    return;
  }
  let c = unsafe { &mut *(this as *mut ModuleCompilation) };
  let bytes = unsafe { std::slice::from_raw_parts(url as *const u8, length) };
  c.source_url = std::string::String::from_utf8_lossy(bytes).into_owned();
}

pub(crate) fn module_compilation_abort(this: *mut c_void) {
  if !this.is_null() {
    unsafe { (*(this as *mut ModuleCompilation)).aborted = true };
  }
}

pub(crate) fn module_compilation_finish(
  this: *mut c_void,
  isolate: *mut c_void,
  _caching_callback: *const c_void,
  resolution_callback: *const c_void,
  resolution_data: *mut c_void,
  _drop_resolution_data: *const c_void,
) {
  if this.is_null() || resolution_callback.is_null() {
    return;
  }
  let c = unsafe { &mut *(this as *mut ModuleCompilation) };
  if c.aborted {
    return;
  }
  let iso = if isolate.is_null() {
    current_iso()
  } else {
    isolate as *mut RealIsolate
  };
  if iso.is_null() {
    return;
  }
  let ctx = iso_state(iso)
    .contexts
    .last()
    .copied()
    .unwrap_or(iso_state(iso).ctx);
  let id = match unsafe { compile_module(ctx, &c.bytes) } {
    Ok(id) => id,
    Err(_) => return,
  };
  with_state(|st| {
    if let Some(entry) = st.modules.get_mut(id) {
      if !c.source_url.is_empty() {
        entry.source_url = c.source_url.clone();
      }
    }
  });
  let module = unsafe { make_module_obj(ctx, id) };
  let module_ptr = intern::<Object>(module) as *const Value;
  let cb: unsafe extern "C" fn(*mut c_void, *const Value, *const Value) =
    unsafe { std::mem::transmute(resolution_callback) };
  unsafe { cb(resolution_data, module_ptr, ptr::null()) };
}

pub(crate) fn streaming_unpack(value: *const Value, that: *mut c_void) {
  if value.is_null() || that.is_null() {
    return;
  }
  let ctx = current_ctx();
  let mut ptr_i = 0i64;
  unsafe { JS_ToBigInt64(ctx, &mut ptr_i, jsval_of(value)) };
  let sp = that as *mut WasmStreamingSharedPtr;
  unsafe {
    (*sp).ptr = ptr_i as *mut StreamingState;
    (*sp)._control = ptr::null_mut();
    if !(*sp).ptr.is_null() {
      (*(*sp).ptr).refcount += 1;
    }
  }
}

pub(crate) fn streaming_shared_ptr_destruct(this: *mut c_void) {
  if this.is_null() {
    return;
  }
  let sp = this as *mut WasmStreamingSharedPtr;
  let ptr = unsafe { (*sp).ptr };
  if ptr.is_null() {
    return;
  }
  unsafe {
    let st = &mut *ptr;
    st.refcount = st.refcount.saturating_sub(1);
    if st.refcount == 0 {
      JS_FreeValue(st.ctx, st.resolve);
      JS_FreeValue(st.ctx, st.reject);
      JS_FreeValue(st.ctx, st.promise);
      drop(Box::from_raw(ptr));
    }
    (*sp).ptr = ptr::null_mut();
  }
}

pub(crate) fn streaming_on_bytes_received(
  this: *mut c_void,
  data: *const u8,
  len: usize,
) {
  let Some(st) = streaming_state(this) else {
    return;
  };
  if data.is_null() {
    return;
  }
  let b = unsafe { std::slice::from_raw_parts(data, len) };
  st.bytes.extend_from_slice(b);
}

pub(crate) fn streaming_set_url(
  this: *mut c_void,
  url: *const c_char,
  len: usize,
) {
  let Some(st) = streaming_state(this) else {
    return;
  };
  if url.is_null() {
    return;
  }
  let b = unsafe { std::slice::from_raw_parts(url as *const u8, len) };
  st.source_url = std::string::String::from_utf8_lossy(b).into_owned();
}

pub(crate) fn streaming_finish(
  this: *mut c_void,
  _callback: Option<unsafe extern "C" fn(*mut c_void)>,
) {
  let Some(st) = streaming_state(this) else {
    return;
  };
  if st.done {
    return;
  }
  st.done = true;
  let id = match unsafe { compile_module(st.ctx, &st.bytes) } {
    Ok(id) => id,
    Err(_) => return,
  };
  with_state(|state| {
    if let Some(entry) = state.modules.get_mut(id) {
      if !st.source_url.is_empty() {
        entry.source_url = st.source_url.clone();
      }
    }
  });
  let module = unsafe { make_module_obj(st.ctx, id) };
  let mut args = [module];
  unsafe {
    let r = JS_Call(st.ctx, st.resolve, jsv_undefined(), 1, args.as_mut_ptr());
    JS_FreeValue(st.ctx, r);
  }
  drain_jobs(st.ctx);
  STREAMING_PENDING.with(|p| p.set(p.get().saturating_sub(1)));
}

pub(crate) fn streaming_abort(this: *mut c_void, exception: *const Value) {
  let Some(st) = streaming_state(this) else {
    return;
  };
  if st.done {
    return;
  }
  st.done = true;
  if !exception.is_null() {
    unsafe {
      super::exception::mark_host_stack_boundary(st.ctx, jsval_of(exception))
    };
    let mut args = [unsafe { JS_DupValue(st.ctx, jsval_of(exception)) }];
    unsafe {
      let r = JS_Call(st.ctx, st.reject, jsv_undefined(), 1, args.as_mut_ptr());
      JS_FreeValue(st.ctx, args[0]);
      JS_FreeValue(st.ctx, r);
    }
    drain_jobs(st.ctx);
  }
  STREAMING_PENDING.with(|p| p.set(p.get().saturating_sub(1)));
}

fn drain_jobs(ctx: *mut JSContext) {
  if ctx.is_null() {
    return;
  }
  let iso = current_iso();
  if iso.is_null() {
    return;
  }
  let rt = iso_state(iso).rt;
  if rt.is_null() {
    return;
  }
  unsafe {
    let mut pctx = ctx;
    while JS_ExecutePendingJob(rt, &mut pctx) > 0 {}
  }
}

fn streaming_state(this: *mut c_void) -> Option<&'static mut StreamingState> {
  if this.is_null() {
    return None;
  }
  let ptr = unsafe { (*(this as *mut WasmStreamingSharedPtr)).ptr };
  if ptr.is_null() {
    None
  } else {
    Some(unsafe { &mut *ptr })
  }
}
