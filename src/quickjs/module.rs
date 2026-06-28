//! QuickJS-ng-backed definitions for the "module" family:
//! Module / ModuleRequest / Script / ScriptCompiler / UnboundScript /
//! UnboundModuleScript / FixedArray / ScriptOrigin.
//!
//! Ported from the deno PR's `reference/qjs_v8_compat/src/module.rs` (which is
//! the primary source for the QuickJS module-loader logic) and shaped to the
//! C-ABI of the JSC backend's `src/module.rs`.
//!
//! QuickJS-ng has REAL ES modules, so most of this is implemented for real:
//!   * `ScriptCompiler::CompileModule` compiles via
//!     `JS_Eval(JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_COMPILE_ONLY)`, which yields
//!     a `JS_TAG_MODULE` bytecode value whose `u.ptr` is the `JSModuleDef`.
//!   * `Module::Evaluate` runs that bytecode with `JS_EvalFunction` (which
//!     evaluates statically-imported dependencies transitively) and drains
//!     pending jobs, then hands back a resolved promise.
//!   * `Module::GetModuleNamespace` returns the real `JS_GetModuleNamespace`.
//!   * Synthetic modules use `JS_NewCModule` + `JS_AddModuleExport`, with
//!     exports populated lazily by an init callback.
//!
//! Per-module state can't live on the `JSValue` (it's a 16-byte tagged union,
//! not a pointer), so — exactly like the PR — we keep thread-local side tables
//! keyed by the module handle's pointer payload (`JSValue.u.ptr as usize`).
//!
//! Refcount discipline: every handle we return is routed through
//! `intern`/`intern_dup`; every `JSValue` we create and don't keep is
//! `JS_FreeValue`d. The bytecode `JSValue` (owned at +1 from the COMPILE_ONLY
//! eval) is stored in a side table and freed on isolate drop is *not* attempted
//! (QuickJS frees module bytecode through the module def); we dup before handing
//! it to `JS_EvalFunction` (which consumes one ref).
#![allow(non_snake_case, unused)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CString;
use std::mem::MaybeUninit;
use std::ptr;

use crate::quickjs::core::{
  ctx_of, current_ctx, current_iso, intern, intern_ctx, intern_dup, iso_state,
  jsval_of,
};
use crate::quickjs::quickjs_sys::*;
use crate::{
  Context, Data, FixedArray, Function, Message, Module, ModuleRequest, Object,
  RealIsolate, Script, String as V8String, UnboundModuleScript, UnboundScript,
  Value,
};

use crate::isolate::ModuleImportPhase;
use crate::module::{
  Location, ModuleStatus, ResolveModuleCallback, ResolveSourceCallback,
  StalledTopLevelAwaitMessage, SyntheticModuleEvaluationSteps,
};
use crate::script::ScriptOrigin;
use crate::script_compiler::{
  CachedData, CompileOptions, NoCacheReason, Source,
};
use crate::support::{MaybeBool, int};

const JS_WRITE_OBJ_BYTECODE: int = 1 << 0;
const JS_READ_OBJ_BYTECODE: int = 1 << 0;

const BC_MAGIC: u32 = 0x5142_4302;

fn bc_cache_dir() -> Option<std::path::PathBuf> {
  use std::sync::OnceLock;
  static DIR: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
  DIR
    .get_or_init(|| {
      if std::env::var_os("V82JSC_NO_BC_CACHE").is_some() {
        return None;
      }
      let base = std::env::var_os("DENO_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| {
          std::env::var_os("HOME")
            .map(|h| std::path::PathBuf::from(h).join("Library/Caches"))
        })
        .unwrap_or_else(std::env::temp_dir);
      let dir = base.join("v82jsc_bc");
      std::fs::create_dir_all(&dir).ok()?;
      Some(dir)
    })
    .clone()
}

fn bc_key(source: &str) -> u64 {
  use std::hash::{Hash, Hasher};
  let mut h = std::collections::hash_map::DefaultHasher::new();
  BC_MAGIC.hash(&mut h);
  source.len().hash(&mut h);
  source.hash(&mut h);
  h.finish()
}

fn bc_path(key: u64) -> Option<std::path::PathBuf> {
  Some(bc_cache_dir()?.join(format!("{key:016x}.qbc")))
}

fn bc_load(key: u64) -> Option<Vec<u8>> {
  let p = bc_path(key)?;
  std::fs::read(p).ok().filter(|b| !b.is_empty())
}

fn bc_store(key: u64, bytes: &[u8]) {
  if bytes.is_empty() {
    return;
  }
  if let Some(p) = bc_path(key) {
    let tmp = p.with_extension("tmp");
    if std::fs::write(&tmp, bytes).is_ok() {
      let _ = std::fs::rename(&tmp, &p);
    }
  }
}

unsafe fn bc_write(ctx: *mut JSContext, key: u64, obj: JSValue) {
  if bc_cache_dir().is_none() {
    return;
  }
  let mut size: usize = 0;
  let buf =
    unsafe { JS_WriteObject(ctx, &mut size, obj, JS_WRITE_OBJ_BYTECODE) };
  if !buf.is_null() && size > 0 {
    let slice = unsafe { std::slice::from_raw_parts(buf, size) };
    bc_store(key, slice);
  }
  if !buf.is_null() {
    unsafe { js_free(ctx, buf as *mut std::os::raw::c_void) };
  }
}

struct ModuleState {
  status: ModuleStatus,

  module_def: *mut JSModuleDef,

  bytecode: Option<JSValue>,

  import_specifiers: Vec<(std::string::String, Option<std::string::String>)>,

  // Parallel to `import_specifiers`: byte offset of each specifier literal's
  // opening quote in the module source (deno's `referrer_source_offset`) and
  // the full `with { ... }` attribute key/value set for that import.
  import_offsets: Vec<i32>,
  import_attributes: Vec<Vec<(std::string::String, std::string::String)>>,

  source_imports: Vec<(u64, std::string::String)>,

  synthetic: bool,

  is_async: bool,

  source_text: std::string::String,
  source_name: std::string::String,
}

thread_local! {
    static MODULE_STATE: RefCell<HashMap<usize, ModuleState>> =
        RefCell::new(HashMap::new());

    static MODULE_SOURCES_BY_NAME: RefCell<HashMap<std::string::String, std::string::String>> =
        RefCell::new(HashMap::new());

    static MODULE_DEF_CACHE: RefCell<HashMap<std::string::String, usize>> =
        RefCell::new(HashMap::new());

    static SYNTHETIC_EXPORTS: RefCell<HashMap<usize, Vec<(std::string::String, JSValue)>>> =
        RefCell::new(HashMap::new());

    // Persistent (dup'd) copy of a synthetic module's exports, keyed by def ptr,
    // kept so `GetModuleNamespace` can build the namespace object — the values in
    // SYNTHETIC_EXPORTS are consumed by JS_SetModuleExport during init.
    static SYNTHETIC_NS_EXPORTS: RefCell<HashMap<usize, Vec<(std::string::String, JSValue)>>> =
        RefCell::new(HashMap::new());

    static SYNTHETIC_DEFS: RefCell<HashMap<usize, usize>> = RefCell::new(HashMap::new());

    static SYNTHETIC_EVAL_STEPS: RefCell<HashMap<usize, (SyntheticModuleEvaluationSteps<'static>, JSValue)>> =
        RefCell::new(HashMap::new());

    static AFTER_FIRST_EVAL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };

    static RESOLVED_SPECIFIERS: RefCell<HashMap<(std::string::String, std::string::String), std::string::String>> =
        RefCell::new(HashMap::new());
}

thread_local! {

    static IMPORT_META_ENABLED: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };

    // deno's real HostInitializeImportMetaObjectCallback. When set we route
    // import.meta population through it (so url/main/resolve/filename/dirname all
    // match deno exactly — notably `import.meta.resolve` uses the embedder's
    // loader). Only the native fallback below runs when this is None.
    static IMPORT_META_CB: std::cell::Cell<
        Option<crate::isolate::HostInitializeImportMetaObjectCallback>,
    > = const { std::cell::Cell::new(None) };

    // Maps a module's source name -> the raw `JSValue` of its deno Module
    // WRAPPER (the handle CompileModule returned). deno's import_meta callback
    // looks the module up by Global identity (`get_name_by_module`), so we must
    // hand it a handle naming the EXACT object deno registered. We store the raw
    // JSValue (not the interned handle pointer) because the handle's heap box is
    // freed when its HandleScope unwinds — the object itself stays alive via
    // deno's Global, so the JSValue payload (`u.ptr`) remains a valid identity.
    static MODULE_WRAPPER_BY_NAME: RefCell<HashMap<std::string::String, JSValue>> =
        RefCell::new(HashMap::new());

    // URL of the main module — the first user (file://http(s)) module deno
    // compiles. Used to set `import.meta.main` (deno's host callback, which we
    // bypass, would normally do this).
    static MAIN_MODULE_URL: std::cell::RefCell<Option<std::string::String>> =
        const { std::cell::RefCell::new(None) };
}

fn record_module_wrapper(name: &str, wrapper: *const Module) {
  if name.is_empty() || wrapper.is_null() {
    return;
  }
  let v = jsval_of(wrapper);
  MODULE_WRAPPER_BY_NAME.with(|t| {
    t.borrow_mut().insert(name.to_string(), v);
  });
}

fn lookup_module_wrapper(name: &str) -> Option<JSValue> {
  MODULE_WRAPPER_BY_NAME.with(|t| t.borrow().get(name).copied())
}

fn note_main_module(name: &str) {
  if !(name.starts_with("file://")
    || name.starts_with("http://")
    || name.starts_with("https://"))
  {
    return;
  }
  MAIN_MODULE_URL.with(|c| {
    let mut b = c.borrow_mut();
    if b.is_none() {
      *b = Some(name.to_string());
    }
  });
}

fn is_main_module(name: &str) -> bool {
  MAIN_MODULE_URL.with(|c| c.borrow().as_deref() == Some(name))
}

pub(crate) fn set_import_meta_callback(
  cb: crate::isolate::HostInitializeImportMetaObjectCallback,
) {
  IMPORT_META_ENABLED.with(|c| c.set(true));
  IMPORT_META_CB.with(|c| c.set(Some(cb)));
}

thread_local! {

    static DYN_IMPORT_CB: std::cell::Cell<
        Option<crate::isolate::RawHostImportModuleDynamicallyCallback>,
    > = const { std::cell::Cell::new(None) };

    static DYN_IMPORT_CHAIN: std::cell::Cell<Option<JSValue>> =
        const { std::cell::Cell::new(None) };

    static SOURCE_CB: std::cell::Cell<Option<ResolveSourceCallback<'static>>> =
        const { std::cell::Cell::new(None) };

    static PENDING_SOURCE_IMPORTS: RefCell<
        Vec<(*const Module, Vec<(u64, std::string::String)>)>,
    > = const { RefCell::new(Vec::new()) };
}

pub(crate) fn set_dynamic_import_callback(
  cb: crate::isolate::RawHostImportModuleDynamicallyCallback,
) {
  DYN_IMPORT_CB.with(|c| c.set(Some(cb)));
  unsafe { JS_SetDynamicImportHook(dynamic_import_hook) };
}

// Flatten QuickJS's null-proto `with` attributes object (e.g. {type:"json"})
// into deno's host-callback FixedArray shape [key1, val1, key2, val2, ...].
unsafe fn build_dyn_attrs(ctx: *mut JSContext, attributes: JSValue) -> JSValue {
  if !jsv_is_object(&attributes) {
    return unsafe { JS_NewArray(ctx) };
  }
  let src =
    c"(o)=>{const a=[];for(const k of Object.keys(o)){a.push(k,String(o[k]));}return a;}";
  let f = unsafe {
    JS_Eval(
      ctx,
      src.as_ptr(),
      src.to_bytes().len(),
      c"<dynimport-attrs>".as_ptr(),
      JS_EVAL_TYPE_GLOBAL,
    )
  };
  if f.tag == JS_TAG_EXCEPTION {
    unsafe { JS_FreeValue(ctx, JS_GetException(ctx)) };
    return unsafe { JS_NewArray(ctx) };
  }
  let mut arg = [attributes];
  let r = unsafe { JS_Call(ctx, f, jsv_undefined(), 1, arg.as_mut_ptr()) };
  unsafe { JS_FreeValue(ctx, f) };
  if r.tag == JS_TAG_EXCEPTION {
    unsafe { JS_FreeValue(ctx, JS_GetException(ctx)) };
    return unsafe { JS_NewArray(ctx) };
  }
  r
}

unsafe extern "C" fn dynamic_import_hook(
  ctx: *mut JSContext,
  basename: JSValue,
  specifier: JSValue,
  attributes: JSValue,
  resolving_funcs: *const JSValue,
) {
  let resolve = unsafe { *resolving_funcs };
  let reject = unsafe { *resolving_funcs.add(1) };

  if std::env::var_os("QJS_DBG_DYNIMPORT").is_some() {
    let s = unsafe { jsval_to_rust(ctx, specifier) };
    eprintln!("[dynimport] specifier={s:?}");
  }

  let reject_with = |msg: &str| unsafe {
    if let Ok(cm) = CString::new(msg) {
      let mut a = [JS_NewString(ctx, cm.as_ptr())];
      let r = JS_Call(ctx, reject, jsv_undefined(), 1, a.as_mut_ptr());
      JS_FreeValue(ctx, r);
      JS_FreeValue(ctx, a[0]);
    }
  };

  let Some(cb) = DYN_IMPORT_CB.with(|c| c.get()) else {
    reject_with("dynamic import: no host callback");
    return;
  };

  let context = intern_ctx(ctx);
  let host_opts = intern::<Data>(jsv_undefined());
  let referrer = intern_dup::<Value>(ctx, basename);
  let spec_handle = intern_dup::<V8String>(ctx, specifier);
  let attrs_handle =
    intern::<FixedArray>(unsafe { build_dyn_attrs(ctx, attributes) });
  let (Some(ctx_l), Some(ho_l), Some(ref_l), Some(spec_l), Some(attr_l)) = (
    unsafe { crate::Local::from_raw(context) },
    unsafe { crate::Local::from_raw(host_opts) },
    unsafe { crate::Local::from_raw(referrer) },
    unsafe { crate::Local::from_raw(spec_handle) },
    unsafe { crate::Local::from_raw(attrs_handle) },
  ) else {
    reject_with("dynamic import: handle alloc failed");
    return;
  };

  let promise_ptr = unsafe { cb(ctx_l, ho_l, ref_l, spec_l, attr_l) };
  if promise_ptr.is_null() {
    if unsafe { JS_HasException(ctx) } {
      let mut a = [unsafe { JS_GetException(ctx) }];
      let r =
        unsafe { JS_Call(ctx, reject, jsv_undefined(), 1, a.as_mut_ptr()) };
      unsafe { JS_FreeValue(ctx, r) };
      unsafe { JS_FreeValue(ctx, a[0]) };
    } else {
      reject_with("dynamic import: host callback returned null");
    }
    return;
  }
  let d = jsval_of(promise_ptr as *const Value);

  let chain = DYN_IMPORT_CHAIN.with(|c| {
    if let Some(cur) = c.get() {
      return cur;
    }
    let src = c"(d,res,rej)=>{Promise.resolve(d).then(res,rej);}";
    let f = unsafe {
      JS_Eval(
        ctx,
        src.as_ptr(),
        src.to_bytes().len(),
        c"<dynimport-chain>".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      )
    };
    if f.tag != JS_TAG_EXCEPTION {
      c.set(Some(f));
    }
    f
  });
  if chain.tag == JS_TAG_EXCEPTION {
    reject_with("dynamic import: chain helper failed");
    return;
  }
  let mut args = [d, resolve, reject];
  let r = unsafe { JS_Call(ctx, chain, jsv_undefined(), 3, args.as_mut_ptr()) };
  unsafe { JS_FreeValue(ctx, r) };
}

unsafe fn populate_import_meta(
  ctx: *mut JSContext,
  def: *mut JSModuleDef,
  name: &str,
) {
  if def.is_null() || !IMPORT_META_ENABLED.with(|c| c.get()) {
    return;
  }

  // Preferred path: hand the meta object to deno's real
  // HostInitializeImportMetaObjectCallback. It populates url/main/filename/
  // dirname and installs `import.meta.resolve` backed by the embedder's loader —
  // none of which the native fallback below can do faithfully. Requires the
  // exact Module wrapper deno registered (matched by Global identity).
  if let Some(cb) = IMPORT_META_CB.with(|c| c.get()) {
    if let Some(wrapper_val) = lookup_module_wrapper(name) {
      // Re-intern a fresh scope-bound handle naming deno's registered wrapper
      // object (the stored handle's box may already have been freed).
      let wrapper = intern_dup::<Module>(ctx, wrapper_val);
      if wrapper.is_null() {
        return;
      }
      let meta = unsafe { JS_GetImportMeta(ctx, def) };
      if meta.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return;
      }
      let context = intern_ctx(ctx);
      let meta_handle = intern::<Object>(meta);
      let (Some(ctx_l), Some(mod_l), Some(meta_l)) = (
        unsafe { crate::Local::from_raw(context) },
        unsafe { crate::Local::from_raw(wrapper) },
        unsafe { crate::Local::from_raw(meta_handle) },
      ) else {
        return;
      };
      unsafe { cb(ctx_l, mod_l, meta_l) };
      // deno's callback may throw (e.g. WebAssembly-unavailable); clear so a
      // spurious pending exception doesn't poison the subsequent eval.
      if unsafe { JS_HasException(ctx) } {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
      }
      return;
    }
  }

  if !(name.starts_with("file://")
    || name.starts_with("http://")
    || name.starts_with("https://"))
  {
    return;
  }
  let meta = unsafe { JS_GetImportMeta(ctx, def) };
  if meta.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return;
  }
  if let Ok(curl) = CString::new(name) {
    let url_val = unsafe { JS_NewString(ctx, curl.as_ptr()) };
    unsafe { JS_SetPropertyStr(ctx, meta, c"url".as_ptr(), url_val) };

    // import.meta.resolve(spec): deno's host callback installs this; we bypass
    // the callback and populate import.meta natively, so add it here. Resolves
    // relative to this module's URL — matches deno for relative/absolute
    // specifiers (import-map / npm: resolution would need deno's resolver).
    let factory_src = c"(u)=>(s)=>new URL(s,u).href";
    let factory = unsafe {
      JS_Eval(
        ctx,
        factory_src.as_ptr(),
        factory_src.to_bytes().len(),
        c"<import-meta-resolve>".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      )
    };
    if factory.tag != JS_TAG_EXCEPTION {
      let mut arg = [unsafe { JS_NewString(ctx, curl.as_ptr()) }];
      let resolve_fn =
        unsafe { JS_Call(ctx, factory, jsv_undefined(), 1, arg.as_mut_ptr()) };
      unsafe { JS_FreeValue(ctx, arg[0]) };
      if resolve_fn.tag != JS_TAG_EXCEPTION {
        unsafe {
          JS_SetPropertyStr(ctx, meta, c"resolve".as_ptr(), resolve_fn)
        };
      } else {
        unsafe { JS_FreeValue(ctx, JS_GetException(ctx)) };
      }
    } else {
      unsafe { JS_FreeValue(ctx, JS_GetException(ctx)) };
    }
    unsafe { JS_FreeValue(ctx, factory) };
  }
  let is_main = if is_main_module(name) { 1 } else { 0 };
  unsafe {
    JS_SetPropertyStr(ctx, meta, c"main".as_ptr(), JS_NewBool(ctx, is_main))
  };

  if name.ends_with(".wasm") {
    let global = unsafe { JS_GetGlobalObject(ctx) };
    let wasm =
      unsafe { JS_GetPropertyStr(ctx, global, c"WebAssembly".as_ptr()) };
    if jsv_is_object(&wasm) {
      let inst = unsafe { JS_GetPropertyStr(ctx, wasm, c"Instance".as_ptr()) };
      unsafe { JS_SetPropertyStr(ctx, meta, c"WasmInstance".as_ptr(), inst) };
    }
    unsafe { JS_FreeValue(ctx, wasm) };
    let src = c"new Map()";
    let map = unsafe {
      JS_Eval(
        ctx,
        src.as_ptr(),
        src.to_bytes().len(),
        c"<wasmmap>".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      )
    };
    if map.tag == JS_TAG_EXCEPTION {
      unsafe {
        let e = JS_GetException(ctx);
        JS_FreeValue(ctx, e);
      }
    } else {
      unsafe { JS_SetPropertyStr(ctx, meta, c"wasmInstances".as_ptr(), map) };
    }
    unsafe { JS_FreeValue(ctx, global) };
  }
  unsafe { JS_FreeValue(ctx, meta) };
}

fn lookup_resolved_specifier(
  base: &str,
  spec: &str,
) -> Option<std::string::String> {
  RESOLVED_SPECIFIERS.with(|t| {
    t.borrow()
      .get(&(base.to_string(), spec.to_string()))
      .cloned()
  })
}

fn record_resolved_specifier(base: &str, spec: &str, resolved: &str) {
  RESOLVED_SPECIFIERS.with(|t| {
    t.borrow_mut()
      .insert((base.to_string(), spec.to_string()), resolved.to_string());
  });
}

fn lookup_resolved_specifier_any(spec: &str) -> Option<std::string::String> {
  RESOLVED_SPECIFIERS.with(|t| {
    t.borrow()
      .iter()
      .find(|((_, s), _)| s == spec)
      .map(|(_, resolved)| resolved.clone())
  })
}

fn mark_all_modules_evaluated() {
  MODULE_STATE.with(|t| {
    for m in t.borrow_mut().values_mut() {
      m.status = ModuleStatus::Evaluated;
    }
  });
  AFTER_FIRST_EVAL.with(|f| f.set(true));
}

#[allow(dead_code)]
fn after_first_eval() -> bool {
  AFTER_FIRST_EVAL.with(|f| f.get())
}

#[inline]
fn handle_key(this: *const Module) -> usize {
  let v = jsval_of(this);
  unsafe { v.u.ptr as usize }
}

fn with_module_state<R>(
  this: *const Module,
  f: impl FnOnce(&mut ModuleState) -> R,
) -> Option<R> {
  let key = handle_key(this);
  MODULE_STATE.with(|t| t.borrow_mut().get_mut(&key).map(f))
}

fn record_module_state(this: *const Module, st: ModuleState) {
  let key = handle_key(this);
  MODULE_STATE.with(|t| {
    t.borrow_mut().insert(key, st);
  });
}

unsafe fn jsval_to_rust(
  ctx: *mut JSContext,
  v: JSValue,
) -> std::string::String {
  if ctx.is_null() {
    return std::string::String::new();
  }
  let mut len: usize = 0;
  let cstr = unsafe { JS_ToCStringLen(ctx, &mut len, v) };
  if cstr.is_null() {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return std::string::String::new();
  }
  let bytes = unsafe { std::slice::from_raw_parts(cstr as *const u8, len) };
  let s = std::string::String::from_utf8_lossy(bytes).into_owned();
  unsafe { JS_FreeCString(ctx, cstr) };
  s
}

fn parse_import_specifiers(
  src: &str,
) -> Vec<(std::string::String, Option<std::string::String>)> {
  let mut out = Vec::new();
  let bytes = src.as_bytes();
  let mut i = 0usize;
  let n = bytes.len();
  while i < n {
    while i < n && bytes[i].is_ascii_whitespace() {
      i += 1;
    }
    if i >= n {
      break;
    }

    if i + 1 < n && bytes[i] == b'/' && bytes[i + 1] == b'/' {
      while i < n && bytes[i] != b'\n' {
        i += 1;
      }
      continue;
    }

    if i + 1 < n && bytes[i] == b'/' && bytes[i + 1] == b'*' {
      i += 2;
      while i + 1 < n && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
        i += 1;
      }
      i += 2;
      continue;
    }

    if bytes[i] == b'"' || bytes[i] == b'\'' {
      i = skip_string(bytes, i, bytes[i]);
      continue;
    }
    if bytes[i] == b'`' {
      i = skip_template(bytes, i);
      continue;
    }

    let at_boundary = i == 0
      || matches!(
        bytes[i - 1],
        b' ' | b'\t' | b'\r' | b'\n' | b';' | b'{' | b'}'
      );
    if !at_boundary {
      i += 1;
      continue;
    }

    let rest = &src[i..];
    let is_import = rest.starts_with("import")
      && rest[6..]
        .chars()
        .next()
        .map(|c| {
          c == ' ' || c == '{' || c == '*' || c == '"' || c == '\'' || c == '('
        })
        .unwrap_or(false);

    let after_export = rest.get(6..).map(|s| s.trim_start()).unwrap_or("");
    let is_export = rest.starts_with("export")
      && (after_export.starts_with('*') || after_export.starts_with('{'));
    if is_import || is_export {
      let dynamic = rest.starts_with("import(") || rest.starts_with("import (");

      let mut j = i;
      let mut depth = 0i32;
      let mut end = n;
      while j < n {
        match bytes[j] {
          b'{' => depth += 1,
          b'}' => depth -= 1,
          b';' if depth <= 0 => {
            end = j;
            break;
          }
          b'\n' if depth <= 0 && j > i => {
            let seg = &src[i..j];
            if seg.contains(" from ") || seg.contains("\"") || seg.contains("'")
            {
              if has_balanced_quotes(seg) {
                end = j;
                break;
              }
            }
          }
          _ => {}
        }
        j += 1;
      }
      let stmt = &src[i..end.min(n)];
      if !dynamic {
        let has_from = find_from_specifier_start(stmt).is_some();

        let bare_immediate = stmt
          .strip_prefix("import")
          .map(|r| {
            let t = r.trim_start();
            t.starts_with('"') || t.starts_with('\'')
          })
          .unwrap_or(false);
        let bare = is_import && !has_from && bare_immediate;

        let bare_ok = bare && bare_import_well_formed(stmt);
        if has_from || bare_ok {
          if let Some(spec) = extract_specifier(stmt) {
            if !spec.is_empty() {
              let ty = extract_attr_type(stmt);
              if std::env::var_os("QJS_DEBUG_PARSE").is_some() {
                let snip: std::string::String = stmt.chars().take(80).collect();
                eprintln!(
                  "[QJS parse] spec={spec:?} type={ty:?} stmt={snip:?}"
                );
              }
              out.push((spec, ty));
            }
          }
        }
      }
      i = end.min(n) + 1;
      continue;
    }

    i += 1;
  }
  out
}

/// Byte offset of each specifier literal's opening quote in `text`, parallel to
/// `specifiers`. This is V8's `ModuleRequest::source_offset` /
/// deno's `referrer_source_offset`. A cursor advances past each match so a later
/// identical specifier resolves to its own occurrence.
fn compute_import_offsets(
  text: &str,
  specifiers: &[std::string::String],
) -> Vec<i32> {
  let mut cursor = 0usize;
  let mut out = Vec::with_capacity(specifiers.len());
  for spec in specifiers {
    let mut found = -1i32;
    for quote in ['"', '\''] {
      let needle = format!("{quote}{spec}{quote}");
      if let Some(rel) = text[cursor..].find(&needle) {
        let pos = cursor + rel;
        if found < 0 || (pos as i32) < found {
          found = pos as i32;
        }
      }
    }
    if found >= 0 {
      cursor = (found as usize + 1).min(text.len());
      out.push(found);
    } else {
      out.push(0);
    }
  }
  out
}

/// Full per-import `with { ... }` / `assert { ... }` attribute key/value pairs,
/// parallel to `specifiers`. QuickJS's loader doesn't expose attributes through
/// its C-ABI, so scan the clause that immediately follows each specifier literal
/// in the source. Best-effort, sufficient for deno's flat `key: "value"` form.
fn compute_import_attributes(
  text: &str,
  specifiers: &[std::string::String],
) -> Vec<Vec<(std::string::String, std::string::String)>> {
  let offsets = compute_import_offsets(text, specifiers);
  let mut out = Vec::with_capacity(specifiers.len());
  for (i, spec) in specifiers.iter().enumerate() {
    let mut attrs = Vec::new();
    let off = offsets[i];
    if off >= 0 {
      // Position just past the specifier's closing quote.
      let after = (off as usize + spec.len() + 2).min(text.len());
      let tail = text[after..].trim_start();
      let kw = ["with", "assert"].iter().find_map(|k| {
        tail.strip_prefix(*k).filter(|rest| {
          rest.starts_with(|c: char| c.is_whitespace() || c == '{')
        })
      });
      if let Some(rest) = kw {
        if let Some(open) = rest.find('{') {
          if let Some(close_rel) = rest[open + 1..].find('}') {
            let body = &rest[open + 1..open + 1 + close_rel];
            for pair in body.split(',') {
              if let Some((k, v)) = pair.split_once(':') {
                let key =
                  k.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
                let val =
                  v.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
                if !key.is_empty() {
                  attrs.push((key, val));
                }
              }
            }
          }
        }
      }
    }
    out.push(attrs);
  }
  out
}

thread_local! {
    static SRC_PHASE_COUNTER: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
}
fn next_src_phase_id() -> u64 {
  SRC_PHASE_COUNTER.with(|c| {
    let v = c.get();
    c.set(v + 1);
    v
  })
}

fn rewrite_source_phase(
  src: &str,
) -> (std::string::String, Vec<(u64, std::string::String)>) {
  if !src.contains("import source") {
    return (src.to_string(), Vec::new());
  }
  let bytes = src.as_bytes();
  let n = bytes.len();
  let mut out = std::string::String::with_capacity(src.len());
  let mut records = Vec::new();
  let mut i = 0usize;
  let mut last = 0usize;
  while i < n {
    let boundary = i == 0
      || matches!(
        bytes[i - 1],
        b'\n' | b'\r' | b';' | b'{' | b'}' | b' ' | b'\t'
      );
    if boundary && src[i..].starts_with("import") {
      if let Some((consumed, binding, spec)) = parse_source_phase_at(&src[i..])
      {
        out.push_str(&src[last..i]);
        let id = next_src_phase_id();
        out.push_str(&format!(
          "const {binding}=globalThis.__v82jsc_wasm_src.get({id});"
        ));
        records.push((id, spec));
        i += consumed;
        last = i;
        continue;
      }
    }
    i += 1;
  }
  out.push_str(&src[last..]);
  (out, records)
}

fn parse_source_phase_at(
  s: &str,
) -> Option<(usize, std::string::String, std::string::String)> {
  let b = s.as_bytes();
  let n = b.len();
  let skip_ws = |p: usize| {
    let mut p = p;
    while p < n && b[p].is_ascii_whitespace() {
      p += 1;
    }
    p
  };
  if !s.starts_with("import") {
    return None;
  }
  let mut p = 6;
  let p1 = skip_ws(p);
  if p1 == p {
    return None;
  }
  p = p1;
  if !s[p..].starts_with("source") {
    return None;
  }
  p += 6;
  let p2 = skip_ws(p);
  if p2 == p {
    return None;
  }
  p = p2;
  let bstart = p;
  while p < n && (b[p].is_ascii_alphanumeric() || b[p] == b'_' || b[p] == b'$')
  {
    p += 1;
  }
  if p == bstart {
    return None;
  }
  let binding = s[bstart..p].to_string();
  p = skip_ws(p);
  if !s[p..].starts_with("from") {
    return None;
  }
  p += 4;
  p = skip_ws(p);
  if p >= n {
    return None;
  }
  let q = b[p];
  if q != b'"' && q != b'\'' {
    return None;
  }
  p += 1;
  let sstart = p;
  while p < n && b[p] != q {
    p += 1;
  }
  if p >= n {
    return None;
  }
  let spec = s[sstart..p].to_string();
  p += 1;
  if p < n && b[p] == b';' {
    p += 1;
  }
  Some((p, binding, spec))
}

fn skip_string(bytes: &[u8], i: usize, quote: u8) -> usize {
  let n = bytes.len();
  let mut j = i + 1;
  while j < n {
    match bytes[j] {
      b'\\' => j += 2,
      c if c == quote => return j + 1,
      b'\n' => return j,
      _ => j += 1,
    }
  }
  n
}

fn skip_template(bytes: &[u8], i: usize) -> usize {
  let n = bytes.len();
  let mut j = i + 1;
  while j < n {
    match bytes[j] {
      b'\\' => j += 2,
      b'`' => return j + 1,
      b'$' if j + 1 < n && bytes[j + 1] == b'{' => {
        let mut depth = 1i32;
        j += 2;
        while j < n && depth > 0 {
          match bytes[j] {
            b'{' => {
              depth += 1;
              j += 1;
            }
            b'}' => {
              depth -= 1;
              j += 1;
            }
            b'"' | b'\'' => j = skip_string(bytes, j, bytes[j]),
            b'`' => j = skip_template(bytes, j),
            b'\\' => j += 2,
            _ => j += 1,
          }
        }
      }
      _ => j += 1,
    }
  }
  n
}

fn bare_import_well_formed(stmt: &str) -> bool {
  let b = stmt.as_bytes();

  let after_kw = match stmt.strip_prefix("import") {
    Some(r) => r,
    None => return false,
  };
  let lead_ws =
    r#"import"#.len() + (after_kw.len() - after_kw.trim_start().len());
  let q = match b.get(lead_ws) {
    Some(&c) if c == b'"' || c == b'\'' => c,
    _ => return false,
  };

  let mut j = lead_ws + 1;
  while j < b.len() {
    match b[j] {
      b'\\' => j += 2,
      c if c == q => break,
      b'\n' => return false,
      _ => j += 1,
    }
  }
  if j >= b.len() {
    return false;
  }

  let mut k = j + 1;
  while k < b.len() && (b[k] == b' ' || b[k] == b'\t' || b[k] == b'\r') {
    k += 1;
  }
  match b.get(k) {
    None => true,
    Some(&c) if c == b';' || c == b'\n' => true,
    _ => stmt[k..].starts_with("with") || stmt[k..].starts_with("assert"),
  }
}

fn has_balanced_quotes(s: &str) -> bool {
  let dq = s.matches('"').count();
  let sq = s.matches('\'').count();
  dq % 2 == 0 && sq % 2 == 0 && (dq + sq) >= 2
}

fn extract_specifier(line: &str) -> Option<std::string::String> {
  let scan = match find_from_specifier_start(line) {
    Some(p) => &line[p..],
    None => line,
  };
  let bytes = scan.as_bytes();
  let mut open = None;
  let mut q = b'"';
  for (i, &b) in bytes.iter().enumerate() {
    if b == b'"' || b == b'\'' {
      open = Some(i);
      q = b;
      break;
    }
  }
  let open = open?;
  for i in (open + 1)..bytes.len() {
    if bytes[i] == q {
      return Some(scan[open + 1..i].to_string());
    }
  }
  None
}

fn find_from_specifier_start(stmt: &str) -> Option<usize> {
  let b = stmt.as_bytes();
  let mut result = None;
  let mut idx = 0;
  while let Some(p) = stmt[idx..].find("from") {
    let at = idx + p;
    idx = at + 4;
    let prev_ok = at == 0 || {
      let c = b[at - 1];
      !(c.is_ascii_alphanumeric() || c == b'_' || c == b'$')
    };
    let post_ident = b
      .get(at + 4)
      .map(|&c| c.is_ascii_alphanumeric() || c == b'_' || c == b'$')
      .unwrap_or(false);
    let after = stmt[at + 4..].trim_start();
    let next_quote = after.starts_with('"') || after.starts_with('\'');
    if prev_ok && !post_ident && next_quote {
      result = Some(at + 4);
    }
  }
  result
}

fn extract_attr_type(stmt: &str) -> Option<std::string::String> {
  let kw = stmt
    .find(" with ")
    .or_else(|| stmt.find(" with{"))
    .or_else(|| stmt.find(" assert "))
    .or_else(|| stmt.find(" assert{"))?;
  let clause = &stmt[kw..];

  let tpos = clause.find("type")?;
  let after = &clause[tpos + 4..];
  let bytes = after.as_bytes();
  let mut open = None;
  let mut q = b'"';
  for (i, &b) in bytes.iter().enumerate() {
    if b == b'"' || b == b'\'' {
      open = Some(i);
      q = b;
      break;
    }
  }
  let open = open?;
  for i in (open + 1)..bytes.len() {
    if bytes[i] == q {
      return Some(after[open + 1..i].to_string());
    }
  }
  None
}

fn has_top_level_await(src: &str) -> bool {
  for line in src.lines() {
    let t = line.trim_start();
    if (t.starts_with("await ") || t.starts_with("for await"))
      && !line.starts_with("  ")
    {
      return true;
    }
  }
  false
}

thread_local! {

    static JOBS_DRAINING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

unsafe fn drain_jobs(rt: *mut JSRuntime) {
  if rt.is_null() {
    return;
  }

  if JOBS_DRAINING.with(|d| d.replace(true)) {
    if std::env::var("V82_WASM_TRACE").is_ok() {
      eprintln!("[jobs] re-entrant drain_jobs BLOCKED");
    }
    return;
  }
  loop {
    let mut pctx: *mut JSContext = ptr::null_mut();
    let r = unsafe { JS_ExecutePendingJob(rt, &mut pctx) };
    if r <= 0 {
      break;
    }
  }
  JOBS_DRAINING.with(|d| d.set(false));
}

pub(crate) fn register_module_source(name: &str, source: &str) {
  if name.is_empty() {
    return;
  }
  MODULE_SOURCES_BY_NAME.with(|t| {
    t.borrow_mut().insert(name.to_string(), source.to_string());
  });
}

fn lookup_module_source_by_name(name: &str) -> Option<std::string::String> {
  MODULE_SOURCES_BY_NAME.with(|t| t.borrow().get(name).cloned())
}

pub(crate) unsafe extern "C" fn module_normalize_callback(
  ctx: *mut JSContext,
  module_base_name: *const std::os::raw::c_char,
  module_name: *const std::os::raw::c_char,
  _opaque: *mut std::os::raw::c_void,
) -> *mut std::os::raw::c_char {
  let base = unsafe { std::ffi::CStr::from_ptr(module_base_name) }
    .to_str()
    .unwrap_or("");
  let name = unsafe { std::ffi::CStr::from_ptr(module_name) }
    .to_str()
    .unwrap_or("");

  let mut canonical = lookup_resolved_specifier(base, name);

  if canonical.is_none() && (name.starts_with("./") || name.starts_with("../"))
  {
    canonical = Some(resolve_relative_specifier(base, name));
  }

  if canonical.is_none()
    && !name.contains(':')
    && !name.starts_with('.')
    && !name.starts_with('/')
  {
    let node_name = format!("node:{name}");

    let cname = CString::new(node_name.as_str()).ok();
    let loaded = cname
      .as_ref()
      .map(|c| unsafe { v82jsc_has_loaded_module(ctx, c.as_ptr()) != 0 })
      .unwrap_or(false);
    if loaded || lookup_module_source_by_name(&node_name).is_some() {
      canonical = Some(node_name);
    }
  }

  if canonical.is_none() && !name.starts_with('.') && !name.starts_with('/') {
    canonical = lookup_resolved_specifier_any(name);
  }
  if std::env::var_os("QJS_DEBUG_MOD").is_some() {
    eprintln!(
      "[QJS normalize] base={base} name={name} -> {}",
      canonical.as_deref().unwrap_or("(identity)")
    );
  }
  match canonical {
    Some(resolved) => match CString::new(resolved) {
      Ok(c) => unsafe { js_strdup(ctx, c.as_ptr()) },
      Err(_) => unsafe { js_strdup(ctx, module_name) },
    },
    None => unsafe { js_strdup(ctx, module_name) },
  }
}

fn resolve_relative_specifier(base: &str, name: &str) -> std::string::String {
  let (scheme, base_path) = match base.find("://") {
    Some(i) => base.split_at(i + 3),
    None => ("", base),
  };

  let dir = match base_path.rfind('/') {
    Some(i) => &base_path[..i],
    None => "",
  };
  let mut segments: Vec<&str> =
    dir.split('/').filter(|s| !s.is_empty()).collect();
  for seg in name.split('/') {
    match seg {
      "" | "." => {}
      ".." => {
        segments.pop();
      }
      s => segments.push(s),
    }
  }

  let joined = segments.join("/");
  if base_path.starts_with('/') {
    format!("{scheme}/{joined}")
  } else {
    format!("{scheme}{joined}")
  }
}

unsafe fn build_resolution_map(
  ctx: *mut JSContext,
  context: *const Context,
  cb: ResolveModuleCallback,
  source_cb: Option<ResolveSourceCallback>,
  root: *const Module,
) {
  use std::collections::HashSet;
  let mut visited: HashSet<usize> = HashSet::new();
  let mut stack: Vec<*const Module> = vec![root];
  while let Some(m) = stack.pop() {
    if !visited.insert(handle_key(m)) {
      continue;
    }
    let Some((base, specs, src_imports)) = with_module_state(m, |st| {
      (
        st.source_name.clone(),
        st.import_specifiers.clone(),
        st.source_imports.clone(),
      )
    }) else {
      continue;
    };

    if let Some(scb) = source_cb {
      for (id, spec) in &src_imports {
        unsafe { resolve_source_import(ctx, context, scb, m, *id, spec) };
      }
    }
    for (spec, _ty) in specs {
      let Ok(cspec) = CString::new(spec.as_str()) else {
        continue;
      };
      let sval = unsafe { JS_NewString(ctx, cspec.as_ptr()) };
      if sval.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        continue;
      }
      let spec_handle = intern::<V8String>(sval);
      let attrs_handle = intern::<FixedArray>(unsafe { JS_NewArray(ctx) });
      if spec_handle.is_null() || attrs_handle.is_null() {
        continue;
      }

      let (
        Some(ctx_local),
        Some(spec_local),
        Some(attrs_local),
        Some(ref_local),
      ) = (
        unsafe { crate::Local::from_raw(context) },
        unsafe { crate::Local::from_raw(spec_handle) },
        unsafe { crate::Local::from_raw(attrs_handle) },
        unsafe { crate::Local::from_raw(m) },
      )
      else {
        continue;
      };
      let ret = unsafe { cb(ctx_local, spec_local, attrs_local, ref_local) };

      let resolved: *const Module = unsafe { std::mem::transmute(ret) };
      if resolved.is_null() {
        continue;
      }
      if let Some(rname) =
        with_module_state(resolved, |st| st.source_name.clone())
      {
        if !rname.is_empty() && rname != spec {
          record_resolved_specifier(&base, &spec, &rname);
        }
        stack.push(resolved);
      }
    }
  }
}

unsafe fn resolve_source_import(
  ctx: *mut JSContext,
  context: *const Context,
  scb: ResolveSourceCallback,
  m: *const Module,
  id: u64,
  spec: &str,
) {
  let Ok(cspec) = CString::new(spec) else {
    return;
  };
  let sval = unsafe { JS_NewString(ctx, cspec.as_ptr()) };
  if sval.tag == JS_TAG_EXCEPTION {
    unsafe {
      let e = JS_GetException(ctx);
      JS_FreeValue(ctx, e);
    }
    return;
  }
  let spec_handle = intern::<V8String>(sval);
  let attrs_handle = intern::<FixedArray>(unsafe { JS_NewArray(ctx) });
  let (Some(ctx_local), Some(spec_local), Some(attrs_local), Some(ref_local)) = (
    unsafe { crate::Local::from_raw(context) },
    unsafe { crate::Local::from_raw(spec_handle) },
    unsafe { crate::Local::from_raw(attrs_handle) },
    unsafe { crate::Local::from_raw(m) },
  ) else {
    return;
  };
  let ret = unsafe { scb(ctx_local, spec_local, attrs_local, ref_local) };

  let obj: *const Object = unsafe { std::mem::transmute(ret) };
  if obj.is_null() {
    if std::env::var_os("QJS_TRACE_MOD").is_some() {
      eprintln!("[src-phase] resolve({spec}) -> null");
    }
    return;
  }
  let obj_val = jsval_of(obj);

  unsafe {
    let global = JS_GetGlobalObject(ctx);
    let mut map = JS_GetPropertyStr(ctx, global, c"__v82jsc_wasm_src".as_ptr());
    if !jsv_is_object(&map) {
      JS_FreeValue(ctx, map);
      let src = c"(globalThis.__v82jsc_wasm_src=new Map())";
      map = JS_Eval(
        ctx,
        src.as_ptr(),
        src.to_bytes().len(),
        c"<wasmsrc>".as_ptr(),
        JS_EVAL_TYPE_GLOBAL,
      );
    }
    let set_fn = JS_GetPropertyStr(ctx, map, c"set".as_ptr());
    let mut args = [JS_NewInt64(ctx, id as i64), JS_DupValue(ctx, obj_val)];
    let r = JS_Call(ctx, set_fn, map, 2, args.as_mut_ptr());
    if r.tag == JS_TAG_EXCEPTION {
      let e = JS_GetException(ctx);
      JS_FreeValue(ctx, e);
    } else {
      JS_FreeValue(ctx, r);
    }
    JS_FreeValue(ctx, args[0]);
    JS_FreeValue(ctx, args[1]);
    JS_FreeValue(ctx, set_fn);
    JS_FreeValue(ctx, map);
    JS_FreeValue(ctx, global);
    if std::env::var_os("QJS_TRACE_MOD").is_some() {
      eprintln!("[src-phase] resolve({spec}) -> stored id={id}");
    }
  }
}

pub(crate) unsafe extern "C" fn module_loader_callback(
  ctx: *mut JSContext,
  module_name: *const std::os::raw::c_char,
  _opaque: *mut std::os::raw::c_void,
) -> *mut JSModuleDef {
  let name = match unsafe { std::ffi::CStr::from_ptr(module_name) }.to_str() {
    Ok(s) => s,
    Err(_) => return ptr::null_mut(),
  };
  let Some(source) = lookup_module_source_by_name(name) else {
    if std::env::var_os("QJS_DEBUG_EXC").is_some() {
      eprintln!("[QJS module loader] no source for {name}");
    }
    return ptr::null_mut();
  };
  let Ok(src_c) = CString::new(source.clone()) else {
    return ptr::null_mut();
  };
  let Ok(name_c) = CString::new(name) else {
    return ptr::null_mut();
  };

  let existing = unsafe { v82jsc_get_loaded_module(ctx, name_c.as_ptr()) };
  if std::env::var_os("QJS_DEBUG_MOD").is_some() && name.contains("stream") {
    eprintln!("[loader] {name} existing_loaded={}", !existing.is_null());
  }
  if !existing.is_null() {
    MODULE_DEF_CACHE.with(|c| {
      c.borrow_mut().insert(name.to_string(), existing as usize);
    });
    return existing;
  }

  let key = bc_key(&source);
  if let Some(bytes) = bc_load(key) {
    if std::env::var_os("QJS_DEBUG_MOD").is_some() && name.contains("stream") {
      eprintln!("[loader] {name} -> BYTECODE path");
    }
    let m = unsafe {
      JS_ReadObject(ctx, bytes.as_ptr(), bytes.len(), JS_READ_OBJ_BYTECODE)
    };
    if m.tag == JS_TAG_MODULE {
      let def = unsafe { m.u.ptr } as *mut JSModuleDef;
      MODULE_DEF_CACHE.with(|c| {
        c.borrow_mut().insert(name.to_string(), def as usize);
      });
      unsafe { populate_import_meta(ctx, def, name) };
      return def;
    }

    if m.tag == JS_TAG_EXCEPTION {
      let exc = unsafe { JS_GetException(ctx) };
      unsafe { JS_FreeValue(ctx, exc) };
    } else {
      unsafe { JS_FreeValue(ctx, m) };
    }
  }

  if std::env::var_os("QJS_DEBUG_MOD").is_some() {
    let already = unsafe { v82jsc_has_loaded_module(ctx, name_c.as_ptr()) };
    eprintln!("[loader compile fresh] {name} (already_loaded={already})");
  }
  let result = unsafe {
    JS_Eval(
      ctx,
      src_c.as_ptr(),
      src_c.as_bytes().len(),
      name_c.as_ptr(),
      JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_COMPILE_ONLY,
    )
  };
  if result.tag == JS_TAG_EXCEPTION {
    if std::env::var_os("QJS_DEBUG_EXC").is_some() {
      unsafe {
        let exc = JS_GetException(ctx);
        let mut l = 0usize;
        let s = JS_ToCStringLen(ctx, &mut l, exc);
        if !s.is_null() {
          let b = std::slice::from_raw_parts(s as *const u8, l);
          eprintln!(
            "[QJS module loader] parse failed for {name}: {}",
            std::string::String::from_utf8_lossy(b)
          );
          JS_FreeCString(ctx, s);
        }
        JS_FreeValue(ctx, exc);
      }
    }
    return ptr::null_mut();
  }

  unsafe { bc_write(ctx, key, result) };
  let m = unsafe { result.u.ptr } as *mut JSModuleDef;
  MODULE_DEF_CACHE.with(|c| {
    c.borrow_mut().insert(name.to_string(), m as usize);
  });
  unsafe { populate_import_meta(ctx, m, name) };

  m
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FixedArray__Length(this: *const FixedArray) -> int {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return 0;
  }
  let v = jsval_of(this);
  let len_val = unsafe { JS_GetPropertyStr(ctx, v, c"length".as_ptr()) };
  if len_val.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return 0;
  }
  let n = match len_val.tag {
    JS_TAG_INT => unsafe { len_val.u.int32 },
    JS_TAG_FLOAT64 => (unsafe { len_val.u.float64 }) as i32,
    _ => 0,
  };
  unsafe { JS_FreeValue(ctx, len_val) };
  n as int
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FixedArray__Get(
  this: *const FixedArray,
  index: int,
) -> *const Data {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() || index < 0 {
    return ptr::null();
  }
  let v = jsval_of(this);

  let elem = unsafe { JS_GetPropertyUint32(ctx, v, index as u32) };
  if elem.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  intern::<Data>(elem)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Script__GetUnboundScript(
  script: *const Script,
) -> *const UnboundScript {
  let ctx = current_ctx();
  intern_dup::<UnboundScript>(ctx, jsval_of(script))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptOrigin__CONSTRUCT(
  buf: *mut MaybeUninit<ScriptOrigin>,
  resource_name: *const Value,
  _resource_line_offset: i32,
  _resource_column_offset: i32,
  _resource_is_shared_cross_origin: bool,
  _script_id: i32,
  _source_map_url: *const Value,
  _resource_is_opaque: bool,
  _is_wasm: bool,
  _is_module: bool,
  _host_defined_options: *const Data,
) {
  if !buf.is_null() {
    unsafe {
      ptr::write_bytes(buf as *mut u8, 0u8, size_of::<ScriptOrigin>());
      *(buf as *mut usize) = resource_name as usize;
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__Source__CONSTRUCT(
  buf: *mut MaybeUninit<Source>,
  source_string: *const V8String,
  origin: *const ScriptOrigin,
  _cached_data: *mut CachedData,
) {
  if buf.is_null() {
    return;
  }
  unsafe {
    ptr::write_bytes(buf as *mut u8, 0u8, size_of::<Source>());
    let slots = buf as *mut usize;
    *slots = source_string as usize;
    if !origin.is_null() {
      *slots.add(1) = *(origin as *const usize);
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__Source__DESTRUCT(_this: *mut Source) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__Source__GetCachedData<'a>(
  _this: *const Source,
) -> *const CachedData<'a> {
  ptr::null()
}

#[inline]
unsafe fn source_string_of(source: *mut Source) -> *const V8String {
  if source.is_null() {
    return ptr::null();
  }
  unsafe { *(source as *const usize) as *const V8String }
}

#[inline]
unsafe fn resource_name_of(
  ctx: *mut JSContext,
  source: *mut Source,
) -> std::string::String {
  if source.is_null() || ctx.is_null() {
    return std::string::String::new();
  }
  let name_ptr = unsafe { *((source as *const usize).add(1)) } as *const Value;
  if name_ptr.is_null() {
    return std::string::String::new();
  }
  let v = jsval_of(name_ptr);
  if jsv_is_undefined(&v) || jsv_is_null(&v) {
    return std::string::String::new();
  }
  unsafe { jsval_to_rust(ctx, v) }
}

#[repr(C)]
struct RawCachedData {
  data: *const u8,
  length: i32,
  rejected: bool,
  buffer_policy: i32,
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CachedData__NEW<'a>(
  data: *const u8,
  length: i32,
) -> *mut CachedData<'a> {
  let boxed = Box::new(RawCachedData {
    data,
    length,
    rejected: false,
    buffer_policy: 0,
  });
  Box::into_raw(boxed) as *mut CachedData<'a>
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CachedData__DELETE<'a>(
  this: *mut CachedData<'a>,
) {
  if this.is_null() {
    return;
  }
  unsafe {
    let raw = Box::from_raw(this as *mut RawCachedData);
    if raw.buffer_policy == 1 && !raw.data.is_null() && raw.length > 0 {
      let slice = std::slice::from_raw_parts_mut(
        raw.data as *mut u8,
        raw.length as usize,
      );
      drop(Box::from_raw(slice as *mut [u8]));
    }
    drop(raw);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__Compile(
  context: *const Context,
  source: *mut Source,
  _options: CompileOptions,
  _no_cache_reason: NoCacheReason,
) -> *const Script {
  let ctx = ctx_of(context);
  let src = unsafe { source_string_of(source) };
  if ctx.is_null() || src.is_null() {
    return ptr::null();
  }
  let src_val = jsval_of(src);
  let text = unsafe { jsval_to_rust(ctx, src_val) };
  let Ok(csrc) = CString::new(text) else {
    return ptr::null();
  };
  let len = csrc.as_bytes().len();
  let compiled = unsafe {
    JS_Eval(
      ctx,
      csrc.as_ptr(),
      len,
      c"<compile>".as_ptr(),
      JS_EVAL_TYPE_GLOBAL | JS_EVAL_FLAG_COMPILE_ONLY,
    )
  };
  if compiled.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  unsafe { JS_FreeValue(ctx, compiled) };

  intern_dup::<Script>(ctx, src_val)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CompileModule(
  isolate: *mut RealIsolate,
  source: *mut Source,
  _options: CompileOptions,
  _no_cache_reason: NoCacheReason,
) -> *const Module {
  if isolate.is_null() {
    return ptr::null();
  }
  let st = iso_state(isolate);
  let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
  let src = unsafe { source_string_of(source) };
  if ctx.is_null() || src.is_null() {
    return ptr::null();
  }
  let src_val = jsval_of(src);
  let raw_text = unsafe { jsval_to_rust(ctx, src_val) };

  let (text, source_imports) = rewrite_source_phase(&raw_text);
  let specifier = unsafe { resource_name_of(ctx, source) };
  let fname = if specifier.is_empty() {
    "<module>".to_string()
  } else {
    specifier.clone()
  };

  let import_specifiers = parse_import_specifiers(&text);
  let spec_strs: Vec<std::string::String> =
    import_specifiers.iter().map(|(s, _)| s.clone()).collect();
  let import_offsets = compute_import_offsets(&text, &spec_strs);
  let import_attributes = compute_import_attributes(&text, &spec_strs);
  let is_async = has_top_level_await(&text);

  register_module_source(&fname, &text);
  note_main_module(&fname);
  if std::env::var_os("QJS_DEBUG_MOD").is_some() {
    eprintln!("[QJS CompileModule] {fname} imports={import_specifiers:?}");
  }

  let handle_val = unsafe { JS_NewObject(ctx) };
  if handle_val.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  let this = intern::<Module>(handle_val);
  if this.is_null() {
    return ptr::null();
  }
  if !source_imports.is_empty() {
    PENDING_SOURCE_IMPORTS.with(|p| {
      p.borrow_mut().push((this, source_imports.clone()));
    });
  }
  record_module_state(
    this,
    ModuleState {
      status: ModuleStatus::Uninstantiated,
      module_def: ptr::null_mut(),
      bytecode: None,
      import_specifiers,
      import_offsets,
      import_attributes,
      source_imports,
      synthetic: false,
      is_async,
      source_text: text,
      source_name: fname.clone(),
    },
  );
  // Map name -> this wrapper so deno's import.meta callback can be handed the
  // exact handle it registered (it looks modules up by Global identity).
  record_module_wrapper(&fname, this);
  this
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CompileFunction(
  context: *const Context,
  source: *mut Source,
  arguments_count: usize,
  arguments: *const *const V8String,
  _context_extensions_count: usize,
  _context_extensions: *const *const Object,
  _options: CompileOptions,
  _no_cache_reason: NoCacheReason,
) -> *const Function {
  let ctx = ctx_of(context);
  let src = unsafe { source_string_of(source) };
  if ctx.is_null() || src.is_null() {
    if !ctx.is_null() {
      unsafe {
        JS_ThrowTypeError(ctx, c"compile_function: invalid source".as_ptr())
      };
    }
    return ptr::null();
  }
  let mut body = unsafe { jsval_to_rust(ctx, jsval_of(src)) };

  if body.starts_with("#!") {
    body.replace_range(0..2, "//");
  }

  let mut arg_names: Vec<std::string::String> = Vec::new();
  if !arguments.is_null() {
    for i in 0..arguments_count {
      let a = unsafe { *arguments.add(i) };
      if a.is_null() {
        continue;
      }
      arg_names.push(unsafe { jsval_to_rust(ctx, jsval_of(a)) });
    }
  }

  let wrapped = format!("(function({}) {{\n{}\n}})", arg_names.join(","), body);
  let Ok(csrc) = CString::new(wrapped) else {
    unsafe {
      JS_ThrowTypeError(ctx, c"compile_function: NUL in source".as_ptr())
    };
    return ptr::null();
  };
  let len = csrc.as_bytes().len();

  let name = unsafe { resource_name_of(ctx, source) };
  let name_c = CString::new(if name.is_empty() {
    "<function>".to_string()
  } else {
    name
  })
  .unwrap_or_else(|_| CString::new("<function>").unwrap());
  let result = unsafe {
    JS_Eval(
      ctx,
      csrc.as_ptr(),
      len,
      name_c.as_ptr(),
      JS_EVAL_TYPE_GLOBAL,
    )
  };
  if result.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }
  intern::<Function>(result)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundScript__CreateCodeCache(
  _script: *const UnboundScript,
) -> *mut CachedData<'static> {
  make_placeholder_code_cache()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundModuleScript__CreateCodeCache(
  _script: *const UnboundModuleScript,
) -> *mut CachedData<'static> {
  make_placeholder_code_cache()
}

pub(crate) fn make_placeholder_code_cache() -> *mut CachedData<'static> {
  #[repr(C)]
  struct RawCachedData {
    data: *const u8,
    length: i32,
    rejected: bool,
    buffer_policy: i32,
  }
  let v = vec![0u8; 1].into_boxed_slice();
  let len = v.len() as i32;
  let data = Box::into_raw(v) as *const u8;
  let boxed = Box::new(RawCachedData {
    data,
    length: len,
    rejected: false,
    buffer_policy: 1,
  });
  Box::into_raw(boxed) as *mut CachedData<'static>
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CompileUnboundScript(
  isolate: *mut RealIsolate,
  source: *mut Source,
  _options: CompileOptions,
  _no_cache_reason: NoCacheReason,
) -> *const UnboundScript {
  if isolate.is_null() {
    return ptr::null();
  }
  let st = crate::quickjs::core::iso_state(isolate);
  let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
  let src = unsafe { source_string_of(source) };
  if ctx.is_null() || src.is_null() {
    return ptr::null();
  }
  let src_val = jsval_of(src);
  let text = unsafe { jsval_to_rust(ctx, src_val) };
  let Ok(csrc) = CString::new(text) else {
    return ptr::null();
  };
  let len = csrc.as_bytes().len();
  let compiled = unsafe {
    JS_Eval(
      ctx,
      csrc.as_ptr(),
      len,
      c"<compile>".as_ptr(),
      JS_EVAL_TYPE_GLOBAL | JS_EVAL_FLAG_COMPILE_ONLY,
    )
  };
  if compiled.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  unsafe { JS_FreeValue(ctx, compiled) };
  intern_dup::<UnboundScript>(ctx, src_val)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundScript__BindToCurrentContext(
  script: *const UnboundScript,
) -> *const Script {
  if script.is_null() {
    return ptr::null();
  }
  let ctx = current_ctx();
  intern_dup::<Script>(ctx, jsval_of(script))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundScript__GetSourceMappingURL(
  _script: *const UnboundScript,
) -> *const Value {
  intern::<Value>(jsv_undefined())
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundModuleScript__GetSourceMappingURL(
  _script: *const UnboundModuleScript,
) -> *const Value {
  intern::<Value>(jsv_undefined())
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundModuleScript__GetSourceURL(
  _script: *const UnboundModuleScript,
) -> *const Value {
  intern::<Value>(jsv_undefined())
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetStatus(this: *const Module) -> ModuleStatus {
  with_module_state(this, |m| clone_status(&m.status))
    .unwrap_or(ModuleStatus::Errored)
}

fn clone_status(s: &ModuleStatus) -> ModuleStatus {
  match s {
    ModuleStatus::Uninstantiated => ModuleStatus::Uninstantiated,
    ModuleStatus::Instantiating => ModuleStatus::Instantiating,
    ModuleStatus::Instantiated => ModuleStatus::Instantiated,
    ModuleStatus::Evaluating => ModuleStatus::Evaluating,
    ModuleStatus::Evaluated => ModuleStatus::Evaluated,
    ModuleStatus::Errored => ModuleStatus::Errored,
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetException(
  _this: *const Module,
) -> *const Value {
  intern::<Value>(jsv_undefined())
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetModuleRequests(
  this: *const Module,
) -> *const FixedArray {
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }
  let (specs, offsets, attrs, src_imports) = with_module_state(this, |m| {
    (
      m.import_specifiers.clone(),
      m.import_offsets.clone(),
      m.import_attributes.clone(),
      m.source_imports.clone(),
    )
  })
  .unwrap_or_default();

  let arr = unsafe { JS_NewArray(ctx) };
  if arr.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  let mut idx = 0u32;

  for (_id, spec) in src_imports.iter() {
    let req = unsafe { JS_NewObject(ctx) };
    if req.tag == JS_TAG_EXCEPTION {
      unsafe { JS_FreeValue(ctx, req) };
      continue;
    }
    if let Ok(cspec) = CString::new(spec.as_str()) {
      let sval = unsafe { JS_NewString(ctx, cspec.as_ptr()) };
      unsafe { JS_SetPropertyStr(ctx, req, c"specifier".as_ptr(), sval) };
    }
    unsafe {
      JS_SetPropertyStr(ctx, req, c"__src_phase".as_ptr(), JS_NewBool(ctx, 1));
      JS_SetPropertyStr(
        ctx,
        req,
        c"__v8jsc_module_request".as_ptr(),
        JS_NewBool(ctx, 1),
      );
      JS_SetPropertyUint32(ctx, arr, idx, req);
    }
    idx += 1;
  }
  for (si, (spec, ty)) in specs.iter().enumerate() {
    let req = unsafe { JS_NewObject(ctx) };
    if req.tag == JS_TAG_EXCEPTION {
      unsafe { JS_FreeValue(ctx, req) };
      continue;
    }
    if let Ok(cspec) = CString::new(spec.as_str()) {
      let sval = unsafe { JS_NewString(ctx, cspec.as_ptr()) };

      unsafe { JS_SetPropertyStr(ctx, req, c"specifier".as_ptr(), sval) };
    }

    if let Some(t) = ty {
      if let Ok(ct) = CString::new(t.as_str()) {
        let tval = unsafe { JS_NewString(ctx, ct.as_ptr()) };
        unsafe { JS_SetPropertyStr(ctx, req, c"__attr_type".as_ptr(), tval) };
      }
    }

    // Byte offset of the specifier literal's opening quote (deno's
    // `referrer_source_offset`), surfaced via `GetSourceOffset`.
    let off = offsets.get(si).copied().unwrap_or(0);
    unsafe {
      JS_SetPropertyStr(
        ctx,
        req,
        c"__source_offset".as_ptr(),
        JS_NewInt32(ctx, off),
      );
    }

    // Full `with { ... }` attributes as a flat [k0, v0, k1, v1, ...] string
    // array, surfaced as `[k, v, source_offset]` triples by GetImportAttributes.
    if let Some(pairs) = attrs.get(si) {
      if !pairs.is_empty() {
        let kv = unsafe { JS_NewArray(ctx) };
        if kv.tag != JS_TAG_EXCEPTION {
          let mut ki = 0u32;
          for (k, v) in pairs.iter() {
            if let (Ok(ck), Ok(cv)) =
              (CString::new(k.as_str()), CString::new(v.as_str()))
            {
              unsafe {
                JS_SetPropertyUint32(
                  ctx,
                  kv,
                  ki,
                  JS_NewString(ctx, ck.as_ptr()),
                );
                JS_SetPropertyUint32(
                  ctx,
                  kv,
                  ki + 1,
                  JS_NewString(ctx, cv.as_ptr()),
                );
              }
              ki += 2;
            }
          }
          unsafe { JS_SetPropertyStr(ctx, req, c"__attr_kv".as_ptr(), kv) };
        } else {
          unsafe { JS_FreeValue(ctx, kv) };
        }
      }
    }

    unsafe {
      JS_SetPropertyStr(
        ctx,
        req,
        c"__v8jsc_module_request".as_ptr(),
        JS_NewBool(ctx, 1),
      );

      JS_SetPropertyUint32(ctx, arr, idx, req);
    }
    idx += 1;
  }
  intern::<FixedArray>(arr)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__SourceOffsetToLocation(
  _this: *const Module,
  _offset: int,
  out: *mut Location,
) {
  if !out.is_null() {
    unsafe { ptr::write_bytes(out as *mut u8, 0u8, size_of::<Location>()) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetModuleNamespace(
  this: *const Module,
) -> *const Value {
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }
  // Synthetic modules (e.g. dynamically-imported JSON) are `JS_NewCModule`s with
  // no `func_obj`; QuickJS's `JS_GetModuleNamespace` -> `js_build_module_ns`
  // dereferences `func_obj` and segfaults. Build the namespace object directly
  // from the recorded synthetic exports instead.
  if with_module_state(this, |m| m.synthetic).unwrap_or(false) {
    let ns = unsafe { JS_NewObject(ctx) };
    if let Some(def) =
      SYNTHETIC_DEFS.with(|t| t.borrow().get(&handle_key(this)).copied())
    {
      // Prefer the post-init copy (static path). For a dynamically-imported
      // synthetic module (e.g. JSON) the CModule init never ran, so run deno's
      // evaluation_steps to populate SYNTHETIC_EXPORTS and read that. We must NOT
      // call JS_SetModuleExport on the dynamic module — it was never
      // QuickJS-instantiated, so its export slots don't exist (segfault).
      let mut exports =
        SYNTHETIC_NS_EXPORTS.with(|t| t.borrow().get(&def).cloned());
      if exports.is_none() {
        unsafe { run_synthetic_eval_steps(ctx, def as *mut JSModuleDef) };
        exports = SYNTHETIC_EXPORTS.with(|t| t.borrow().get(&def).cloned());
      }
      if let Some(exports) = exports {
        for (name, val) in exports {
          if let Ok(c) = CString::new(name) {
            let dup = unsafe { JS_DupValue(ctx, val) };
            unsafe { JS_SetPropertyStr(ctx, ns, c.as_ptr(), dup) };
          }
        }
      }
    }
    return intern::<Value>(ns);
  }
  let mut def =
    with_module_state(this, |m| m.module_def).unwrap_or(ptr::null_mut());
  if def.is_null() {
    def = unsafe { materialize_module_def(ctx, this) };
  }
  if !def.is_null() {
    let ns = unsafe { JS_GetModuleNamespace(ctx, def) };
    if ns.tag != JS_TAG_EXCEPTION {
      return intern::<Value>(ns);
    }
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
  }

  let obj = unsafe { JS_NewObject(ctx) };
  intern::<Value>(obj)
}

unsafe fn materialize_module_def(
  ctx: *mut JSContext,
  this: *const Module,
) -> *mut JSModuleDef {
  let existing =
    with_module_state(this, |m| m.module_def).unwrap_or(ptr::null_mut());
  if !existing.is_null() {
    return existing;
  }
  let Some((source_text, source_name)) =
    with_module_state(this, |m| (m.source_text.clone(), m.source_name.clone()))
  else {
    return ptr::null_mut();
  };
  if source_text.is_empty() {
    return ptr::null_mut();
  }

  if !source_name.is_empty() {
    if let Ok(cn) = CString::new(source_name.clone()) {
      let loaded = unsafe { v82jsc_get_loaded_module(ctx, cn.as_ptr()) };
      if !loaded.is_null() {
        with_module_state(this, |m| m.module_def = loaded);
        MODULE_DEF_CACHE.with(|c| {
          c.borrow_mut().insert(source_name.clone(), loaded as usize);
        });

        let is_ev = unsafe { v82jsc_module_is_evaluated(loaded) };
        let ev_started = unsafe { v82jsc_module_eval_started(loaded) };
        if is_ev == 0 && ev_started == 0 {
          let mv = make_value(
            JS_TAG_MODULE,
            JSValueUnion {
              ptr: loaded as *mut std::os::raw::c_void,
            },
          );
          let mv = unsafe { JS_DupValue(ctx, mv) };
          let result = unsafe { JS_EvalFunction(ctx, mv) };
          let iso2 = current_iso();
          let rt2 = if iso2.is_null() {
            ptr::null_mut()
          } else {
            iso_state(iso2).rt
          };
          unsafe { drain_jobs(rt2) };
          if result.tag == JS_TAG_EXCEPTION {
            let exc = unsafe { JS_GetException(ctx) };
            unsafe { JS_FreeValue(ctx, exc) };
          } else {
            unsafe { JS_FreeValue(ctx, result) };
          }
        }
        return loaded;
      }
    }
  }
  let iso = current_iso();
  let rt = if iso.is_null() {
    ptr::null_mut()
  } else {
    iso_state(iso).rt
  };
  let key = bc_key(&source_text);
  let Ok(cname) = CString::new(if source_name.is_empty() {
    "<module>".to_string()
  } else {
    source_name
  }) else {
    return ptr::null_mut();
  };

  let mut module_val: Option<JSValue> = None;
  if let Some(bytes) = bc_load(key) {
    let m = unsafe {
      JS_ReadObject(ctx, bytes.as_ptr(), bytes.len(), JS_READ_OBJ_BYTECODE)
    };
    if m.tag == JS_TAG_MODULE {
      module_val = Some(m);
    } else if m.tag == JS_TAG_EXCEPTION {
      let exc = unsafe { JS_GetException(ctx) };
      unsafe { JS_FreeValue(ctx, exc) };
    } else {
      unsafe { JS_FreeValue(ctx, m) };
    }
  }
  if module_val.is_none() {
    if let Ok(csrc) = CString::new(source_text.clone()) {
      let c = unsafe {
        JS_Eval(
          ctx,
          csrc.as_ptr(),
          csrc.as_bytes().len(),
          cname.as_ptr(),
          JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_COMPILE_ONLY,
        )
      };
      if c.tag == JS_TAG_MODULE {
        unsafe { bc_write(ctx, key, c) };
        module_val = Some(c);
      } else if c.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
      } else {
        unsafe { JS_FreeValue(ctx, c) };
      }
    }
  }
  let Some(mv) = module_val else {
    return ptr::null_mut();
  };

  let def = unsafe { mv.u.ptr } as *mut JSModuleDef;
  let meta_name = with_module_state(this, |m| {
    m.module_def = def;
    m.status = ModuleStatus::Evaluated;
    m.source_name.clone()
  })
  .unwrap_or_default();
  unsafe { populate_import_meta(ctx, def, &meta_name) };
  let result = unsafe { JS_EvalFunction(ctx, mv) };
  unsafe { drain_jobs(rt) };
  if result.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    if std::env::var_os("QJS_DEBUG_MOD").is_some() {
      let s = unsafe { jsval_to_rust(ctx, exc) };
      eprintln!(
        "[qjs materialize_module {}] exception: {s}",
        cname.to_string_lossy()
      );
    }
    unsafe { JS_FreeValue(ctx, exc) };
  } else {
    unsafe { JS_FreeValue(ctx, result) };
  }
  def
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetModuleNamespace2(
  this: *const Module,
  _phase: ModuleImportPhase,
) -> *const Value {
  v8__Module__GetModuleNamespace(this)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__EvaluateForImportDefer(
  _this: *const Module,
  _context: *const Context,
) -> *const Value {
  ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetIdentityHash(this: *const Module) -> int {
  (handle_key(this) as int) ^ 0x4d4f_44
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__InstantiateModule(
  this: *const Module,
  context: *const Context,
  cb: ResolveModuleCallback,
  source_callback: Option<ResolveSourceCallback>,
) -> MaybeBool {
  if let Some(scb) = source_callback {
    SOURCE_CB.with(|c| c.set(Some(unsafe { std::mem::transmute(scb) })));
  }
  let ctx = ctx_of(context);
  if !ctx.is_null() {
    unsafe { build_resolution_map(ctx, context, cb, source_callback, this) };

    if let Some(scb) = source_callback {
      let pending: Vec<_> =
        PENDING_SOURCE_IMPORTS.with(|p| p.borrow_mut().drain(..).collect());
      for (referrer, imports) in pending {
        for (id, spec) in &imports {
          unsafe {
            resolve_source_import(ctx, context, scb, referrer, *id, spec)
          };
        }
      }
    }
  }

  match with_module_state(this, |m| {
    if matches!(m.status, ModuleStatus::Uninstantiated) {
      m.status = ModuleStatus::Instantiated;
    }
    true
  }) {
    Some(true) => MaybeBool::JustTrue,
    _ => MaybeBool::JustFalse,
  }
}

fn make_resolved_promise(ctx: *mut JSContext) -> *const Value {
  let global = unsafe { JS_GetGlobalObject(ctx) };
  let promise_ctor =
    unsafe { JS_GetPropertyStr(ctx, global, c"Promise".as_ptr()) };
  let resolve_fn =
    unsafe { JS_GetPropertyStr(ctx, promise_ctor, c"resolve".as_ptr()) };
  let r = unsafe { JS_Call(ctx, resolve_fn, promise_ctor, 0, ptr::null_mut()) };
  unsafe {
    JS_FreeValue(ctx, resolve_fn);
    JS_FreeValue(ctx, promise_ctor);
    JS_FreeValue(ctx, global);
  }
  if r.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return intern::<Value>(jsv_undefined());
  }
  intern::<Value>(r)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__Evaluate(
  this: *const Module,
  context: *const Context,
) -> *const Value {
  let ctx = ctx_of(context);
  if ctx.is_null() {
    return ptr::null();
  }
  if std::env::var_os("QJS_DEBUG_EVAL").is_some() {
    let nm =
      with_module_state(this, |m| m.source_name.clone()).unwrap_or_default();
    let md =
      with_module_state(this, |m| !m.module_def.is_null()).unwrap_or(false);
    eprintln!(
      "[EVAL-ENTRY] this={:?} name={nm:?} has_module_def={md}",
      this
    );
  }
  let iso = current_iso();
  let rt = if iso.is_null() {
    ptr::null_mut()
  } else {
    iso_state(iso).rt
  };
  let _t0 = if std::env::var_os("V82JSC_TIMING").is_some() {
    Some(std::time::Instant::now())
  } else {
    None
  };

  let already_evaluated = with_module_state(this, |m| {
    if !m.module_def.is_null() {
      m.status = ModuleStatus::Evaluated;
      true
    } else {
      false
    }
  })
  .unwrap_or(false);
  if already_evaluated {
    return make_resolved_promise(ctx);
  }

  let (bytecode, source_text, source_name) = with_module_state(this, |m| {
    m.status = ModuleStatus::Evaluated;
    (
      m.bytecode.take(),
      m.source_text.clone(),
      m.source_name.clone(),
    )
  })
  .unwrap_or((None, std::string::String::new(), std::string::String::new()));
  let source_name_dbg = source_name.clone();

  if !source_name.is_empty() {
    let cached =
      MODULE_DEF_CACHE.with(|c| c.borrow().get(&source_name).copied());
    if std::env::var_os("QJS_DEBUG_MOD").is_some()
      && source_name.contains("stream")
    {
      let (isev, evst) = match cached {
        Some(d) => unsafe {
          (
            v82jsc_module_is_evaluated(d as *mut JSModuleDef),
            v82jsc_module_eval_started(d as *mut JSModuleDef),
          )
        },
        None => (-1, -1),
      };
      eprintln!(
        "[STREAM-EVAL] {source_name} cached={} is_evaluated={isev} eval_started={evst} bytecode={}",
        cached.is_some(),
        bytecode.is_some()
      );
    }

    if let Some(d) = cached {
      let def = d as *mut JSModuleDef;

      if unsafe { v82jsc_module_is_evaluated(def) } == 0
        && unsafe { v82jsc_module_eval_started(def) } != 0
      {
        with_module_state(this, |m| m.module_def = def);
        return make_resolved_promise(ctx);
      }
      if unsafe { v82jsc_module_is_evaluated(def) } == 0 {
      } else {
        with_module_state(this, |m| m.module_def = def);
        let mv = make_value(
          JS_TAG_MODULE,
          JSValueUnion {
            ptr: def as *mut std::os::raw::c_void,
          },
        );
        let mv = unsafe { JS_DupValue(ctx, mv) };
        let result = unsafe { JS_EvalFunction(ctx, mv) };
        unsafe { drain_jobs(rt) };
        if result.tag == JS_TAG_EXCEPTION {
          let exc = unsafe { JS_GetException(ctx) };
          if std::env::var_os("QJS_DEBUG_MOD").is_some() {
            let s = unsafe { jsval_to_rust(ctx, exc) };
            eprintln!("[qjs Evaluate reuse {source_name}] exception: {s}");
          }
          unsafe { JS_FreeValue(ctx, exc) };
          return make_resolved_promise(ctx);
        }
        return intern::<Value>(result);
      }
    }
  }

  if std::env::var_os("QJS_DEBUG_MOD").is_some() {
    eprintln!(
      "[QJS Evaluate] {} (precompiled={})",
      source_name,
      bytecode.is_some()
    );
  }

  let mut async_promise: Option<JSValue> = None;
  if let Some(bc) = bytecode {
    let result = unsafe { JS_EvalFunction(ctx, bc) };
    unsafe { drain_jobs(rt) };
    if result.tag == JS_TAG_EXCEPTION {
      let exc = unsafe { JS_GetException(ctx) };
      if !iso.is_null() {
        let s = unsafe { jsval_to_rust(ctx, exc) };
        if !s.is_empty() {
          eprintln!("[qjs] Module::evaluate exception: {s}");
        }
      }
      unsafe { JS_FreeValue(ctx, exc) };
    } else if result.tag == JS_TAG_OBJECT
      && unsafe { JS_IsPromise(result) }
      && unsafe { JS_PromiseState(ctx, result) } == 0
    {
      async_promise = Some(result);
    } else {
      unsafe { JS_FreeValue(ctx, result) };
    }
  } else if !source_text.is_empty() {
    let key = bc_key(&source_text);
    let cname = CString::new(if source_name.is_empty() {
      "<module>".to_string()
    } else {
      source_name
    })
    .ok();
    if let Some(cname) = cname {
      let mut module_val: Option<JSValue> = None;
      if let Some(bytes) = bc_load(key) {
        let m = unsafe {
          JS_ReadObject(ctx, bytes.as_ptr(), bytes.len(), JS_READ_OBJ_BYTECODE)
        };
        if m.tag == JS_TAG_MODULE {
          module_val = Some(m);
        } else if m.tag == JS_TAG_EXCEPTION {
          let exc = unsafe { JS_GetException(ctx) };
          unsafe { JS_FreeValue(ctx, exc) };
        } else {
          unsafe { JS_FreeValue(ctx, m) };
        }
      }
      if module_val.is_none() {
        if let Ok(csrc) = CString::new(source_text.clone()) {
          let c = unsafe {
            JS_Eval(
              ctx,
              csrc.as_ptr(),
              csrc.as_bytes().len(),
              cname.as_ptr(),
              JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_COMPILE_ONLY,
            )
          };
          if c.tag == JS_TAG_MODULE {
            unsafe { bc_write(ctx, key, c) };
            module_val = Some(c);
          } else if c.tag == JS_TAG_EXCEPTION {
            let exc = unsafe { JS_GetException(ctx) };
            unsafe { JS_FreeValue(ctx, exc) };
          } else {
            unsafe { JS_FreeValue(ctx, c) };
          }
        }
      }

      let result = if let Some(mv) = module_val {
        let def = unsafe { mv.u.ptr } as *mut JSModuleDef;
        with_module_state(this, |m| m.module_def = def);
        unsafe { populate_import_meta(ctx, def, &source_name_dbg) };
        unsafe { JS_EvalFunction(ctx, mv) }
      } else if let Ok(csrc) = CString::new(source_text.clone()) {
        unsafe {
          JS_Eval(
            ctx,
            csrc.as_ptr(),
            csrc.as_bytes().len(),
            cname.as_ptr(),
            JS_EVAL_TYPE_MODULE,
          )
        }
      } else {
        jsv_undefined()
      };

      if std::env::var_os("QJS_DBG_UNINIT").is_some() {
        let he = unsafe { JS_HasException(ctx) };
        eprintln!("[eval-result] tag={} has_exception={he}", result.tag);
      }
      let sync_exc = if result.tag == JS_TAG_EXCEPTION {
        Some(unsafe { JS_GetException(ctx) })
      } else {
        None
      };
      unsafe { drain_jobs(rt) };
      if let Some(exc) = sync_exc {
        let s = unsafe { jsval_to_rust(ctx, exc) };
        if !s.is_empty() {
          eprintln!(
            "[qjs] Module::evaluate (deferred) exception: {s} (module={source_name_dbg})"
          );
          if std::env::var_os("QJS_DEBUG_MOD").is_some() {
            let stk = unsafe { JS_GetPropertyStr(ctx, exc, c"stack".as_ptr()) };
            if !jsv_is_undefined(&stk) {
              let ss = unsafe { jsval_to_rust(ctx, stk) };
              eprintln!("[qjs] stack:\n{ss}");
            }
            unsafe { JS_FreeValue(ctx, stk) };
          }
        }
        unsafe { JS_FreeValue(ctx, exc) };
      } else {
        if std::env::var_os("QJS_DEBUG_MOD").is_some() {
          if result.tag == JS_TAG_OBJECT && unsafe { JS_IsPromise(result) } {
            let state = unsafe { JS_PromiseState(ctx, result) };
            eprintln!("[QJS Evaluate-result] promise state={state}");
            if state == 2 {
              let pr = unsafe { JS_PromiseResult(ctx, result) };
              let s = unsafe { jsval_to_rust(ctx, pr) };
              eprintln!("[QJS Evaluate-result] rejection: {s}");
              let stk =
                unsafe { JS_GetPropertyStr(ctx, pr, c"stack".as_ptr()) };
              if !jsv_is_undefined(&stk) {
                let ss = unsafe { jsval_to_rust(ctx, stk) };
                eprintln!("[QJS Evaluate-result] stack:\n{ss}");
              }
              unsafe { JS_FreeValue(ctx, stk) };
              unsafe { JS_FreeValue(ctx, pr) };
            }
          } else {
            eprintln!("[QJS Evaluate-result] tag={}", result.tag);
          }
        }
        if result.tag == JS_TAG_OBJECT
          && unsafe { JS_IsPromise(result) }
          && unsafe { JS_PromiseState(ctx, result) } == 0
        {
          async_promise = Some(result);
        } else if result.tag != JS_TAG_UNDEFINED {
          unsafe { JS_FreeValue(ctx, result) };
        }
      }
    }
  }

  if let Some(t0) = _t0 {
    eprintln!(
      "[V82JSC_TIMING] Module::Evaluate {} took {:.2} ms",
      source_name_dbg,
      t0.elapsed().as_secs_f64() * 1000.0
    );
    crate::quickjs::function::timing::dump();
  }

  if let Some(p) = async_promise {
    return intern::<Value>(p);
  }

  mark_all_modules_evaluated();

  let mut funcs: [JSValue; 2] = [jsv_undefined(), jsv_undefined()];
  let promise = unsafe { JS_NewPromiseCapability(ctx, funcs.as_mut_ptr()) };
  if promise.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  let resolve = funcs[0];
  let reject = funcs[1];
  let mut args = [jsv_undefined()];
  let r =
    unsafe { JS_Call(ctx, resolve, jsv_undefined(), 1, args.as_mut_ptr()) };
  if r.tag != JS_TAG_EXCEPTION {
    unsafe { JS_FreeValue(ctx, r) };
  } else {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
  }
  unsafe {
    JS_FreeValue(ctx, resolve);
    JS_FreeValue(ctx, reject);
    drain_jobs(rt);
  }
  intern::<Value>(promise)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__IsGraphAsync(this: *const Module) -> bool {
  with_module_state(this, |m| m.is_async).unwrap_or(false)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__IsSyntheticModule(this: *const Module) -> bool {
  with_module_state(this, |m| m.synthetic).unwrap_or(false)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__CreateSyntheticModule(
  isolate: *const RealIsolate,
  module_name: *const V8String,
  export_names_len: usize,
  export_names_raw: *const *const V8String,
  evaluation_steps: SyntheticModuleEvaluationSteps,
) -> *const Module {
  let iso = isolate as *mut RealIsolate;
  if iso.is_null() {
    return ptr::null();
  }
  let st = iso_state(iso);
  let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
  if ctx.is_null() {
    return ptr::null();
  }

  let name = if module_name.is_null() {
    "<synthetic>".to_string()
  } else {
    unsafe { jsval_to_rust(ctx, jsval_of(module_name)) }
  };
  let Ok(cname) = CString::new(name) else {
    return ptr::null();
  };

  let def = unsafe {
    JS_NewCModule(ctx, cname.as_ptr(), Some(synthetic_module_init_callback))
  };
  if def.is_null() {
    return ptr::null();
  }
  if !export_names_raw.is_null() {
    for i in 0..export_names_len {
      let n = unsafe { *export_names_raw.add(i) };
      if n.is_null() {
        continue;
      }
      let s = unsafe { jsval_to_rust(ctx, jsval_of(n)) };
      if let Ok(c) = CString::new(s) {
        unsafe { JS_AddModuleExport(ctx, def, c.as_ptr()) };
      }
    }
  }

  let handle_val = unsafe { JS_NewObject(ctx) };
  if handle_val.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  let this = intern::<Module>(handle_val);
  if this.is_null() {
    return ptr::null();
  }
  if std::env::var_os("QJS_DEBUG_MOD").is_some() {
    let nm = unsafe { jsval_to_rust(ctx, jsval_of(module_name)) };
    eprintln!(
      "[QJS CreateSyntheticModule] {nm} def={def:?} n_export_names={export_names_len}"
    );
  }
  SYNTHETIC_DEFS.with(|t| {
    t.borrow_mut().insert(handle_key(this), def as usize);
  });

  let steps: SyntheticModuleEvaluationSteps<'static> =
    unsafe { std::mem::transmute(evaluation_steps) };

  let handle_dup = unsafe { JS_DupValue(ctx, handle_val) };
  SYNTHETIC_EVAL_STEPS.with(|t| {
    t.borrow_mut().insert(def as usize, (steps, handle_dup));
  });
  record_module_state(
    this,
    ModuleState {
      status: ModuleStatus::Instantiated,
      module_def: def,
      bytecode: None,
      import_specifiers: Vec::new(),
      import_offsets: Vec::new(),
      import_attributes: Vec::new(),
      source_imports: Vec::new(),
      synthetic: true,
      is_async: false,
      source_text: std::string::String::new(),
      source_name: std::string::String::new(),
    },
  );
  this
}

// Run deno's `SyntheticModuleEvaluationSteps` for module `m`, which call
// `SetSyntheticModuleExport` to populate SYNTHETIC_EXPORTS. Does NOT touch the
// QuickJS module export slots (caller decides).
unsafe fn run_synthetic_eval_steps(ctx: *mut JSContext, m: *mut JSModuleDef) {
  let steps =
    SYNTHETIC_EVAL_STEPS.with(|t| t.borrow().get(&(m as usize)).copied());
  if let Some((eval_steps, handle_jsval)) = steps {
    let cur_ctx = current_ctx();
    let ctx_for_call = if cur_ctx.is_null() { ctx } else { cur_ctx };

    let ctx_handle = super::core::intern_ctx(ctx_for_call);
    let mod_handle =
      super::core::intern_dup::<Module>(ctx_for_call, handle_jsval);
    unsafe {
      if let (Some(ctx_l), Some(mod_l)) = (
        crate::Local::from_raw(ctx_handle),
        crate::Local::from_raw(mod_handle),
      ) {
        let _ = eval_steps(ctx_l, mod_l);
      }
    }
  }
}

unsafe extern "C" fn synthetic_module_init_callback(
  ctx: *mut JSContext,
  m: *mut JSModuleDef,
) -> std::os::raw::c_int {
  unsafe { run_synthetic_eval_steps(ctx, m) };

  let exports = SYNTHETIC_EXPORTS
    .with(|t| t.borrow_mut().remove(&(m as usize)).unwrap_or_default());
  if std::env::var_os("QJS_DEBUG_MOD").is_some() {
    eprintln!(
      "[QJS synthetic init] def={:?} n_exports={}",
      m,
      exports.len()
    );
  }
  let mut ns_copy: Vec<(std::string::String, JSValue)> = Vec::new();
  for (name, value) in exports {
    let Ok(name_c) = CString::new(name.clone()) else {
      unsafe { JS_FreeValue(ctx, value) };
      continue;
    };
    // Keep a dup for the namespace (JS_SetModuleExport consumes `value`).
    ns_copy.push((name, unsafe { JS_DupValue(ctx, value) }));
    unsafe { JS_SetModuleExport(ctx, m, name_c.as_ptr(), value) };
  }
  SYNTHETIC_NS_EXPORTS.with(|t| {
    t.borrow_mut().insert(m as usize, ns_copy);
  });
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__SetSyntheticModuleExport(
  this: *const Module,
  isolate: *const RealIsolate,
  export_name: *const V8String,
  export_value: *const Value,
) -> MaybeBool {
  let iso = isolate as *mut RealIsolate;
  let ctx = if iso.is_null() {
    current_ctx()
  } else {
    let st = iso_state(iso);
    st.contexts.last().copied().unwrap_or(st.ctx)
  };
  if ctx.is_null() {
    return MaybeBool::JustFalse;
  }
  let def = SYNTHETIC_DEFS
    .with(|t| t.borrow().get(&handle_key(this)).copied())
    .map(|p| p as *mut JSModuleDef);
  let Some(def) = def else {
    return MaybeBool::JustFalse;
  };
  let name = unsafe { jsval_to_rust(ctx, jsval_of(export_name)) };

  let val = unsafe { JS_DupValue(ctx, jsval_of(export_value)) };
  if std::env::var_os("QJS_DEBUG_MOD").is_some() {
    eprintln!("[QJS SetSyntheticExport] def={def:?} name={name}");
  }
  SYNTHETIC_EXPORTS.with(|t| {
    t.borrow_mut()
      .entry(def as usize)
      .or_default()
      .push((name, val));
  });
  MaybeBool::JustTrue
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetUnboundModuleScript(
  this: *const Module,
) -> *const UnboundModuleScript {
  let ctx = current_ctx();
  intern_dup::<UnboundModuleScript>(ctx, jsval_of(this))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetStalledTopLevelAwaitMessage(
  _this: *const Module,
  _isolate: *const RealIsolate,
  _out_vec: *mut StalledTopLevelAwaitMessage,
  _vec_len: usize,
) -> usize {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ModuleRequest__GetSpecifier(
  this: *const ModuleRequest,
) -> *const V8String {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  let v = jsval_of(this);

  let spec = unsafe { JS_GetPropertyStr(ctx, v, c"specifier".as_ptr()) };
  if spec.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  intern::<V8String>(spec)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ModuleRequest__GetPhase(
  this: *const ModuleRequest,
) -> ModuleImportPhase {
  let ctx = current_ctx();
  if !ctx.is_null() && !this.is_null() {
    let v = unsafe {
      JS_GetPropertyStr(ctx, jsval_of(this), c"__src_phase".as_ptr())
    };
    let is_src = unsafe { JS_ToBool(ctx, v) } != 0;
    unsafe { JS_FreeValue(ctx, v) };
    if is_src {
      return ModuleImportPhase::kSource;
    }
  }
  ModuleImportPhase::kEvaluation
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ModuleRequest__GetSourceOffset(
  this: *const ModuleRequest,
) -> int {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return 0;
  }
  let v = jsval_of(this);
  let off = unsafe { JS_GetPropertyStr(ctx, v, c"__source_offset".as_ptr()) };
  let n = match off.tag {
    JS_TAG_INT => unsafe { off.u.int32 },
    JS_TAG_FLOAT64 => (unsafe { off.u.float64 }) as i32,
    _ => 0,
  };
  unsafe { JS_FreeValue(ctx, off) };
  n as int
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ModuleRequest__GetImportAttributes(
  this: *const ModuleRequest,
) -> *const FixedArray {
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }
  let arr = unsafe { JS_NewArray(ctx) };
  if arr.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }

  if !this.is_null() {
    let req = jsval_of(this);
    // Prefer the full `with { ... }` attribute set (flat [k0,v0,k1,v1,...]
    // string array) so deno's validate-import-attributes callback and custom
    // module-type machinery see every key, not just `type`.
    let kv = unsafe { JS_GetPropertyStr(ctx, req, c"__attr_kv".as_ptr()) };
    let mut emitted = false;
    if jsv_is_object(&kv) {
      let len_v = unsafe { JS_GetPropertyStr(ctx, kv, c"length".as_ptr()) };
      let kv_len = match len_v.tag {
        JS_TAG_INT => unsafe { len_v.u.int32 },
        JS_TAG_FLOAT64 => (unsafe { len_v.u.float64 }) as i32,
        _ => 0,
      };
      unsafe { JS_FreeValue(ctx, len_v) };
      let mut out = 0u32;
      let mut i = 0i32;
      while i + 1 < kv_len {
        let k = unsafe { JS_GetPropertyUint32(ctx, kv, i as u32) };
        let v = unsafe { JS_GetPropertyUint32(ctx, kv, (i + 1) as u32) };
        unsafe {
          // Static-import attributes are triples (key, value, source_offset).
          JS_SetPropertyUint32(ctx, arr, out, k);
          JS_SetPropertyUint32(ctx, arr, out + 1, v);
          JS_SetPropertyUint32(ctx, arr, out + 2, JS_NewInt32(ctx, 0));
        }
        out += 3;
        emitted = true;
        i += 2;
      }
    }
    unsafe { JS_FreeValue(ctx, kv) };

    if !emitted {
      // Fallback: legacy `type`-only path (synthetic/source-phase requests that
      // never recorded a full `with {}` clause).
      let ty = unsafe { JS_GetPropertyStr(ctx, req, c"__attr_type".as_ptr()) };
      if jsv_is_string(&ty) {
        unsafe {
          let key = JS_NewString(ctx, c"type".as_ptr());
          JS_SetPropertyUint32(ctx, arr, 0, key);

          JS_SetPropertyUint32(ctx, arr, 1, ty);
          JS_SetPropertyUint32(ctx, arr, 2, JS_NewInt32(ctx, 0));
        }
      } else {
        unsafe { JS_FreeValue(ctx, ty) };
      }
    }
  }
  intern::<FixedArray>(arr)
}

// ---------------------------------------------------------------------------
// Link-stubs for v8 C-ABI symbols that `test_api.rs` references but this
// backend doesn't implement yet. Each returns a benign default
// (null / 0 / false / `Nothing`) so the target LINKS and the many tests that
// don't touch these paths run; tests that do exercise them fail gracefully
// without crashing. Promote individual stubs to real implementations over time.
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__IsSourceTextModule(
  _this: *const std::os::raw::c_void,
) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__ScriptId(
  _this: *const std::os::raw::c_void,
) -> crate::support::int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptOrigin__ResourceName(
  _origin: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptOrigin__ScriptId(
  _origin: *const std::os::raw::c_void,
) -> i32 {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptOrigin__SourceMapUrl(
  _origin: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}
