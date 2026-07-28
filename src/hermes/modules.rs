//! D2: ES modules on the Hermes backend, modeled on top of JSI.
//!
//! Hermes / JSI has no ES-module-record API: `evaluateJavaScript` runs a single
//! classic script, and a top-level `import`/`export` is a syntax error. V8's
//! module semantics are therefore MODELED here. A module is compiled to a JS
//! closure of the shape
//!
//! ```text
//!   (function (__imports, __exports) { <rewritten module body> })
//! ```
//!
//! where every `import ... from "spec"` is rewritten to read bindings off
//! `__imports["spec"]` and every `export` is rewritten to assign onto
//! `__exports`. Instantiation walks the module's recorded import requests,
//! invokes the embedder's resolve callback to get each dependency Module,
//! recursively instantiates it, and binds its namespace object into the
//! `__imports` map. Evaluation runs the closure with `(__imports, __exports)`.
//!
//! A synthetic module (how deno_core's `ext:core/ops` is provided) is just an
//! exports object populated by native evaluation steps; `SetSyntheticModuleExport`
//! writes one named export.
//!
//! ### Module handles
//!
//! A Module / ModuleRequest / FixedArray is a Rust-owned `Box` record whose raw
//! pointer IS the v8 `Local` handle. These records never enter the JSI handle
//! table (`slot_of` sees their even-aligned pointer as the null slot, which is
//! fine because they never pass through it); they are freed at isolate Dispose
//! via `IsoState::module_records`. JS values the record must hold across
//! HandleScope pops (exports object, namespace, compiled closure) are parked in
//! runtime-owned durable pins (`v8x_hermes_pin`), honoring the C2 lifetime rule.
//!
//! ### Handled import/export forms
//!
//! The transform is a focused source-to-source rewrite covering the forms the
//! deno_core boot graph and the module rusty_v8 tests use, NOT every ESM edge
//! case. See docs/hermes-spike/experiments/D2-hermes-modules.md for the exact
//! grammar handled and the known gaps.
#![allow(non_snake_case)]

use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use crate::hermes::core::{
  current_rtw, iso_state, read_string_slot, slot_of, slot_ptr,
};
use crate::isolate::ModuleImportPhase;
use crate::isolate::RealIsolate;
use crate::module::ResolveModuleCallback;
use crate::module::ResolveSourceCallback;
use crate::module::SyntheticModuleEvaluationSteps;
use crate::support::MaybeBool;
use crate::{
  Context, Data, FixedArray, Function, Module, ModuleRequest, Object,
  String as V8String, Value,
};

// The JSI-bridge C functions the module linker drives. These are defined in
// src/hermes/hermes_shim.cpp and also declared in core.rs; re-declaring them
// here resolves to the same symbols at link time.
unsafe extern "C" {
  fn v8x_hermes_pin(rtw: *mut c_void, slot: i64) -> i64;
  fn v8x_hermes_pin_get(rtw: *mut c_void, pin_id: i64) -> i64;
  #[allow(dead_code)]
  fn v8x_hermes_unpin(rtw: *mut c_void, pin_id: i64);
  fn v8x_hermes_object_new(rtw: *mut c_void) -> i64;
  fn v8x_hermes_object_set(
    rtw: *mut c_void,
    obj_slot: i64,
    key_slot: i64,
    value_slot: i64,
  ) -> c_int;
  fn v8x_hermes_string_new_utf8(
    rtw: *mut c_void,
    data: *const c_char,
    len: usize,
  ) -> i64;
  fn v8x_hermes_run(rtw: *mut c_void, src_slot: i64, ok: *mut c_int) -> i64;
  fn v8x_hermes_function_call(
    rtw: *mut c_void,
    fn_slot: i64,
    recv_slot: i64,
    arg_slots: *const i64,
    argc: usize,
    ok: *mut c_int,
  ) -> i64;
  fn v8x_hermes_undefined(rtw: *mut c_void) -> i64;
  fn v8x_hermes_promise_resolver_new(rtw: *mut c_void) -> i64;
  fn v8x_hermes_promise_resolver_get_promise(
    rtw: *mut c_void,
    resolver_slot: i64,
  ) -> i64;
  fn v8x_hermes_promise_resolver_resolve(
    rtw: *mut c_void,
    resolver_slot: i64,
    value_slot: i64,
  ) -> c_int;
}

// V8's ModuleStatus, mirrored (repr(C), same order as
// vendor/rusty_v8/src/module.rs ModuleStatus).
const K_UNINSTANTIATED: i32 = 0;
const K_INSTANTIATING: i32 = 1;
const K_INSTANTIATED: i32 = 2;
const K_EVALUATING: i32 = 3;
const K_EVALUATED: i32 = 4;
const K_ERRORED: i32 = 5;

/// A parsed import request: `import ... from "specifier"`.
struct ImportReq {
  specifier: String,
  /// Namespace pin id of the module this request resolved to, set at
  /// instantiate. -1 / None until resolved.
  resolved_ns_pin: Option<i64>,
  /// The dependency Module this request resolved to (its record pointer), so
  /// Evaluate can run dependencies before the importer (V8 depth-first eval).
  resolved_module: Option<*const Module>,
}

enum ModuleKind {
  /// A source-text module: a compiled closure plus its parsed requests/exports.
  SourceText {
    /// The transformed closure source, `(function(__imports,__exports){...})`.
    closure_source: String,
    /// Distinct import specifiers, in first-seen order.
    requests: Vec<ImportReq>,
    /// Declared export names (best-effort; retained for the namespace shape and
    /// for diagnostics on the deno_core boot graph).
    #[allow(dead_code)]
    export_names: Vec<String>,
  },
  /// A synthetic module: an exports object filled by native evaluation steps.
  Synthetic {
    #[allow(dead_code)]
    export_names: Vec<String>,
    eval_steps: SyntheticModuleEvaluationSteps<'static>,
  },
}

/// A modeled v8 Module. Its `Box` raw pointer is the `*const Module` handle.
struct ModuleRecord {
  kind: ModuleKind,
  status: i32,
  /// Pin id of the module namespace / exports object. Created at instantiate.
  namespace_pin: i64,
  /// The FixedArray record of ModuleRequests (source modules only).
  requests_array: *const FixedArray,
  /// Stable non-zero identity hash.
  identity_hash: i32,
  /// Pin id of the errored exception value, or -1.
  exception_pin: i64,
  /// Pin id of the promise returned by evaluating this module's async closure
  /// (its top-level-await promise), or -1 until evaluated. Source modules only.
  eval_promise_pin: i64,
}

/// A modeled v8 ModuleRequest. Its `Box` raw pointer is the handle.
struct ModuleRequestRecord {
  /// Interned specifier string slot pointer (re-interned lazily if the scope
  /// that produced it is gone; we keep the raw text to re-intern on demand).
  specifier: String,
  /// The import-attributes FixedArray (always empty for our path).
  attributes: *const FixedArray,
}

/// A modeled v8 FixedArray: a flat vector of `*const Data` element handles.
struct FixedArrayRecord {
  elements: Vec<*const Data>,
}

static mut IDENTITY_COUNTER: i32 = 0;

fn next_identity_hash() -> i32 {
  // Never 0 (v8 contract). Single-threaded per isolate; the whole backend is
  // one-runtime-per-thread, so a plain static bump is adequate here.
  unsafe {
    IDENTITY_COUNTER = IDENTITY_COUNTER.wrapping_add(1);
    if IDENTITY_COUNTER == 0 {
      IDENTITY_COUNTER = 1;
    }
    IDENTITY_COUNTER
  }
}

/// Register a boxed record's drop glue so it is freed at isolate Dispose.
fn register_drop<T: 'static>(iso: *mut RealIsolate, boxed: *mut T) {
  if iso.is_null() {
    return;
  }
  iso_state(iso).module_records.push(Box::new(move || {
    // Reconstitute and drop the box. Safe: registered exactly once per record,
    // and Dispose drains the list exactly once.
    unsafe { drop(Box::from_raw(boxed)) };
  }));
}

#[inline]
fn module_ref<'a>(this: *const Module) -> Option<&'a mut ModuleRecord> {
  if this.is_null() {
    return None;
  }
  Some(unsafe { &mut *(this as *mut ModuleRecord) })
}

// ---- ScriptOrigin / Source (our own layout) --------------------------------
//
// We implement CONSTRUCT, so the 40-byte opaque ScriptOrigin buffer and the
// Source struct carry a layout of our choosing. Only the fields CompileModule
// needs are stored: the source-string slot pointer and the is_module flag.

#[repr(C)]
struct OriginLayout {
  resource_name: *const Value,
  source_map_url: *const Value,
  is_module: u8,
  _pad: [u8; 7],
  // remaining bytes of the 40-byte buffer are unused
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptOrigin__CONSTRUCT(
  buf: *mut c_void,
  resource_name: *const Value,
  _resource_line_offset: i32,
  _resource_column_offset: i32,
  _resource_is_shared_cross_origin: bool,
  _script_id: i32,
  source_map_url: *const Value,
  _resource_is_opaque: bool,
  _is_wasm: bool,
  is_module: bool,
  _host_defined_options: *const Data,
) {
  if buf.is_null() {
    return;
  }
  // Zero the whole 40-byte buffer first, then write our layout.
  unsafe {
    ptr::write_bytes(buf as *mut u8, 0, 40);
    let o = buf as *mut OriginLayout;
    (*o).resource_name = resource_name;
    (*o).source_map_url = source_map_url;
    (*o).is_module = u8::from(is_module);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptOrigin__ResourceName(
  origin: *const c_void,
) -> *const Value {
  if origin.is_null() {
    return ptr::null();
  }
  unsafe { (*(origin as *const OriginLayout)).resource_name }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptOrigin__SourceMapUrl(
  origin: *const c_void,
) -> *const Value {
  if origin.is_null() {
    return ptr::null();
  }
  unsafe { (*(origin as *const OriginLayout)).source_map_url }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptOrigin__ScriptId(_origin: *const c_void) -> i32 {
  0
}

// The Source struct (vendor/rusty_v8/src/script_compiler.rs) is 12 usizes; its
// first field is the source-string pointer and the seventh (`_host_defined_
// options`) we leave alone. We only populate what CompileModule reads: the
// source string (field 0) and the ScriptOrigin pointer (we stash it in the
// resource-name field 1 so CompileModule can recover is_module + name).

#[repr(C)]
struct SourceLayout {
  source_string: *const V8String,
  origin: *const c_void,
  _rest: [usize; 10],
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__Source__CONSTRUCT(
  buf: *mut c_void,
  source_string: *const V8String,
  origin: *const c_void,
  _cached_data: *mut c_void,
) {
  if buf.is_null() {
    return;
  }
  unsafe {
    let s = buf as *mut SourceLayout;
    (*s).source_string = source_string;
    (*s).origin = origin;
    for i in 0..10 {
      (*s)._rest[i] = 0;
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__Source__DESTRUCT(_this: *mut c_void) {
  // Our Source layout owns no heap; nothing to free.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__Source__GetCachedData(
  _this: *const c_void,
) -> *const c_void {
  ptr::null()
}

// ---- CompileModule ---------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CompileModule(
  isolate: *mut RealIsolate,
  source: *mut c_void,
  _options: c_int,
  _no_cache_reason: c_int,
) -> *const Module {
  if isolate.is_null() || source.is_null() {
    return ptr::null();
  }
  let rtw = iso_state(isolate).rtw;
  let src_layout = source as *const SourceLayout;
  let source_string = unsafe { (*src_layout).source_string };
  let text = match read_string_slot(rtw, slot_of(source_string)) {
    Some(t) => t,
    None => return ptr::null(),
  };

  let parsed = transform_module(&text);

  // Build the ModuleRequest / FixedArray records for the parsed imports.
  let mut req_ptrs: Vec<*const Data> = Vec::with_capacity(parsed.requests.len());
  for req in &parsed.requests {
    let attrs = Box::into_raw(Box::new(FixedArrayRecord {
      elements: Vec::new(),
    })) as *const FixedArray;
    register_drop(isolate, attrs as *mut FixedArrayRecord);
    let rr = Box::into_raw(Box::new(ModuleRequestRecord {
      specifier: req.specifier.clone(),
      attributes: attrs,
    }));
    register_drop(isolate, rr);
    req_ptrs.push(rr as *const Data);
  }
  let requests_array = Box::into_raw(Box::new(FixedArrayRecord {
    elements: req_ptrs,
  })) as *const FixedArray;
  register_drop(isolate, requests_array as *mut FixedArrayRecord);

  let record = Box::into_raw(Box::new(ModuleRecord {
    kind: ModuleKind::SourceText {
      closure_source: parsed.closure_source,
      requests: parsed.requests,
      export_names: parsed.export_names,
    },
    status: K_UNINSTANTIATED,
    namespace_pin: -1,
    requests_array,
    identity_hash: next_identity_hash(),
    exception_pin: -1,
    eval_promise_pin: -1,
  }));
  register_drop(isolate, record);
  record as *const Module
}

// ---- CompileFunction -------------------------------------------------------
//
// `v8::script_compiler::compile_function(context, source, args, ctx_exts, ...)`
// compiles `source` as the BODY of a function whose parameters are `args`,
// returning that function object (V8's `CompileFunctionInContext`). deno_core
// uses this to hydrate `lazy_loaded_js` ext scripts (`core.loadExtScript`): the
// wrapped source is `"use strict"; return (<IIFE>);` compiled with a single
// parameter `__bootstrap`, then called with the captured bootstrap view.
//
// Hermes/JSI has no CompileFunctionInContext, but `new Function(args, body)` is
// exactly this primitive, and evaluating a `(function (a, b) { <body> })`
// expression yields the same function object. We build that expression source,
// run the E1 async-generator lowering on the body (ext/web streams contain
// async generators), and evaluate it via `v8x_hermes_run`. A parse/eval error
// is captured into the innermost live TryCatch (the C9 path), so the caller's
// `tc_scope.exception()` is populated (deno_core unwraps it) and we return null.
//
// `context_extensions` (the `with`-scope objects V8 supports) are not modeled:
// deno_core passes none for ext scripts. If any are passed we ignore them
// (the compiled function simply won't see those bindings), which is inert.
#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CompileFunction(
  context: *const Context,
  source: *mut c_void,
  arguments_count: usize,
  arguments: *const *const V8String,
  _context_extensions_count: usize,
  _context_extensions: *const *const Object,
  _options: c_int,
  _no_cache_reason: c_int,
) -> *const Function {
  if context.is_null() || source.is_null() {
    return ptr::null();
  }
  let isolate = context as *mut RealIsolate;
  let rtw = iso_state(isolate).rtw;

  // Read the function body out of the Source (field 0 is the source string).
  let src_layout = source as *const SourceLayout;
  let source_string = unsafe { (*src_layout).source_string };
  let body = match read_string_slot(rtw, slot_of(source_string)) {
    Some(t) => t,
    None => return ptr::null(),
  };

  // Read each parameter name.
  let mut params: Vec<String> = Vec::with_capacity(arguments_count);
  if !arguments.is_null() {
    let arg_slice =
      unsafe { std::slice::from_raw_parts(arguments, arguments_count) };
    for &arg in arg_slice {
      if arg.is_null() {
        return ptr::null();
      }
      match read_string_slot(rtw, slot_of(arg)) {
        Some(name) => params.push(name),
        None => return ptr::null(),
      }
    }
  }

  // Build `(function (<params>) { <body> })`, an expression evaluating to the
  // function object. Parameter names come from V8 and are plain identifiers.
  // The body may contain a top-level `return` (deno_core wraps ext scripts as
  // `"use strict"; return (<IIFE>);`), which is only valid INSIDE a function,
  // so the function wrapper must be applied BEFORE the async-generator lowering
  // parses it. Lowering the bare body first would make oxc reject the top-level
  // `return`, silently pass the source through unchanged, and leave the
  // `async function*` syntax for Hermes to reject ("async generators are
  // unsupported"). See docs/hermes-spike/experiments/E3-deno-web.md.
  let params_joined = params.join(", ");
  let wrapped = format!("(function ({params_joined}) {{\n{body}\n}})");

  // E1 async-generator lowering on the whole wrapped expression (ext/web
  // streams, e.g. ext/web/09_file.js, use `async function*` / `async *m()`).
  let lowered = crate::hermes::lower::lower_async_generators(&wrapped);

  let src_slot = intern(rtw, &lowered);
  let mut ok: c_int = 0;
  let fn_slot = unsafe { v8x_hermes_run(rtw, src_slot, &mut ok) };
  if ok == 0 || fn_slot < 0 {
    // Parse/eval error already captured into the live TryCatch (C9). Returning
    // null makes the caller read that captured exception.
    return ptr::null();
  }
  slot_ptr::<Function>(fn_slot)
}

// ---- Module status / metadata ----------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetStatus(this: *const Module) -> i32 {
  module_ref(this).map(|m| m.status).unwrap_or(K_ERRORED)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetIdentityHash(this: *const Module) -> i32 {
  module_ref(this).map(|m| m.identity_hash).unwrap_or(1)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__ScriptId(this: *const Module) -> i32 {
  module_ref(this).map(|m| m.identity_hash).unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__IsSourceTextModule(this: *const Module) -> bool {
  matches!(
    module_ref(this).map(|m| &m.kind),
    Some(ModuleKind::SourceText { .. })
  )
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__IsSyntheticModule(this: *const Module) -> bool {
  matches!(
    module_ref(this).map(|m| &m.kind),
    Some(ModuleKind::Synthetic { .. })
  )
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__IsGraphAsync(_this: *const Module) -> bool {
  // No top-level await in our modeled modules.
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetException(this: *const Module) -> *const Value {
  let rtw = current_rtw();
  if let Some(m) = module_ref(this) {
    if m.exception_pin >= 0 && !rtw.is_null() {
      let slot = unsafe { v8x_hermes_pin_get(rtw, m.exception_pin) };
      return slot_ptr::<Value>(slot);
    }
  }
  ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetModuleRequests(
  this: *const Module,
) -> *const FixedArray {
  match module_ref(this) {
    Some(m) => m.requests_array,
    None => ptr::null(),
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetModuleNamespace(
  this: *const Module,
) -> *const Value {
  let rtw = current_rtw();
  if rtw.is_null() {
    return ptr::null();
  }
  if let Some(m) = module_ref(this) {
    if m.namespace_pin >= 0 {
      let slot = unsafe { v8x_hermes_pin_get(rtw, m.namespace_pin) };
      return slot_ptr::<Value>(slot);
    }
  }
  ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetModuleNamespace2(
  this: *const Module,
  _phase: ModuleImportPhase,
) -> *const Value {
  v8__Module__GetModuleNamespace(this)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetUnboundModuleScript(
  this: *const Module,
) -> *const c_void {
  // Our modules carry no separate UnboundModuleScript (the closure source is
  // re-compiled on each Evaluate). deno_core's `new_module_from_js_source`
  // unconditionally calls this and `.unwrap()`s the result, then reads
  // `get_source_mapping_url` off it. So return the module record pointer itself
  // as an opaque, non-null UnboundModuleScript handle (it is never dereferenced
  // as a script; the only method called on it, GetSourceMappingURL/GetSourceURL,
  // returns undefined below). Return null only for a null module.
  if this.is_null() {
    return ptr::null();
  }
  this as *const c_void
}

/// `UnboundModuleScript::GetSourceMappingURL`. Our modeled modules carry no
/// V8-embedded `//# sourceMappingURL`, so return `undefined` (a real value, not
/// null: the vendored accessor `.unwrap()`s it). deno_core skips the source-map
/// branch when this is undefined/null.
#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundModuleScript__GetSourceMappingURL(
  _script: *const c_void,
) -> *const Value {
  let rtw = current_rtw();
  if rtw.is_null() {
    return ptr::null();
  }
  slot_ptr::<Value>(unsafe { v8x_hermes_undefined(rtw) })
}

/// `UnboundModuleScript::GetSourceURL`. Same as above: no embedded
/// `//# sourceURL`, return `undefined`.
#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundModuleScript__GetSourceURL(
  _script: *const c_void,
) -> *const Value {
  let rtw = current_rtw();
  if rtw.is_null() {
    return ptr::null();
  }
  slot_ptr::<Value>(unsafe { v8x_hermes_undefined(rtw) })
}

/// `Module::GetStalledTopLevelAwaitMessage`: our modeled modules have no
/// top-level await, so there is never a stalled module. Returns 0 (empty).
#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetStalledTopLevelAwaitMessage(
  _this: *const Module,
  _isolate: *const RealIsolate,
  _out_vec: *mut c_void,
  _vec_len: usize,
) -> usize {
  0
}

/// `Module::EvaluateForImportDefer`: `import.defer()` support. Our modules are
/// evaluated eagerly, so this just evaluates and returns the same resolved
/// promise as Evaluate.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__EvaluateForImportDefer(
  this: *const Module,
  context: *const Context,
) -> *const Value {
  v8__Module__Evaluate(this, context)
}

// ---- Instantiate -----------------------------------------------------------

/// Ensure the module (and, for source modules, all its requested modules) has a
/// namespace/exports object, recursively resolving requests via `callback`.
/// Returns Some(namespace_slot) on success, None on failure (a resolve returned
/// null or a nested instantiate failed).
fn ensure_instantiated(
  isolate: *mut RealIsolate,
  context: *const Context,
  this: *const Module,
  callback: ResolveModuleCallback<'static>,
) -> bool {
  let rtw = iso_state(isolate).rtw;
  let m = match module_ref(this) {
    Some(m) => m,
    None => return false,
  };
  if m.status >= K_INSTANTIATED {
    return true;
  }
  m.status = K_INSTANTIATING;

  // Create this module's exports/namespace object now, so a cyclic import can
  // observe a live (initially empty) namespace, matching V8 module linking.
  if m.namespace_pin < 0 {
    let ns = unsafe { v8x_hermes_object_new(rtw) };
    let pin = unsafe { v8x_hermes_pin(rtw, ns) };
    m.namespace_pin = pin;
  }

  match &m.kind {
    ModuleKind::Synthetic { .. } => {
      // Synthetic modules have no requests; instantiation is a no-op beyond
      // creating the (empty) exports object above.
      m.status = K_INSTANTIATED;
      true
    }
    ModuleKind::SourceText { requests, .. } => {
      let specs: Vec<String> =
        requests.iter().map(|r| r.specifier.clone()).collect();
      let empty_attrs = Box::into_raw(Box::new(FixedArrayRecord {
        elements: Vec::new(),
      })) as *const FixedArray;
      register_drop(isolate, empty_attrs as *mut FixedArrayRecord);

      // (request index, resolved namespace pin, resolved module) recorded back
      // after the loop, since the loop mutably re-borrows other module records
      // recursively.
      let mut resolved_pins: Vec<(usize, i64, *const Module)> =
        Vec::with_capacity(specs.len());

      // The callback returns the System V struct ResolveModuleCallbackRet,
      // which is #[repr(C)] wrapping a single `*const Module`, hence
      // ABI-identical to returning that pointer directly. Its field is private,
      // so transmute the fn pointer to the pointer-returning shape.
      let raw_cb: RawResolveCb = unsafe { std::mem::transmute(callback) };

      for (idx, spec) in specs.iter().enumerate() {
        let spec_slot = intern(rtw, spec);
        let specifier = slot_ptr::<V8String>(spec_slot) as *const V8String;
        // Invoke the embedder resolve callback: (context, specifier,
        // import_attributes, referrer) -> Option<Module>.
        let resolved: *const Module = unsafe {
          raw_cb(
            local(context),
            local(specifier),
            local(empty_attrs),
            local(this),
          )
        };
        if resolved.is_null() {
          if let Some(m) = module_ref(this) {
            m.status = K_ERRORED;
          }
          return false;
        }
        if !ensure_instantiated(isolate, context, resolved, callback) {
          if let Some(m) = module_ref(this) {
            m.status = K_ERRORED;
          }
          return false;
        }
        let ns_pin = module_ref(resolved).map(|d| d.namespace_pin).unwrap_or(-1);
        resolved_pins.push((idx, ns_pin, resolved));
      }

      // Re-borrow after the recursive calls and record resolution results.
      if let Some(m) = module_ref(this) {
        if let ModuleKind::SourceText { requests, .. } = &mut m.kind {
          for (idx, pin, resolved) in resolved_pins {
            if let Some(req) = requests.get_mut(idx) {
              req.resolved_ns_pin = if pin >= 0 { Some(pin) } else { None };
              req.resolved_module = Some(resolved);
            }
          }
        }
        m.status = K_INSTANTIATED;
      }
      true
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__InstantiateModule(
  this: *const Module,
  context: *const Context,
  cb: ResolveModuleCallback<'static>,
  _source_callback: Option<ResolveSourceCallback<'static>>,
) -> MaybeBool {
  if this.is_null() || context.is_null() {
    return MaybeBool::Nothing;
  }
  let isolate = context as *mut RealIsolate;
  if ensure_instantiated(isolate, context, this, cb) {
    MaybeBool::JustTrue
  } else {
    MaybeBool::Nothing
  }
}

// ---- Evaluate --------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__Evaluate(
  this: *const Module,
  context: *const Context,
) -> *const Value {
  if this.is_null() || context.is_null() {
    return ptr::null();
  }
  let isolate = context as *mut RealIsolate;
  let rtw = iso_state(isolate).rtw;
  if evaluate_rec(isolate, context, this) {
    // Return the real promise from the (async) module closure evaluation: it is
    // pending while a top-level await is in flight and already-resolved for a
    // synchronous module body. deno_core tracks this promise to detect TLA.
    // Synthetic modules never run a closure, so they fall back to a fresh
    // resolved promise.
    let pin = module_ref(this).map(|m| m.eval_promise_pin).unwrap_or(-1);
    if pin >= 0 {
      let slot = unsafe { v8x_hermes_pin_get(rtw, pin) };
      return slot_ptr::<Value>(slot);
    }
    resolved_promise(rtw, context)
  } else {
    ptr::null()
  }
}

/// Evaluate `this` and, for source modules, every requested module first
/// (V8 evaluates the dependency graph depth-first before the importer). Returns
/// true on success. Idempotent: an already-evaluated module is a no-op.
fn evaluate_rec(
  isolate: *mut RealIsolate,
  context: *const Context,
  this: *const Module,
) -> bool {
  let rtw = iso_state(isolate).rtw;
  let (status, is_synthetic) = match module_ref(this) {
    Some(m) => (
      m.status,
      matches!(m.kind, ModuleKind::Synthetic { .. }),
    ),
    None => return false,
  };
  if status == K_ERRORED {
    return false;
  }
  if status < K_INSTANTIATED {
    return false;
  }
  if status >= K_EVALUATED {
    return true;
  }
  if let Some(m) = module_ref(this) {
    m.status = K_EVALUATING;
  }

  if is_synthetic {
    let steps = match module_ref(this) {
      Some(ModuleRecord {
        kind: ModuleKind::Synthetic { eval_steps, .. },
        ..
      }) => *eval_steps,
      _ => return false,
    };
    // The steps return the System V struct SyntheticModuleEvaluationStepsRet
    // (repr(C) around a single `*const Value`); transmute to the
    // pointer-returning shape. They call SetSyntheticModuleExport to populate
    // the exports object.
    let raw_steps: RawSyntheticSteps = unsafe { std::mem::transmute(steps) };
    let _ret = unsafe { raw_steps(local(context), local(this)) };
    if let Some(m) = module_ref(this) {
      m.status = K_EVALUATED;
    }
    return true;
  }

  // Source module: evaluate every dependency first.
  let deps: Vec<*const Module> = match module_ref(this) {
    Some(ModuleRecord {
      kind: ModuleKind::SourceText { requests, .. },
      ..
    }) => requests.iter().filter_map(|r| r.resolved_module).collect(),
    _ => Vec::new(),
  };
  for dep in deps {
    if !evaluate_rec(isolate, context, dep) {
      if let Some(m) = module_ref(this) {
        m.status = K_ERRORED;
      }
      return false;
    }
  }

  let closure_source = match module_ref(this) {
    Some(ModuleRecord {
      kind: ModuleKind::SourceText { closure_source, .. },
      ..
    }) => closure_source.clone(),
    _ => return false,
  };

  // E1: lower any `async function*` / `async *method` in the module body into
  // the ES2017 downlevel Hermes accepts (a no-op for module bodies without one).
  // This runs on the fully-formed closure source (after import/export rewriting)
  // so the whole module body flows through the same async-generator lowering the
  // Script::compile path uses. See src/hermes/lower.rs.
  let lowered = crate::hermes::lower::lower_async_generators(&closure_source);

  // Compile the closure (an expression producing a function).
  let src_slot = intern(rtw, &lowered);
  let mut ok: c_int = 0;
  let fn_slot = unsafe { v8x_hermes_run(rtw, src_slot, &mut ok) };
  if ok == 0 || fn_slot < 0 {
    // Compilation / top-level error: mark errored. The enclosing TryCatch, if
    // any, already captured the JSError via the C9 path.
    if let Some(m) = module_ref(this) {
      m.status = K_ERRORED;
    }
    return false;
  }

  // Build the __imports map: { specifier: <dependency namespace>, ... }.
  let imports = build_imports(isolate, context, this);
  // The exports/namespace object created at instantiate.
  let exports_slot = unsafe {
    v8x_hermes_pin_get(rtw, module_ref(this).map(|m| m.namespace_pin).unwrap_or(-1))
  };

  let undef = unsafe { v8x_hermes_undefined(rtw) };
  let args = [imports, exports_slot];
  let mut call_ok: c_int = 0;
  // The closure is an async function, so calling it returns a Promise (its
  // top-level-await promise). Pin it so `v8__Module__Evaluate` can hand the
  // real promise back to deno_core (which awaits it: a pending promise = TLA
  // still in flight, an already-resolved one = a synchronous module body).
  let res = unsafe {
    v8x_hermes_function_call(
      rtw,
      fn_slot,
      undef,
      args.as_ptr(),
      args.len(),
      &mut call_ok,
    )
  };
  if call_ok == 0 {
    if let Some(m) = module_ref(this) {
      m.status = K_ERRORED;
    }
    return false;
  }
  let promise_pin = unsafe { v8x_hermes_pin(rtw, res) };
  if let Some(m) = module_ref(this) {
    m.eval_promise_pin = promise_pin;
    m.status = K_EVALUATED;
  }
  true
}

/// Build the `__imports` object mapping each import specifier to the resolved
/// module's namespace object. Resolution happened at instantiate, which parked
/// each request's resolved namespace pin on the request record.
fn build_imports(
  isolate: *mut RealIsolate,
  _context: *const Context,
  this: *const Module,
) -> i64 {
  let rtw = iso_state(isolate).rtw;
  let imports = unsafe { v8x_hermes_object_new(rtw) };
  if let Some(m) = module_ref(this) {
    if let ModuleKind::SourceText { requests, .. } = &m.kind {
      for req in requests {
        if let Some(ns_pin) = req.resolved_ns_pin {
          let ns_slot = unsafe { v8x_hermes_pin_get(rtw, ns_pin) };
          let key = intern(rtw, &req.specifier);
          unsafe {
            v8x_hermes_object_set(rtw, imports, key, ns_slot);
          }
        }
      }
    }
  }
  imports
}

// ---- Synthetic modules -----------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__CreateSyntheticModule(
  isolate: *const RealIsolate,
  _module_name: *const V8String,
  export_names_len: usize,
  export_names_raw: *const *const V8String,
  evaluation_steps: SyntheticModuleEvaluationSteps<'static>,
) -> *const Module {
  let iso = isolate as *mut RealIsolate;
  if iso.is_null() {
    return ptr::null();
  }
  let rtw = iso_state(iso).rtw;
  let mut names: Vec<String> = Vec::with_capacity(export_names_len);
  for i in 0..export_names_len {
    let name_ptr = unsafe { *export_names_raw.add(i) };
    if let Some(s) = read_string_slot(rtw, slot_of(name_ptr)) {
      names.push(s);
    }
  }
  let empty = Box::into_raw(Box::new(FixedArrayRecord {
    elements: Vec::new(),
  })) as *const FixedArray;
  register_drop(iso, empty as *mut FixedArrayRecord);

  let record = Box::into_raw(Box::new(ModuleRecord {
    kind: ModuleKind::Synthetic {
      export_names: names,
      eval_steps: evaluation_steps,
    },
    status: K_UNINSTANTIATED,
    namespace_pin: -1,
    requests_array: empty,
    identity_hash: next_identity_hash(),
    exception_pin: -1,
    eval_promise_pin: -1,
  }));
  register_drop(iso, record);
  record as *const Module
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__SetSyntheticModuleExport(
  this: *const Module,
  isolate: *const RealIsolate,
  export_name: *const V8String,
  export_value: *const Value,
) -> MaybeBool {
  let iso = isolate as *mut RealIsolate;
  if this.is_null() || iso.is_null() {
    return MaybeBool::Nothing;
  }
  let rtw = iso_state(iso).rtw;
  let m = match module_ref(this) {
    Some(m) => m,
    None => return MaybeBool::Nothing,
  };
  if m.namespace_pin < 0 {
    // Create the exports object lazily if SetSyntheticModuleExport is called
    // before instantiate (V8 requires instantiate first, but be lenient).
    let ns = unsafe { v8x_hermes_object_new(rtw) };
    m.namespace_pin = unsafe { v8x_hermes_pin(rtw, ns) };
  }
  let ns_slot = unsafe { v8x_hermes_pin_get(rtw, m.namespace_pin) };
  let ok = unsafe {
    v8x_hermes_object_set(
      rtw,
      ns_slot,
      slot_of(export_name),
      slot_of(export_value),
    )
  };
  if ok != 0 {
    MaybeBool::JustTrue
  } else {
    MaybeBool::Nothing
  }
}

// ---- ModuleRequest ---------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__ModuleRequest__GetSpecifier(
  this: *const ModuleRequest,
) -> *const V8String {
  let rtw = current_rtw();
  if this.is_null() || rtw.is_null() {
    return ptr::null();
  }
  let rr = unsafe { &*(this as *const ModuleRequestRecord) };
  let slot = intern(rtw, &rr.specifier);
  slot_ptr::<V8String>(slot)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ModuleRequest__GetPhase(
  _this: *const ModuleRequest,
) -> ModuleImportPhase {
  ModuleImportPhase::kEvaluation
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ModuleRequest__GetSourceOffset(
  _this: *const ModuleRequest,
) -> i32 {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ModuleRequest__GetImportAttributes(
  this: *const ModuleRequest,
) -> *const FixedArray {
  if this.is_null() {
    return ptr::null();
  }
  let rr = unsafe { &*(this as *const ModuleRequestRecord) };
  rr.attributes
}

// ---- FixedArray ------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__FixedArray__Length(this: *const FixedArray) -> c_int {
  if this.is_null() {
    return 0;
  }
  let a = unsafe { &*(this as *const FixedArrayRecord) };
  a.elements.len() as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FixedArray__Get(
  this: *const FixedArray,
  index: c_int,
) -> *const Data {
  if this.is_null() || index < 0 {
    return ptr::null();
  }
  let a = unsafe { &*(this as *const FixedArrayRecord) };
  a.elements.get(index as usize).copied().unwrap_or(ptr::null())
}

// ---- Location --------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__Location__GetLineNumber(_this: *const c_void) -> c_int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Location__GetColumnNumber(_this: *const c_void) -> c_int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__SourceOffsetToLocation(
  _this: *const Module,
  _offset: c_int,
  out: *mut c_void,
) {
  // A Location is [i32; 2]; zero it.
  if !out.is_null() {
    unsafe { ptr::write_bytes(out as *mut u8, 0, 8) };
  }
}

// ---- helpers ---------------------------------------------------------------

/// Intern a Rust string into the JSI runtime, returning its handle slot.
fn intern(rtw: *mut c_void, s: &str) -> i64 {
  unsafe {
    v8x_hermes_string_new_utf8(rtw, s.as_ptr() as *const c_char, s.len())
  }
}

/// Wrap a raw pointer as a `Local<T>` for callback invocation. The vendored
/// callback ABI takes `Local` (a `NonNull<T>`); our pointers are non-null when
/// this is called.
#[inline]
unsafe fn local<'s, T>(p: *const T) -> crate::Local<'s, T> {
  unsafe { crate::Local::from_raw(p).unwrap() }
}

/// The resolve callback with its return struct flattened to the raw pointer it
/// wraps (ABI-identical on System V; the struct's field is private so we cannot
/// name it, hence the fn-pointer transmute at the call site).
type RawResolveCb = unsafe extern "C" fn(
  crate::Local<'static, Context>,
  crate::Local<'static, V8String>,
  crate::Local<'static, FixedArray>,
  crate::Local<'static, Module>,
) -> *const Module;

/// The synthetic evaluation steps, likewise flattened to its raw `*const Value`.
type RawSyntheticSteps = unsafe extern "C" fn(
  crate::Local<'static, Context>,
  crate::Local<'static, Module>,
) -> *const Value;

/// Build a resolved Promise (module evaluation returns a promise; reuse D1).
fn resolved_promise(rtw: *mut c_void, context: *const Context) -> *const Value {
  let resolver = unsafe { v8x_hermes_promise_resolver_new(rtw) };
  if resolver < 0 {
    return ptr::null();
  }
  let undef = unsafe { v8x_hermes_undefined(rtw) };
  unsafe {
    v8x_hermes_promise_resolver_resolve(rtw, resolver, undef);
  }
  let promise = unsafe {
    v8x_hermes_promise_resolver_get_promise(rtw, resolver)
  };
  let _ = context;
  slot_ptr::<Value>(promise)
}

// ---- source-to-closure transform ------------------------------------------

struct ParsedModule {
  closure_source: String,
  requests: Vec<ImportReq>,
  export_names: Vec<String>,
}

/// Transform ES-module source into a closure body that reads imports off
/// `__imports` and writes exports onto `__exports`. Focused on the forms the
/// deno_core boot graph and module rusty_v8 tests use. Documented in
/// docs/hermes-spike/experiments/D2-hermes-modules.md.
fn transform_module(src: &str) -> ParsedModule {
  let mut requests: Vec<ImportReq> = Vec::new();
  let mut export_names: Vec<String> = Vec::new();
  let mut body = String::with_capacity(src.len() + 64);
  // Prologue lines that bind imported names from __imports before the body.
  let mut import_prologue = String::new();

  for raw_line in src.lines() {
    let line = raw_line.trim_start();
    if let Some(rewritten) =
      rewrite_import(line, &mut requests, &mut import_prologue)
    {
      body.push_str(&rewritten);
      body.push('\n');
    } else if let Some(rewritten) = rewrite_export(line, &mut export_names) {
      body.push_str(&rewritten);
      body.push('\n');
    } else {
      body.push_str(raw_line);
      body.push('\n');
    }
  }

  // The module body is wrapped in an ASYNC function so a top-level `await`
  // (top-level await, TLA) in the module is legal syntax and its promise can be
  // returned to the caller. For a module WITHOUT any await this is still
  // correct: an async function with no await runs its body synchronously and
  // returns an already-resolved promise (exports are assigned before any await
  // point, matching V8's module-evaluation semantics). `v8__Module__Evaluate`
  // returns the promise this call produces, so deno_core awaits real TLA.
  let closure_source = format!(
    "(async function (__imports, __exports) {{\n{import_prologue}{body}}})"
  );
  ParsedModule {
    closure_source,
    requests,
    export_names,
  }
}

/// Register (or find) an import specifier and return its index.
fn record_request(requests: &mut Vec<ImportReq>, spec: &str) {
  if !requests.iter().any(|r| r.specifier == spec) {
    requests.push(ImportReq {
      specifier: spec.to_string(),
      resolved_ns_pin: None,
      resolved_module: None,
    });
  }
}

/// Rewrite a single `import ...` line. Returns the replacement body line, or
/// None if the line is not an import. Named/namespace/default bindings are
/// emitted into `prologue` as `const name = __imports["spec"].name;`.
fn rewrite_import(
  line: &str,
  requests: &mut Vec<ImportReq>,
  prologue: &mut String,
) -> Option<String> {
  let rest = line.strip_prefix("import")?;
  // Must be `import` followed by whitespace or `{`/`*`/quote (not `important`).
  let first = rest.chars().next()?;
  if !(first.is_whitespace() || first == '{' || first == '*' || first == '"'
    || first == '\'')
  {
    return None;
  }
  let rest = rest.trim();

  // Side-effect import: `import "spec";`
  if rest.starts_with('"') || rest.starts_with('\'') {
    if let Some(spec) = extract_quoted(rest) {
      record_request(requests, &spec);
    }
    return Some(String::new());
  }

  // Split into the binding clause and the `from "spec"` tail.
  let from_idx = rest.rfind(" from ").or_else(|| rest.rfind("from "))?;
  let (clause, tail) = rest.split_at(from_idx);
  let spec = extract_quoted(tail)?;
  record_request(requests, &spec);
  let clause = clause.trim();

  let access = format!("__imports[{:?}]", spec);

  // `import * as ns from "spec"`
  if let Some(ns) = clause.strip_prefix("* as ") {
    let ns = ns.trim();
    prologue.push_str(&format!("const {ns} = {access};\n"));
    return Some(String::new());
  }

  // `import defaultName, { a, b } from "spec"` or `import defaultName from ...`
  // or `import { a, b as c } from "spec"`.
  let mut default_name: Option<&str> = None;
  let mut named_part: Option<&str> = None;

  if let Some(brace_start) = clause.find('{') {
    let before = clause[..brace_start].trim().trim_end_matches(',').trim();
    if !before.is_empty() {
      default_name = Some(before);
    }
    let brace_end = clause.rfind('}').unwrap_or(clause.len());
    named_part = Some(&clause[brace_start + 1..brace_end]);
  } else {
    // Only a default binding.
    if !clause.is_empty() {
      default_name = Some(clause);
    }
  }

  if let Some(def) = default_name {
    prologue.push_str(&format!("const {def} = {access}.default;\n"));
  }
  if let Some(named) = named_part {
    for item in named.split(',') {
      let item = item.trim();
      if item.is_empty() {
        continue;
      }
      let (imported, local_name) = if let Some(idx) = item.find(" as ") {
        (item[..idx].trim(), item[idx + 4..].trim())
      } else {
        (item, item)
      };
      prologue
        .push_str(&format!("const {local_name} = {access}.{imported};\n"));
    }
  }
  Some(String::new())
}

/// Rewrite a single `export ...` line onto `__exports`. Returns the replacement
/// body line, or None if not an export.
fn rewrite_export(
  line: &str,
  export_names: &mut Vec<String>,
) -> Option<String> {
  let rest = line.strip_prefix("export")?;
  let first = rest.chars().next()?;
  if !(first.is_whitespace() || first == '{' || first == '*') {
    return None;
  }
  let rest = rest.trim();

  // `export default <expr>`
  if let Some(expr) = rest.strip_prefix("default ") {
    export_names.push("default".to_string());
    return Some(format!("__exports.default = ({});", expr.trim_end_matches(';')));
  }

  // `export function foo(...)` / `export async function foo` / `export class X`
  for kw in ["function ", "async function ", "class "] {
    if let Some(after) = rest.strip_prefix(kw) {
      let name = ident(after);
      if !name.is_empty() {
        export_names.push(name.clone());
        // Keep the declaration verbatim, then publish it onto __exports.
        let decl = line["export".len()..].trim_start();
        return Some(format!("{decl}\n__exports.{name} = {name};"));
      }
    }
  }

  // `export const x = ...` / `export let` / `export var`
  for kw in ["const ", "let ", "var "] {
    if let Some(after) = rest.strip_prefix(kw) {
      let name = ident(after);
      if !name.is_empty() {
        export_names.push(name.clone());
        let decl = &line["export".len()..].trim_start();
        return Some(format!("{decl}\n__exports.{name} = {name};"));
      }
    }
  }

  // `export { a, b as c }` (optionally `from "spec"` re-export not supported).
  if rest.starts_with('{') {
    let end = rest.rfind('}').unwrap_or(rest.len());
    let inner = &rest[1..end];
    let mut out = String::new();
    for item in inner.split(',') {
      let item = item.trim();
      if item.is_empty() {
        continue;
      }
      let (local_name, exported) = if let Some(idx) = item.find(" as ") {
        (item[..idx].trim(), item[idx + 4..].trim())
      } else {
        (item, item)
      };
      export_names.push(exported.to_string());
      out.push_str(&format!("__exports.{exported} = {local_name};\n"));
    }
    return Some(out);
  }

  // Unknown export form: drop the `export` keyword and keep the rest.
  Some(rest.to_string())
}

/// Extract the first quoted string literal from `s` (single or double quotes).
fn extract_quoted(s: &str) -> Option<String> {
  let bytes = s.as_bytes();
  let mut i = 0;
  while i < bytes.len() {
    let c = bytes[i] as char;
    if c == '"' || c == '\'' {
      let quote = c;
      let start = i + 1;
      let mut j = start;
      while j < bytes.len() && bytes[j] as char != quote {
        j += 1;
      }
      if j <= bytes.len() {
        return Some(s[start..j].to_string());
      }
    }
    i += 1;
  }
  None
}

/// Read a JS identifier from the start of `s`.
fn ident(s: &str) -> String {
  s.trim_start()
    .chars()
    .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
    .collect()
}
