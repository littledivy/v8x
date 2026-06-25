//! JSC-backed definitions for the "module" family:
//! Module / ModuleRequest / Script / ScriptCompiler / UnboundScript /
//! UnboundModuleScript / FixedArray / ScriptOrigin.
//!
//! Script compilation/execution is implemented for real via JSEvaluateScript:
//! a `Script`/`UnboundScript` handle simply carries the source-text JSValueRef,
//! and `Run` re-stringifies it and evaluates it in the current context.
//! ES-module-specific functions are inert because the JSC C API exposes no
//! module loader / linker.
#![allow(non_snake_case, unused)]

use std::mem::MaybeUninit;
use std::ptr;

use crate::jsc::core::{
  ctx_of, current_ctx, current_iso, intern, intern_ctx, iso_state, jsval,
};
use crate::jsc::jsc_sys::*;
use crate::{
  Context, Data, FixedArray, Function, Message, Module, ModuleRequest, Object,
  Primitive, RealIsolate, Script, String as V8String, UnboundModuleScript,
  UnboundScript, Value,
};

use crate::isolate::ModuleImportPhase;
use crate::module::{
  Location, ModuleStatus, ResolveModuleCallback, ResolveModuleCallbackRet,
  ResolveSourceCallback, StalledTopLevelAwaitMessage,
  SyntheticModuleEvaluationSteps, SyntheticModuleEvaluationStepsRet,
};
use crate::script::ScriptOrigin;
use crate::script_compiler::{
  CachedData, CompileOptions, NoCacheReason, Source,
};
use crate::support::{Maybe, MaybeBool, int};

#[repr(C)]
struct ModJSClassDefinition {
  version: std::os::raw::c_int,
  attributes: u32,
  className: *const std::os::raw::c_char,
  parentClass: JSClassRef,
  staticValues: *const std::os::raw::c_void,
  staticFunctions: *const std::os::raw::c_void,
  initialize: *const std::os::raw::c_void,
  finalize: *const std::os::raw::c_void,
  hasProperty: *const std::os::raw::c_void,
  getProperty: *const std::os::raw::c_void,
  setProperty: *const std::os::raw::c_void,
  deleteProperty: *const std::os::raw::c_void,
  getPropertyNames: *const std::os::raw::c_void,
  callAsFunction: *const std::os::raw::c_void,
  callAsConstructor: *const std::os::raw::c_void,
  hasInstance: *const std::os::raw::c_void,
  convertToType: *const std::os::raw::c_void,
}

struct SyntheticModule {
  ctx: JSGlobalContextRef,
  status: ModuleStatus,
  export_names: Vec<std::string::String>,

  eval_steps: Option<SyntheticModuleEvaluationSteps<'static>>,

  source: Option<std::string::String>,

  import_specifiers: Vec<std::string::String>,

  namespace: JSObjectRef,

  specifier: std::string::String,

  dependencies: Vec<*const Module>,

  is_async: bool,
}

unsafe extern "C" fn mod_finalize(object: JSObjectRef) {
  let p = unsafe { JSObjectGetPrivate(object) } as *mut SyntheticModule;
  if !p.is_null() {
    let m = unsafe { Box::from_raw(p) };
    if !m.namespace.is_null() && !m.ctx.is_null() {
      unsafe { JSValueUnprotect(m.ctx, m.namespace as JSValueRef) };
    }
  }
}

thread_local! {
    static MOD_CLASS: std::cell::Cell<JSClassRef> =
        const { std::cell::Cell::new(ptr::null_mut()) };
}

fn mod_class() -> JSClassRef {
  MOD_CLASS.with(|c| {
    let existing = c.get();
    if !existing.is_null() {
      return existing;
    }
    let def = ModJSClassDefinition {
      version: 0,
      attributes: 0,
      className: c"v8jsc_module".as_ptr(),
      parentClass: ptr::null_mut(),
      staticValues: ptr::null(),
      staticFunctions: ptr::null(),
      initialize: ptr::null(),
      finalize: mod_finalize as *const std::os::raw::c_void,
      hasProperty: ptr::null(),
      getProperty: ptr::null(),
      setProperty: ptr::null(),
      deleteProperty: ptr::null(),
      getPropertyNames: ptr::null(),
      callAsFunction: ptr::null(),
      callAsConstructor: ptr::null(),
      hasInstance: ptr::null(),
      convertToType: ptr::null(),
    };
    let cls =
      unsafe { JSClassCreate(&def as *const _ as *const JSClassDefinition) };
    c.set(cls);
    cls
  })
}

#[inline]
fn module_state<'a>(this: *const Module) -> Option<&'a mut SyntheticModule> {
  if this.is_null() {
    return None;
  }
  let obj = jsval(this) as JSObjectRef;
  let p = unsafe { JSObjectGetPrivate(obj) } as *mut SyntheticModule;
  if p.is_null() {
    None
  } else {
    Some(unsafe { &mut *p })
  }
}

unsafe fn eval_value_as_script(
  ctx: JSContextRef,
  source_val: JSValueRef,
) -> JSValueRef {
  if ctx.is_null() || source_val.is_null() {
    return ptr::null();
  }
  let mut exc: JSValueRef = ptr::null();
  let src_str = unsafe { JSValueToStringCopy(ctx, source_val, &mut exc) };
  if src_str.is_null() {
    return ptr::null();
  }
  let result = unsafe {
    JSEvaluateScript(
      ctx,
      src_str,
      ptr::null_mut(),
      ptr::null_mut(),
      1,
      &mut exc,
    )
  };
  unsafe { JSStringRelease(src_str) };
  result
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FixedArray__Length(this: *const FixedArray) -> int {
  let ctx = current_ctx();
  let v = jsval(this);
  if ctx.is_null() || v.is_null() {
    return 0;
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let obj = JSValueToObject(ctx, v, &mut exc);
    if obj.is_null() {
      return 0;
    }
    let name = JSStringCreateWithUTF8CString(c"length".as_ptr());
    let len_val = JSObjectGetProperty(ctx, obj, name, &mut exc);
    JSStringRelease(name);
    if len_val.is_null() {
      return 0;
    }
    JSValueToNumber(ctx, len_val, &mut exc) as int
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FixedArray__Get(
  this: *const FixedArray,
  index: int,
) -> *const Data {
  let ctx = current_ctx();
  let v = jsval(this);
  if ctx.is_null() || v.is_null() || index < 0 {
    return ptr::null();
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let obj = JSValueToObject(ctx, v, &mut exc);
    if obj.is_null() {
      return ptr::null();
    }
    let elem = JSObjectGetPropertyAtIndex(ctx, obj, index as u32, &mut exc);
    intern_ctx::<Data>(ctx, elem)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Script__Compile(
  context: *const Context,
  source: *const V8String,
  _origin: *const ScriptOrigin,
) -> *const Script {
  let ctx = ctx_of(context) as JSContextRef;
  let src_val = jsval(source);
  if ctx.is_null() || src_val.is_null() {
    return ptr::null();
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let src_str = JSValueToStringCopy(ctx, src_val, &mut exc);
    if src_str.is_null() {
      return ptr::null();
    }
    let ok = JSCheckScriptSyntax(ctx, src_str, ptr::null_mut(), 1, &mut exc);
    JSStringRelease(src_str);
    if !ok {
      return ptr::null();
    }
  }
  intern_ctx::<Script>(ctx, src_val)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Script__GetUnboundScript(
  script: *const Script,
) -> *const UnboundScript {
  intern::<UnboundScript>(jsval(script))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Script__Run(
  script: *const Script,
  context: *const Context,
) -> *const Value {
  let ctx = ctx_of(context) as JSContextRef;
  let result = unsafe { eval_value_as_script(ctx, jsval(script)) };
  if result.is_null() {
    return ptr::null();
  }
  intern_ctx::<Value>(ctx, result)
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
unsafe fn source_string_of(source: *mut Source) -> JSValueRef {
  if source.is_null() {
    return ptr::null();
  }
  unsafe { *(source as *const usize) as JSValueRef }
}

unsafe fn jsstring_to_rust(
  ctx: JSContextRef,
  v: JSValueRef,
) -> std::string::String {
  if ctx.is_null() || v.is_null() {
    return std::string::String::new();
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let s = JSValueToStringCopy(ctx, v, &mut exc);
    if s.is_null() {
      return std::string::String::new();
    }
    let max = JSStringGetMaximumUTF8CStringSize(s);
    let mut buf = vec![0u8; max];
    let n = JSStringGetUTF8CString(s, buf.as_mut_ptr() as *mut _, max);
    JSStringRelease(s);
    if n == 0 {
      return std::string::String::new();
    }
    buf.truncate(n - 1);
    std::string::String::from_utf8(buf).unwrap_or_default()
  }
}

unsafe fn resource_name_of(
  ctx: JSContextRef,
  source: *mut Source,
) -> std::string::String {
  if source.is_null() || ctx.is_null() {
    return std::string::String::new();
  }
  let name_val = unsafe { *((source as *const usize).add(1)) } as JSValueRef;
  if name_val.is_null() {
    return std::string::String::new();
  }
  unsafe {
    if JSValueIsUndefined(ctx, name_val) || JSValueIsNull(ctx, name_val) {
      return std::string::String::new();
    }
    let mut exc: JSValueRef = ptr::null();
    let s = JSValueToStringCopy(ctx, name_val, &mut exc);
    if s.is_null() {
      return std::string::String::new();
    }
    let max = JSStringGetMaximumUTF8CStringSize(s);
    let mut buf = vec![0u8; max];
    let n = JSStringGetUTF8CString(s, buf.as_mut_ptr() as *mut _, max);
    JSStringRelease(s);
    if n == 0 {
      return std::string::String::new();
    }
    buf.truncate(n - 1);
    std::string::String::from_utf8(buf).unwrap_or_default()
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CachedData__NEW<'a>(
  data: *const u8,
  length: i32,
) -> *mut CachedData<'a> {
  #[repr(C)]
  struct RawCachedData {
    data: *const u8,
    length: i32,
    rejected: bool,
    buffer_policy: i32,
  }
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
  #[repr(C)]
  struct RawCachedData {
    data: *const u8,
    length: i32,
    rejected: bool,
    buffer_policy: i32,
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
  let ctx = ctx_of(context) as JSContextRef;
  let src_val = unsafe { source_string_of(source) };
  if ctx.is_null() || src_val.is_null() {
    return ptr::null();
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let src_str = JSValueToStringCopy(ctx, src_val, &mut exc);
    if src_str.is_null() {
      return ptr::null();
    }
    let ok = JSCheckScriptSyntax(ctx, src_str, ptr::null_mut(), 1, &mut exc);
    JSStringRelease(src_str);
    if !ok {
      return ptr::null();
    }
  }
  intern_ctx::<Script>(ctx, src_val)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CompileModule(
  isolate: *mut RealIsolate,
  source: *mut Source,
  _options: CompileOptions,
  _no_cache_reason: NoCacheReason,
) -> *const Module {
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  let src_val = unsafe { source_string_of(source) };
  if ctx.is_null() || src_val.is_null() {
    return ptr::null();
  }
  let gctx = unsafe { JSContextGetGlobalContext(ctx) };

  let text = unsafe {
    let mut exc: JSValueRef = ptr::null();
    let s = JSValueToStringCopy(ctx, src_val, &mut exc);
    if s.is_null() {
      return ptr::null();
    }
    let max = JSStringGetMaximumUTF8CStringSize(s);
    let mut buf = vec![0u8; max];
    let n = JSStringGetUTF8CString(s, buf.as_mut_ptr() as *mut _, max);
    JSStringRelease(s);
    if n == 0 {
      return ptr::null();
    }
    buf.truncate(n - 1);
    match std::string::String::from_utf8(buf) {
      Ok(t) => t,
      Err(_) => return ptr::null(),
    }
  };

  let specifier = unsafe { resource_name_of(ctx, source) };

  let Some(rewrite) = rewrite_es_module(&text) else {
    return ptr::null();
  };

  let namespace =
    unsafe { JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut()) };
  unsafe { JSValueProtect(gctx, namespace as JSValueRef) };

  // Register the namespace into the global registry NOW (at compile), not at
  // Evaluate — a barrel module evaluates last, but its importers read its
  // namespace earlier. Install live re-export getters so `export { X } from
  // spec` resolves through to spec's namespace at access time (after spec's
  // body runs), matching ESM live re-export bindings.
  unsafe {
    register_module_namespace(ctx, &specifier, namespace);
    install_reexport_getters(ctx, namespace, &rewrite.reexports);
  }

  let state = Box::new(SyntheticModule {
    ctx: gctx,
    status: ModuleStatus::Uninstantiated,
    export_names: rewrite.export_names,
    eval_steps: None,
    source: Some(rewrite.body),
    import_specifiers: rewrite.imports,
    namespace,
    specifier,
    dependencies: Vec::new(),
    is_async: rewrite.is_async,
  });
  let obj =
    unsafe { JSObjectMake(ctx, mod_class(), Box::into_raw(state) as *mut _) };
  intern_ctx::<Module>(ctx, obj as JSValueRef)
}

struct RewrittenModule {
  body: std::string::String,
  export_names: Vec<std::string::String>,
  imports: Vec<std::string::String>,
  is_async: bool,
  // (exported, source spec, source local): `export { local as exported } from
  // spec`. Installed as LIVE getters on the namespace at compile time so a
  // barrel's re-exports resolve through to the source module even before the
  // barrel's own body runs (ESM live re-export bindings).
  reexports: Vec<(
    std::string::String,
    std::string::String,
    std::string::String,
  )>,
}

fn strip_js_comments(src: &str) -> std::string::String {
  let b = src.as_bytes();
  let n = b.len();
  let mut out: Vec<u8> = Vec::with_capacity(n);
  let mut i = 0usize;
  let mut prev_sig = 0u8;
  let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';
  while i < n {
    let c = b[i];
    match c {
      b'/' if i + 1 < n && b[i + 1] == b'/' => {
        i += 2;
        while i < n && b[i] != b'\n' {
          i += 1;
        }
      }
      b'/' if i + 1 < n && b[i + 1] == b'*' => {
        i += 2;
        while i + 1 < n && !(b[i] == b'*' && b[i + 1] == b'/') {
          i += 1;
        }
        i += 2;
      }
      b'"' | b'\'' => {
        let q = c;
        out.push(c);
        i += 1;
        while i < n {
          out.push(b[i]);
          if b[i] == b'\\' && i + 1 < n {
            out.push(b[i + 1]);
            i += 2;
            continue;
          }
          if b[i] == q {
            i += 1;
            break;
          }
          i += 1;
        }
        prev_sig = q;
      }
      b'`' => {
        out.push(b'`');
        i += 1;
        while i < n {
          out.push(b[i]);
          if b[i] == b'\\' && i + 1 < n {
            out.push(b[i + 1]);
            i += 2;
            continue;
          }
          if b[i] == b'`' {
            i += 1;
            break;
          }
          i += 1;
        }
        prev_sig = b'`';
      }
      b'/' => {
        let regex_ok = matches!(
          prev_sig,
          0 | b'('
            | b','
            | b'='
            | b':'
            | b'['
            | b'!'
            | b'&'
            | b'|'
            | b'?'
            | b'{'
            | b'}'
            | b';'
            | b'+'
            | b'-'
            | b'*'
            | b'%'
            | b'<'
            | b'>'
            | b'^'
            | b'~'
        );
        if regex_ok {
          out.push(b'/');
          i += 1;
          let mut in_class = false;
          while i < n {
            out.push(b[i]);
            if b[i] == b'\\' && i + 1 < n {
              out.push(b[i + 1]);
              i += 2;
              continue;
            }
            match b[i] {
              b'[' => in_class = true,
              b']' => in_class = false,
              b'/' if !in_class => {
                i += 1;
                break;
              }
              _ => {}
            }
            i += 1;
          }
          prev_sig = b'/';
        } else {
          out.push(b'/');
          prev_sig = b'/';
          i += 1;
        }
      }
      _ => {
        if !c.is_ascii_whitespace() {
          prev_sig = if is_ident(c) { b'a' } else { c };
        }
        out.push(c);
        i += 1;
      }
    }
  }
  std::string::String::from_utf8_lossy(&out).into_owned()
}

fn join_module_statements(src: &str) -> Vec<std::string::String> {
  let lines: Vec<&str> = src.lines().collect();
  let mut result: Vec<std::string::String> = Vec::new();
  let mut i = 0;
  while i < lines.len() {
    let trimmed = lines[i].trim();
    // Only JOIN true named-binding lists (`import {`, `export {`, `import a,
    // {`). A declaration like `export class X {` / `export function f() {` also
    // "starts with export and has an unclosed {", but its `{` opens a body —
    // joining to the next `}` would swallow the declaration into the `export {}`
    // handler and drop the class/function header.
    let after_kw = trimmed
      .strip_prefix("import")
      .or_else(|| trimmed.strip_prefix("export"))
      .map(str::trim_start)
      .unwrap_or("");
    let import_binding = trimmed.starts_with("import")
      && (after_kw.starts_with('{')
        || after_kw
          .split_once(',')
          .map(|(_, r)| r.trim_start().starts_with('{'))
          .unwrap_or(false));
    let export_binding =
      trimmed.starts_with("export") && after_kw.starts_with('{');
    let starts_brace_stmt = (import_binding || export_binding)
      && trimmed.contains('{')
      && !trimmed.contains('}');
    if starts_brace_stmt {
      let mut buf = std::string::String::new();
      buf.push_str(lines[i].trim_end());
      buf.push(' ');
      let mut closed = false;
      i += 1;
      while i < lines.len() {
        let l = lines[i];
        buf.push_str(l.trim());
        buf.push(' ');
        if l.contains('}') {
          closed = true;

          i += 1;

          if !buf.contains(" from ")
            && !buf.trim_end().ends_with(';')
            && i < lines.len()
            && lines[i].trim_start().starts_with("from ")
          {
            buf.push_str(lines[i].trim());
            buf.push(' ');
            i += 1;
          }
          break;
        }
        i += 1;
      }
      let _ = closed;
      result.push(buf.split_whitespace().collect::<Vec<_>>().join(" "));
    } else {
      result.push(lines[i].to_string());
      i += 1;
    }
  }
  result
}

fn strip_leading_block_comments(mut s: &str) -> &str {
  loop {
    s = s.trim_start();
    if let Some(rest) = s.strip_prefix("/*") {
      if let Some(end) = rest.find("*/") {
        s = &rest[end + 2..];
        continue;
      }
    }
    return s;
  }
}

fn rewrite_es_module(src: &str) -> Option<RewrittenModule> {
  let mut out = std::string::String::new();

  let mut imports_out = std::string::String::new();

  let mut exports_out = std::string::String::new();
  let mut export_names: Vec<std::string::String> = Vec::new();
  let mut imports: Vec<std::string::String> = Vec::new();
  let mut reexports: Vec<(
    std::string::String,
    std::string::String,
    std::string::String,
  )> = Vec::new();
  // localName -> (source spec, source export name) for every imported binding,
  // so a later `export { localName }` can re-export it LIVE to the source.
  let mut import_bindings: std::collections::HashMap<
    std::string::String,
    (std::string::String, std::string::String),
  > = std::collections::HashMap::new();

  let cleaned = strip_js_comments(src);
  let logical = join_module_statements(&cleaned);

  for raw_line in logical.iter() {
    let raw_line: &str = raw_line.as_str();
    let line = raw_line.trim_start();
    let trimmed = strip_leading_block_comments(line.trim());

    if trimmed.starts_with("export *") {
      return None;
    }

    if trimmed.starts_with("import ") || trimmed == "import" {
      let spec = extract_specifier(trimmed).unwrap_or_default();
      if !spec.is_empty() {
        imports.push(spec.clone());
      }
      let module_expr =
        format!("((globalThis.__v8jsc_modules||{{}})[{:?}]||{{}})", spec);

      let clause =
        if let Some((c, _)) = trimmed["import".len()..].split_once(" from ") {
          c.trim()
        } else {
          ""
        };

      if clause.contains('{') {
        let brace_start = clause.find('{').unwrap();
        let head = clause[..brace_start].trim().trim_end_matches(',').trim();
        if !head.is_empty() {
          imports_out
            .push_str(&format!("const {} = {}.default;\n", head, module_expr));
          import_bindings
            .insert(head.to_string(), (spec.clone(), "default".to_string()));
        }

        let names = between(trimmed, '{', '}').unwrap_or_default();
        let mut destructure: Vec<std::string::String> = Vec::new();
        for part in names.split(',') {
          let part = part.trim();
          if part.is_empty() {
            continue;
          }
          if let Some((l, r)) = part.split_once(" as ") {
            destructure.push(format!("{}: {}", l.trim(), r.trim()));
            import_bindings.insert(
              r.trim().to_string(),
              (spec.clone(), l.trim().to_string()),
            );
          } else {
            destructure.push(part.to_string());
            import_bindings
              .insert(part.to_string(), (spec.clone(), part.to_string()));
          }
        }
        imports_out.push_str(&format!(
          "const {{ {} }} = {};\n",
          destructure.join(", "),
          module_expr
        ));
      } else if clause.starts_with("* as ") {
        let name = clause["* as ".len()..].trim();
        if !name.is_empty() {
          imports_out.push_str(&format!("const {} = {};\n", name, module_expr));
          import_bindings
            .insert(name.to_string(), (spec.clone(), "*".to_string()));
        }
      } else if !clause.is_empty() && trimmed.contains(" from ") {
        imports_out
          .push_str(&format!("const {} = {}.default;\n", clause, module_expr));
        import_bindings
          .insert(clause.to_string(), (spec.clone(), "default".to_string()));
      }

      continue;
    }

    if trimmed.starts_with("export {") || trimmed.starts_with("export{") {
      let inner = between(trimmed, '{', '}').unwrap_or_default();

      let reexport_spec = if trimmed.contains(" from ") {
        let spec = extract_specifier(trimmed).unwrap_or_default();
        if !spec.is_empty() {
          imports.push(spec.clone());
        }
        Some(spec)
      } else {
        None
      };
      for part in inner.split(',') {
        let part = part.trim();
        if part.is_empty() {
          continue;
        }
        let (local, exported) = if let Some((l, r)) = part.split_once(" as ") {
          (l.trim().to_string(), r.trim().to_string())
        } else {
          (part.to_string(), part.to_string())
        };
        export_names.push(exported.clone());
        if let Some(spec) = &reexport_spec {
          // `export { local } from spec`: live re-export to spec.
          reexports.push((exported.clone(), spec.clone(), local.clone()));
        } else if let Some((spec, source)) = import_bindings.get(&local) {
          // `import X from spec; export { X }` (barrel pattern): X is an
          // imported binding, so re-export LIVE to its source module instead of
          // snapshotting at body time (the barrel body runs last, after its
          // importers already read the binding).
          reexports.push((exported.clone(), spec.clone(), source.clone()));
        } else {
          exports_out.push_str(&format!("__ns[{:?}] = {};\n", exported, local));
        }
      }
      continue;
    }

    if trimmed.starts_with("export default ") {
      // The default expression may span multiple lines (`export default {`
      // ...multi-line object/class/function...). Emit a `const` binding inline
      // in `out` so the body stays CONTIGUOUS with its opening, and assign the
      // namespace slot from that binding at the end. (A prior single-line
      // `__ns["default"] = (expr)` cut multi-line objects to `({)`.)
      let expr = &trimmed["export default ".len()..];
      export_names.push("default".to_string());
      out.push_str("const __v8jsc_default = ");
      out.push_str(expr);
      out.push('\n');
      exports_out.push_str("__ns[\"default\"] = __v8jsc_default;\n");
      continue;
    }

    if trimmed.starts_with("export const ")
      || trimmed.starts_with("export let ")
      || trimmed.starts_with("export var ")
      || trimmed.starts_with("export function ")
      || trimmed.starts_with("export async function ")
      || trimmed.starts_with("export class ")
    {
      let rest = trimmed.strip_prefix("export ").unwrap();

      let after_kw = rest
        .trim_start_matches("const ")
        .trim_start_matches("let ")
        .trim_start_matches("var ")
        .trim_start_matches("async function ")
        .trim_start_matches("function ")
        .trim_start_matches("class ");

      if after_kw.trim_start().starts_with('{') {
        out.push_str(rest);
        out.push('\n');
        let inner = between(after_kw, '{', '}').unwrap_or_default();
        for part in inner.split(',') {
          let part = part.trim();
          if part.is_empty() {
            continue;
          }

          let local = part
            .split(':')
            .next_back()
            .unwrap_or(part)
            .split('=')
            .next()
            .unwrap_or(part)
            .trim();
          let name: std::string::String = local
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
            .collect();
          if !name.is_empty() {
            export_names.push(name.clone());
            exports_out.push_str(&format!("__ns[{:?}] = {};\n", name, name));
          }
        }
        continue;
      }
      let name: std::string::String = after_kw
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
        .collect();
      out.push_str(rest);
      out.push('\n');
      if !name.is_empty() {
        export_names.push(name.clone());
        exports_out.push_str(&format!("__ns[{:?}] = {};\n", name, name));
      }
      continue;
    }

    out.push_str(raw_line);
    out.push('\n');
  }

  let combined = format!("{}{}{}", imports_out, out, exports_out);

  let body = combined.replace("import.meta", "__v8jsc_meta");

  Some(RewrittenModule {
    body,
    export_names,
    imports,
    is_async: has_top_level_await(src),
    reexports,
  })
}

fn has_top_level_await(src: &str) -> bool {
  let b = src.as_bytes();
  let n = b.len();
  let mut i = 0usize;

  let mut fn_body_depths: Vec<i32> = Vec::new();
  let mut depth: i32 = 0;

  let mut pending_fn_body = false;

  let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';

  while i < n {
    let c = b[i];
    match c {
      b'/' if i + 1 < n && b[i + 1] == b'/' => {
        i += 2;
        while i < n && b[i] != b'\n' {
          i += 1;
        }
      }

      b'/' if i + 1 < n && b[i + 1] == b'*' => {
        i += 2;
        while i + 1 < n && !(b[i] == b'*' && b[i + 1] == b'/') {
          i += 1;
        }
        i += 2;
      }

      b'"' | b'\'' => {
        let q = c;
        i += 1;
        while i < n {
          if b[i] == b'\\' {
            i += 2;
            continue;
          }
          if b[i] == q {
            i += 1;
            break;
          }
          i += 1;
        }
      }

      b'`' => {
        i += 1;
        while i < n {
          if b[i] == b'\\' {
            i += 2;
            continue;
          }
          if b[i] == b'`' {
            i += 1;
            break;
          }
          i += 1;
        }
      }
      b'{' => {
        depth += 1;
        if pending_fn_body {
          fn_body_depths.push(depth);
          pending_fn_body = false;
        }
        i += 1;
      }
      b'}' => {
        if let Some(&d) = fn_body_depths.last() {
          if d == depth {
            fn_body_depths.pop();
          }
        }
        depth -= 1;
        i += 1;
      }

      b'=' if i + 1 < n && b[i + 1] == b'>' => {
        pending_fn_body = true;
        i += 2;
      }
      _ if is_ident(c) => {
        let start = i;
        while i < n && is_ident(b[i]) {
          i += 1;
        }
        let word = &src[start..i];
        let prev_is_dot = start > 0 && {
          let mut j = start;
          while j > 0 && (b[j - 1] as char).is_whitespace() {
            j -= 1;
          }
          j > 0 && b[j - 1] == b'.'
        };
        match word {
          "function" => {
            pending_fn_body = true;
          }
          "await" if !prev_is_dot => {
            // Genuine top-level await is at brace-depth 0 (a module-top
            // statement). The function-body tracker misses async METHODS
            // (`async foo() {}`, `async *g() {}`, `for await` inside them) —
            // they have no `function` keyword/`=>`, so their await would
            // false-positive as top-level and wrongly mark the module async,
            // deadlocking its importers. Requiring depth 0 avoids that. (Misses
            // the rare TLA inside a top-level `if`/`for` block; acceptable.)
            if depth == 0 && fn_body_depths.is_empty() {
              return true;
            }
          }
          _ => {}
        }
      }
      _ => {
        i += 1;
      }
    }
  }
  false
}

fn extract_specifier(line: &str) -> Option<std::string::String> {
  let scan = match line.rfind(" from ") {
    Some(p) => &line[p + 6..],
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

fn between(s: &str, open: char, close: char) -> Option<std::string::String> {
  let a = s.find(open)?;
  let b = s[a + 1..].find(close)? + a + 1;
  Some(s[a + 1..b].to_string())
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
  let ctx = ctx_of(context) as JSContextRef;
  let src_val = unsafe { source_string_of(source) };
  if ctx.is_null() || src_val.is_null() {
    return ptr::null();
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();

    let src_str = JSValueToStringCopy(ctx, src_val, &mut exc);
    if src_str.is_null() {
      return ptr::null();
    }
    let max = JSStringGetMaximumUTF8CStringSize(src_str);
    let mut body = vec![0u8; max];
    JSStringGetUTF8CString(src_str, body.as_mut_ptr() as *mut _, max);
    JSStringRelease(src_str);
    let body_str = std::ffi::CStr::from_ptr(body.as_ptr() as *const _)
      .to_string_lossy()
      .into_owned();

    let mut arg_names: Vec<std::string::String> = Vec::new();
    if !arguments.is_null() {
      for i in 0..arguments_count {
        let a = *arguments.add(i);
        let av = jsval(a);
        if av.is_null() {
          continue;
        }
        let s = JSValueToStringCopy(ctx, av, &mut exc);
        if s.is_null() {
          continue;
        }
        let m = JSStringGetMaximumUTF8CStringSize(s);
        let mut nbuf = vec![0u8; m];
        JSStringGetUTF8CString(s, nbuf.as_mut_ptr() as *mut _, m);
        JSStringRelease(s);
        arg_names.push(
          std::ffi::CStr::from_ptr(nbuf.as_ptr() as *const _)
            .to_string_lossy()
            .into_owned(),
        );
      }
    }

    let wrapped =
      format!("(function({}) {{\n{}\n}})", arg_names.join(","), body_str);
    let cstr = match std::ffi::CString::new(wrapped) {
      Ok(c) => c,
      Err(_) => return ptr::null(),
    };
    let js_src = JSStringCreateWithUTF8CString(cstr.as_ptr());
    let result = JSEvaluateScript(
      ctx,
      js_src,
      ptr::null_mut(),
      ptr::null_mut(),
      1,
      &mut exc,
    );
    JSStringRelease(js_src);
    if result.is_null() {
      // Record the compile exception so deno's TryCatch sees has_caught() —
      // returning null with no pending exception trips its assert -> panic.
      let exc = if exc.is_null() {
        make_generic_error(ctx, "CompileFunction failed")
      } else {
        exc
      };
      crate::jsc::core::record_pending_exception(ctx, exc);
      return ptr::null();
    }
    intern_ctx::<Function>(ctx, result)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundScript__CreateCodeCache(
  _script: *const UnboundScript,
) -> *mut CachedData<'static> {
  ptr::null_mut()
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
pub extern "C" fn v8__UnboundModuleScript__GetSourceMappingURL(
  _script: *const UnboundModuleScript,
) -> *const Value {
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }
  let v = unsafe { JSValueMakeUndefined(ctx) };
  intern_ctx::<Value>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundModuleScript__GetSourceURL(
  _script: *const UnboundModuleScript,
) -> *const Value {
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }
  let v = unsafe { JSValueMakeUndefined(ctx) };
  intern_ctx::<Value>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetStatus(this: *const Module) -> ModuleStatus {
  match module_state(this) {
    Some(m) => match m.status {
      ModuleStatus::Uninstantiated => ModuleStatus::Uninstantiated,
      ModuleStatus::Instantiating => ModuleStatus::Instantiating,
      ModuleStatus::Instantiated => ModuleStatus::Instantiated,
      ModuleStatus::Evaluating => ModuleStatus::Evaluating,
      ModuleStatus::Evaluated => ModuleStatus::Evaluated,
      ModuleStatus::Errored => ModuleStatus::Errored,
    },

    None => ModuleStatus::Errored,
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetException(
  _this: *const Module,
) -> *const Value {
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }
  let v = unsafe { JSValueMakeUndefined(ctx) };
  intern_ctx::<Value>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetModuleRequests(
  this: *const Module,
) -> *const FixedArray {
  let ctx = module_state(this)
    .map(|m| m.ctx as JSContextRef)
    .unwrap_or_else(current_ctx);
  if ctx.is_null() {
    return ptr::null();
  }
  let specs: Vec<std::string::String> = module_state(this)
    .map(|m| m.import_specifiers.clone())
    .unwrap_or_default();

  let mut elems: Vec<JSValueRef> = Vec::with_capacity(specs.len());
  unsafe {
    for spec in &specs {
      let req = JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut());
      if let Ok(cspec) = std::ffi::CString::new(spec.as_str()) {
        let sval_str = JSStringCreateWithUTF8CString(cspec.as_ptr());
        let sval = JSValueMakeString(ctx, sval_str);
        JSStringRelease(sval_str);
        let key = JSStringCreateWithUTF8CString(c"specifier".as_ptr());
        JSObjectSetProperty(ctx, req, key, sval, 0, ptr::null_mut());
        JSStringRelease(key);
      }

      let mark =
        JSStringCreateWithUTF8CString(c"__v8jsc_module_request".as_ptr());
      JSObjectSetProperty(
        ctx,
        req,
        mark,
        JSValueMakeBoolean(ctx, true),
        1 << 1,
        ptr::null_mut(),
      );
      JSStringRelease(mark);
      elems.push(req as JSValueRef);
    }
    let mut exc: JSValueRef = ptr::null();
    let arr = JSObjectMakeArray(ctx, elems.len(), elems.as_ptr(), &mut exc);
    if arr.is_null() {
      return ptr::null();
    }
    intern_ctx::<FixedArray>(ctx, arr as JSValueRef)
  }
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
  match module_state(this) {
    Some(m) if !m.namespace.is_null() => {
      intern_ctx::<Value>(m.ctx as JSContextRef, m.namespace as JSValueRef)
    }
    _ => ptr::null(),
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetModuleNamespace2(
  _this: *const Module,
  _phase: ModuleImportPhase,
) -> *const Value {
  ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__EvaluateForImportDefer(
  _this: *const Module,
  _context: *const Context,
) -> *const Value {
  ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetIdentityHash(_this: *const Module) -> int {
  (_this as usize as int) ^ 0x4d4f_44
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__InstantiateModule(
  this: *const Module,
  context: *const Context,
  cb: ResolveModuleCallback,
  _source_callback: Option<ResolveSourceCallback>,
) -> MaybeBool {
  let ctx = ctx_of(context) as JSContextRef;

  let iso = current_iso();
  let Some(m) = module_state(this) else {
    return MaybeBool::JustFalse;
  };
  if !matches!(m.status, ModuleStatus::Uninstantiated) {
    return MaybeBool::JustTrue;
  }
  m.status = ModuleStatus::Instantiating;
  let specs = m.import_specifiers.clone();

  let mut deps: Vec<*const Module> = Vec::new();
  for spec in &specs {
    crate::jsc::core::restore_current(iso);
    let dep = unsafe { resolve_dependency(context, cb, this, spec) };
    if dep.is_null() {
      continue;
    }

    if let Some(dm) = module_state(dep) {
      if matches!(dm.status, ModuleStatus::Uninstantiated) {
        let _ =
          v8__Module__InstantiateModule(dep, context, cb, _source_callback);
      }
    }
    deps.push(dep);
  }
  crate::jsc::core::restore_current(iso);
  if let Some(m) = module_state(this) {
    m.dependencies = deps;
    m.status = ModuleStatus::Instantiated;
  }
  MaybeBool::JustTrue
}

unsafe fn resolve_dependency(
  context: *const Context,
  cb: ResolveModuleCallback,
  referrer: *const Module,
  spec: &str,
) -> *const Module {
  let ctx = ctx_of(context) as JSContextRef;
  if ctx.is_null() {
    return ptr::null();
  }
  let cspec = match std::ffi::CString::new(spec) {
    Ok(c) => c,
    Err(_) => return ptr::null(),
  };
  let spec_str = unsafe { JSStringCreateWithUTF8CString(cspec.as_ptr()) };
  let spec_val = unsafe { JSValueMakeString(ctx, spec_str) };
  unsafe { JSStringRelease(spec_str) };
  let spec_handle = intern_ctx::<V8String>(ctx, spec_val);

  let mut exc: JSValueRef = ptr::null();
  let attrs_arr = unsafe { JSObjectMakeArray(ctx, 0, ptr::null(), &mut exc) };
  let attrs_handle = intern_ctx::<FixedArray>(ctx, attrs_arr as JSValueRef);

  let ret = unsafe {
    let ctx_l = crate::Local::from_raw(context).unwrap();
    let spec_l = crate::Local::from_raw(spec_handle).unwrap();
    let attrs_l = crate::Local::from_raw(attrs_handle).unwrap();
    let ref_l = crate::Local::from_raw(referrer).unwrap();
    cb(ctx_l, spec_l, attrs_l, ref_l)
  };

  unsafe { *(&ret as *const ResolveModuleCallbackRet as *const *const Module) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__Evaluate(
  this: *const Module,
  context: *const Context,
) -> *const Value {
  let ctx = ctx_of(context) as JSContextRef;
  let Some(m) = module_state(this) else {
    return ptr::null();
  };

  if matches!(m.status, ModuleStatus::Evaluated) {
    return make_resolved_promise(ctx);
  }
  m.status = ModuleStatus::Evaluating;

  let iso = current_iso();
  let deps = m.dependencies.clone();
  let mut dep_promises: Vec<JSValueRef> = Vec::new();
  for dep in &deps {
    if dep.is_null() {
      continue;
    }
    let dep_async = v8__Module__IsGraphAsync(*dep);
    if let Some(dm) = module_state(*dep) {
      // Skip a dep already done, OR currently mid-evaluation up the call stack
      // (circular import: A imports B imports A). Re-entering an `Evaluating`
      // module recurses forever; its live bindings are already wired by
      // InstantiateModule, so the cycle resolves without a re-eval.
      if matches!(dm.status, ModuleStatus::Evaluating)
        || (matches!(dm.status, ModuleStatus::Evaluated) && !dep_async)
      {
        continue;
      }
    }
    let p = v8__Module__Evaluate(*dep, context);
    if dep_async && !p.is_null() {
      dep_promises.push(jsval(p));
    }
  }
  crate::jsc::core::restore_current(iso);

  let Some(m) = module_state(this) else {
    return ptr::null();
  };

  if !m.specifier.is_empty() {
    unsafe { register_module_namespace(ctx, &m.specifier, m.namespace) };
  }

  {
    let raw_specs = m.import_specifiers.clone();
    let deps2 = m.dependencies.clone();
    for (raw, dep) in raw_specs.iter().zip(deps2.iter()) {
      if dep.is_null() {
        continue;
      }
      if let Some(dm) = module_state(*dep) {
        if !raw.is_empty() && !dm.namespace.is_null() {
          unsafe { register_module_namespace(ctx, raw, dm.namespace) };
        }
      }
    }
  }

  if let Some(eval_steps) = m.eval_steps {
    let ret = unsafe {
      let ctx_l = crate::Local::from_raw(context as *const Context).unwrap();
      let mod_l = crate::Local::from_raw(this).unwrap();
      eval_steps(ctx_l, mod_l)
    };
    if let Some(m) = module_state(this) {
      m.status = ModuleStatus::Evaluated;
    }
    let promise = unsafe {
      *(&ret as *const SyntheticModuleEvaluationStepsRet as *const *const Value)
    };
    if !promise.is_null() {
      return promise;
    }
    return make_resolved_promise(ctx);
  }

  if let Some(src) = m.source.clone() {
    let is_async = m.is_async || !dep_promises.is_empty();
    let namespace = m.namespace;
    let meta = unsafe { build_import_meta(context, this) };
    match unsafe {
      eval_module_source(ctx, &src, namespace, meta, is_async, &dep_promises)
    } {
      Ok(promise) => {
        if let Some(m) = module_state(this) {
          m.status = ModuleStatus::Evaluated;
        }

        if !promise.is_null() {
          crate::jsc::exception::track_promise_pub(ctx, promise as JSObjectRef);
          return intern_ctx::<Value>(ctx, promise);
        }
      }
      Err(exc) => {
        if let Some(m) = module_state(this) {
          m.status = ModuleStatus::Errored;
        }
        return make_rejected_promise(ctx, exc);
      }
    }
  } else if let Some(m) = module_state(this) {
    m.status = ModuleStatus::Evaluated;
  }
  make_resolved_promise(ctx)
}

fn make_resolved_promise(ctx: JSContextRef) -> *const Value {
  if ctx.is_null() {
    return ptr::null();
  }
  let src = b"Promise.resolve(undefined)\0";
  let mut exc: JSValueRef = ptr::null();
  let js = unsafe {
    JSStringCreateWithUTF8CString(src.as_ptr() as *const std::os::raw::c_char)
  };
  let p = unsafe {
    JSEvaluateScript(ctx, js, ptr::null_mut(), ptr::null_mut(), 1, &mut exc)
  };
  unsafe { JSStringRelease(js) };
  if p.is_null() {
    let v = unsafe { JSValueMakeUndefined(ctx) };
    return intern_ctx::<Value>(ctx, v);
  }

  crate::jsc::exception::track_promise_pub(ctx, p as JSObjectRef);
  intern_ctx::<Value>(ctx, p)
}

unsafe fn eval_module_source(
  ctx: JSContextRef,
  rewritten: &str,
  namespace: JSObjectRef,
  meta: JSValueRef,
  is_async: bool,
  dep_promises: &[JSValueRef],
) -> Result<JSValueRef, JSValueRef> {
  let fail =
    |ctx: JSContextRef, exc: JSValueRef| -> Result<JSValueRef, JSValueRef> {
      unsafe { report_module_exception(ctx, exc) };

      let e = if exc.is_null() {
        make_generic_error(ctx, "module evaluation failed")
      } else {
        exc
      };
      Err(e)
    };
  let kw = if is_async {
    "async function"
  } else {
    "function"
  };

  let prologue = if is_async && !dep_promises.is_empty() {
    "await Promise.all(__deps);\n"
  } else {
    ""
  };
  let wrapped = format!(
    "({kw}(__ns, __v8jsc_meta, __deps){{\n{prologue}{}\n}})",
    rewritten
  );
  let cstr = match std::ffi::CString::new(wrapped) {
    Ok(c) => c,
    Err(_) => return fail(ctx, ptr::null()),
  };
  let mut exc: JSValueRef = ptr::null();
  let js = unsafe { JSStringCreateWithUTF8CString(cstr.as_ptr()) };
  let f = unsafe {
    JSEvaluateScript(ctx, js, ptr::null_mut(), ptr::null_mut(), 1, &mut exc)
  };
  unsafe { JSStringRelease(js) };
  if f.is_null() {
    return fail(ctx, exc);
  }
  let fobj = unsafe { JSValueToObject(ctx, f, &mut exc) };
  if fobj.is_null() {
    return fail(ctx, exc);
  }
  let meta = if meta.is_null() {
    unsafe { JSValueMakeUndefined(ctx) }
  } else {
    meta
  };
  let deps_arr = unsafe {
    JSObjectMakeArray(ctx, dep_promises.len(), dep_promises.as_ptr(), &mut exc)
  };
  let deps_val = if deps_arr.is_null() {
    unsafe { JSValueMakeUndefined(ctx) }
  } else {
    deps_arr as JSValueRef
  };
  let args = [namespace as JSValueRef, meta, deps_val];
  let r = unsafe {
    JSObjectCallAsFunction(
      ctx,
      fobj,
      ptr::null_mut(),
      3,
      args.as_ptr(),
      &mut exc,
    )
  };
  if is_async {
    if r.is_null() || !exc.is_null() {
      return fail(ctx, exc);
    }
    return Ok(r);
  }
  if r.is_null() || !exc.is_null() {
    return fail(ctx, exc);
  }
  Ok(ptr::null())
}

unsafe fn make_generic_error(ctx: JSContextRef, message: &str) -> JSValueRef {
  let mut exc: JSValueRef = ptr::null();
  let msg = std::ffi::CString::new(message).unwrap_or_default();
  let s = unsafe { JSStringCreateWithUTF8CString(msg.as_ptr()) };
  let arg = unsafe { JSValueMakeString(ctx, s) };
  unsafe { JSStringRelease(s) };
  let args = [arg];
  let e = unsafe { JSObjectMakeError(ctx, 1, args.as_ptr(), &mut exc) };
  if e.is_null() {
    unsafe { JSValueMakeUndefined(ctx) }
  } else {
    e as JSValueRef
  }
}

fn make_rejected_promise(ctx: JSContextRef, exc: JSValueRef) -> *const Value {
  if ctx.is_null() {
    return ptr::null();
  }
  let exc = if exc.is_null() {
    unsafe { JSValueMakeUndefined(ctx) }
  } else {
    exc
  };

  let mut e: JSValueRef = ptr::null();
  let p = unsafe {
    let global = JSContextGetGlobalObject(ctx);
    let pkey = JSStringCreateWithUTF8CString(c"Promise".as_ptr());
    let promise_ctor = JSObjectGetProperty(ctx, global, pkey, &mut e);
    JSStringRelease(pkey);
    if promise_ctor.is_null() || !JSValueIsObject(ctx, promise_ctor) {
      return intern_ctx::<Value>(ctx, JSValueMakeUndefined(ctx));
    }
    let rkey = JSStringCreateWithUTF8CString(c"reject".as_ptr());
    let reject =
      JSObjectGetProperty(ctx, promise_ctor as JSObjectRef, rkey, &mut e);
    JSStringRelease(rkey);
    if reject.is_null() || !JSValueIsObject(ctx, reject) {
      return intern_ctx::<Value>(ctx, JSValueMakeUndefined(ctx));
    }
    let args = [exc];
    JSObjectCallAsFunction(
      ctx,
      reject as JSObjectRef,
      promise_ctor as JSObjectRef,
      1,
      args.as_ptr(),
      &mut e,
    )
  };
  if p.is_null() {
    return intern_ctx::<Value>(ctx, unsafe { JSValueMakeUndefined(ctx) });
  }
  crate::jsc::exception::track_promise_pub(ctx, p as JSObjectRef);
  intern_ctx::<Value>(ctx, p)
}

/// Install live re-export getters on `namespace`: for each `export { local as
/// exported } from spec`, define `namespace[exported]` as a getter returning
/// `__v8jsc_modules[spec][local]` evaluated lazily. Lets a barrel's re-exports
/// resolve to the source module even before the barrel's own body runs.
unsafe fn install_reexport_getters(
  ctx: JSContextRef,
  namespace: JSObjectRef,
  reexports: &[(
    std::string::String,
    std::string::String,
    std::string::String,
  )],
) {
  if ctx.is_null() || namespace.is_null() || reexports.is_empty() {
    return;
  }
  let mut body = std::string::String::from("(function(__ns){\n");
  for (exported, spec, local) in reexports {
    let value = if local == "*" {
      format!("((globalThis.__v8jsc_modules||{{}})[{spec:?}]||{{}})")
    } else {
      format!("((globalThis.__v8jsc_modules||{{}})[{spec:?}]||{{}})[{local:?}]")
    };
    body.push_str(&format!(
      "Object.defineProperty(__ns, {exported:?}, {{configurable:true, enumerable:true, get:function(){{ return {value}; }}}});\n"
    ));
  }
  body.push_str("})");
  let Ok(cstr) = std::ffi::CString::new(body) else {
    return;
  };
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let js = JSStringCreateWithUTF8CString(cstr.as_ptr());
    let f =
      JSEvaluateScript(ctx, js, ptr::null_mut(), ptr::null_mut(), 1, &mut exc);
    JSStringRelease(js);
    if f.is_null() || !JSValueIsObject(ctx, f) {
      return;
    }
    let args = [namespace as JSValueRef];
    JSObjectCallAsFunction(
      ctx,
      f as JSObjectRef,
      ptr::null_mut(),
      1,
      args.as_ptr(),
      &mut exc,
    );
  }
}

unsafe fn register_module_namespace(
  ctx: JSContextRef,
  specifier: &str,
  namespace: JSObjectRef,
) {
  if ctx.is_null() || namespace.is_null() || specifier.is_empty() {
    return;
  }
  unsafe {
    let global = JSContextGetGlobalObject(ctx);
    let mut exc: JSValueRef = ptr::null();

    let reg_key = JSStringCreateWithUTF8CString(c"__v8jsc_modules".as_ptr());
    let mut reg = JSObjectGetProperty(ctx, global, reg_key, &mut exc);
    let reg_obj = if reg.is_null() || JSValueIsUndefined(ctx, reg) {
      let o = JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut());
      JSObjectSetProperty(
        ctx,
        global,
        reg_key,
        o as JSValueRef,
        1 << 1,
        &mut exc,
      );
      o
    } else {
      JSValueToObject(ctx, reg, &mut exc)
    };
    JSStringRelease(reg_key);
    if reg_obj.is_null() {
      return;
    }
    if let Ok(cspec) = std::ffi::CString::new(specifier) {
      let spec_key = JSStringCreateWithUTF8CString(cspec.as_ptr());
      JSObjectSetProperty(
        ctx,
        reg_obj,
        spec_key,
        namespace as JSValueRef,
        0,
        &mut exc,
      );
      JSStringRelease(spec_key);
    }
  }
}

unsafe fn build_import_meta(
  context: *const Context,
  module: *const Module,
) -> JSValueRef {
  let ctx = ctx_of(context) as JSContextRef;
  if ctx.is_null() {
    return ptr::null();
  }
  let meta = unsafe { JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut()) };
  let iso = current_iso();
  if iso.is_null() {
    return meta as JSValueRef;
  }
  let cb = iso_state(iso).import_meta_cb;
  if let Some(cb) = cb {
    unsafe {
      let ctx_l = crate::Local::from_raw(context).unwrap();
      let mod_l = crate::Local::from_raw(module).unwrap();
      let meta_l =
        crate::Local::<Object>::from_raw(meta as *const Object).unwrap();
      cb(ctx_l, mod_l, meta_l);
    }
  }
  meta as JSValueRef
}

unsafe fn report_module_exception(ctx: JSContextRef, exc: JSValueRef) {
  if exc.is_null() {
    return;
  }
  crate::jsc::core::record_pending_exception(ctx, exc);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__IsGraphAsync(this: *const Module) -> bool {
  fn graph_async(this: *const Module, seen: &mut Vec<*const Module>) -> bool {
    if this.is_null() || seen.contains(&this) {
      return false;
    }
    seen.push(this);
    match module_state(this) {
      Some(m) => {
        if m.is_async {
          return true;
        }
        let deps = m.dependencies.clone();
        deps.iter().any(|d| graph_async(*d, seen))
      }
      None => false,
    }
  }
  let mut seen = Vec::new();
  graph_async(this, &mut seen)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__IsSyntheticModule(this: *const Module) -> bool {
  module_state(this).is_some()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__CreateSyntheticModule(
  isolate: *const RealIsolate,
  module_name: *const V8String,
  export_names_len: usize,
  export_names_raw: *const *const V8String,
  evaluation_steps: SyntheticModuleEvaluationSteps,
) -> *const Module {
  let st = iso_state(isolate as *mut RealIsolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  if ctx.is_null() {
    return ptr::null();
  }
  let gctx = unsafe { JSContextGetGlobalContext(ctx) };
  let specifier = unsafe { jsstring_to_rust(ctx, jsval(module_name)) };

  let mut names: Vec<std::string::String> =
    Vec::with_capacity(export_names_len);
  if !export_names_raw.is_null() {
    for i in 0..export_names_len {
      let nptr = unsafe { *export_names_raw.add(i) };
      let nv = jsval(nptr);
      if nv.is_null() {
        continue;
      }
      let mut exc: JSValueRef = ptr::null();
      let s = unsafe { JSValueToStringCopy(ctx, nv, &mut exc) };
      if s.is_null() {
        continue;
      }
      let max = unsafe { JSStringGetMaximumUTF8CStringSize(s) };
      let mut buf = vec![0u8; max];
      let n =
        unsafe { JSStringGetUTF8CString(s, buf.as_mut_ptr() as *mut _, max) };
      unsafe { JSStringRelease(s) };
      if n > 0 {
        buf.truncate(n - 1);
        if let Ok(name) = std::string::String::from_utf8(buf) {
          names.push(name);
        }
      }
    }
  }

  let namespace =
    unsafe { JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut()) };
  unsafe { JSValueProtect(gctx, namespace as JSValueRef) };

  let steps: SyntheticModuleEvaluationSteps<'static> =
    unsafe { std::mem::transmute(evaluation_steps) };

  let state = Box::new(SyntheticModule {
    ctx: gctx,
    status: ModuleStatus::Uninstantiated,
    export_names: names,
    eval_steps: Some(steps),
    source: None,
    import_specifiers: Vec::new(),
    namespace,
    specifier,
    dependencies: Vec::new(),
    is_async: false,
  });
  let obj =
    unsafe { JSObjectMake(ctx, mod_class(), Box::into_raw(state) as *mut _) };
  intern_ctx::<Module>(ctx, obj as JSValueRef)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__SetSyntheticModuleExport(
  this: *const Module,
  _isolate: *const RealIsolate,
  export_name: *const V8String,
  export_value: *const Value,
) -> MaybeBool {
  let Some(m) = module_state(this) else {
    return MaybeBool::JustFalse;
  };
  let ctx = m.ctx as JSContextRef;
  let name_v = jsval(export_name);
  let val = jsval(export_value);
  if ctx.is_null() || name_v.is_null() || m.namespace.is_null() {
    return MaybeBool::JustFalse;
  }
  let mut exc: JSValueRef = ptr::null();
  let key = unsafe { JSValueToStringCopy(ctx, name_v, &mut exc) };
  if key.is_null() {
    return MaybeBool::JustFalse;
  }
  unsafe {
    JSObjectSetProperty(ctx, m.namespace, key, val, 0, &mut exc);
    JSStringRelease(key);
  }
  MaybeBool::JustTrue
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetUnboundModuleScript(
  this: *const Module,
) -> *const UnboundModuleScript {
  if this.is_null() {
    return ptr::null();
  }
  intern::<UnboundModuleScript>(jsval(this))
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
  unsafe {
    let obj = jsval(this) as JSObjectRef;
    let key = JSStringCreateWithUTF8CString(c"specifier".as_ptr());
    let mut exc: JSValueRef = ptr::null();
    let v = JSObjectGetProperty(ctx, obj, key, &mut exc);
    JSStringRelease(key);
    if v.is_null() || JSValueIsUndefined(ctx, v) {
      return ptr::null();
    }
    intern_ctx::<V8String>(ctx, v)
  }
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
) -> int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ModuleRequest__GetImportAttributes(
  _this: *const ModuleRequest,
) -> *const FixedArray {
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }
  let mut exc: JSValueRef = ptr::null();
  let arr = unsafe { JSObjectMakeArray(ctx, 0, ptr::null(), &mut exc) };
  if arr.is_null() {
    return ptr::null();
  }
  intern_ctx::<FixedArray>(ctx, arr as JSValueRef)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CompileUnboundScript(
  isolate: *mut RealIsolate,
  source: *mut Source,
  _options: CompileOptions,
  _no_cache_reason: NoCacheReason,
) -> *const UnboundScript {
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  let src_val = unsafe { source_string_of(source) };
  if ctx.is_null() || src_val.is_null() {
    return ptr::null();
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let src_str = JSValueToStringCopy(ctx, src_val, &mut exc);
    if src_str.is_null() {
      return ptr::null();
    }
    let ok = JSCheckScriptSyntax(ctx, src_str, ptr::null_mut(), 1, &mut exc);
    JSStringRelease(src_str);
    if !ok {
      return ptr::null();
    }
  }
  intern_ctx::<UnboundScript>(ctx, src_val)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CachedDataVersionTag() -> u32 {
  0x5643_4a53
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundScript__BindToCurrentContext(
  script: *const UnboundScript,
) -> *const Script {
  if script.is_null() {
    return ptr::null();
  }
  intern::<Script>(jsval(script))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundScript__GetSourceMappingURL(
  _script: *const UnboundScript,
) -> *const Value {
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }
  let v = unsafe { JSValueMakeUndefined(ctx) };
  intern_ctx::<Value>(ctx, v)
}
