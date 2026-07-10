//! Snapshot support utilities for the QuickJS backend.
//!
//! The snapshot FORMAT lives in `capi_tape.rs` (C-API record/replay tape,
//! magic `V8XTAPE1`): a `SnapshotCreator` records the embedder's C-ABI calls
//! and `CreateBlob` serializes them; restoring replays the calls against a
//! fresh runtime. This module keeps only the shared value/blob plumbing:
//!
//! - `serialize_value` / `deserialize_value`: serialize one trusted JS value
//!   to/from bytes (`JS_WriteObject`/`JS_ReadObject`) — used for tape
//!   `ClonedValue` entries and embedder-data snapshots.
//! - `leak_blob` / `free_blob`: hand a serialized blob across the C ABI with
//!   `StartupData`-compatible ownership.

use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use super::quickjs_sys::*;
use crate::{Data, FunctionCallback, FunctionTemplate, RealIsolate};

const MAGIC: &[u8; 8] = b"V8XSNP2\0";
const DATA_MAGIC: &[u8; 8] = b"V8XSDAT\0";
const NO_REF_INDEX: u32 = u32::MAX;
const GLOBAL_VALUE: u8 = 0;
const GLOBAL_FUNCTION: u8 = 1;
const GLOBAL_LEXICAL_VALUE: u8 = 2;
const SNAPSHOT_DATA_MODULE: u8 = 1;
const SNAPSHOT_DATA_OBJECT: u8 = 2;
const SNAPSHOT_DATA_HOST_OBJECT: u8 = 3;
const SNAPSHOT_DATA_HOST_FUNCTION: u8 = 4;
const SNAPSHOT_DATA_JS_FUNCTION: u8 = 5;
const SNAPSHOT_DATA_NOOP_FUNCTION: u8 = 7;
const SNAPSHOT_DATA_JS_FUNCTION_SOURCE: u8 = 8;
const SNAPSHOT_DATA_FUNCTION_TEMPLATE: u8 = 9;
const JS_WRITE_OBJ_BYTECODE: c_int = 1 << 0;
const JS_WRITE_OBJ_SAB: c_int = 1 << 2;
const JS_WRITE_OBJ_REFERENCE: c_int = 1 << 3;
const JS_READ_OBJ_BYTECODE: c_int = 1 << 0;
const JS_READ_OBJ_SAB: c_int = 1 << 2;
const JS_READ_OBJ_REFERENCE: c_int = 1 << 3;
const SNAPSHOT_READ_FLAGS: c_int =
  JS_READ_OBJ_BYTECODE | JS_READ_OBJ_SAB | JS_READ_OBJ_REFERENCE;
const JS_GPN_STRING_MASK: c_int = 1 << 0;
const JS_GPN_ENUM_ONLY: c_int = 1 << 4;
const JS_GPN_SET_ENUM: c_int = 1 << 5;
const JS_PROP_CONFIGURABLE: c_int = 1 << 0;
const JS_PROP_WRITABLE: c_int = 1 << 1;
const JS_PROP_ENUMERABLE: c_int = 1 << 2;
const JS_PROP_GETSET: c_int = 1 << 4;
const NOOP_FUNCTION_SRC: &str = "(function(){})\0";

#[repr(C)]
struct JSPropertyEnum {
  is_enumerable: bool,
  atom: JSAtom,
}

#[repr(C)]
struct JSPropertyDescriptor {
  flags: c_int,
  value: JSValue,
  getter: JSValue,
  setter: JSValue,
}

unsafe extern "C" {
  fn JS_GetOwnPropertyNames(
    ctx: *mut JSContext,
    ptab: *mut *mut JSPropertyEnum,
    plen: *mut u32,
    obj: JSValue,
    flags: c_int,
  ) -> c_int;
  fn JS_FreePropertyEnum(
    ctx: *mut JSContext,
    tab: *mut JSPropertyEnum,
    len: u32,
  );
  fn JS_AtomToCStringLen(
    ctx: *mut JSContext,
    plen: *mut usize,
    atom: JSAtom,
  ) -> *const c_char;
  fn JS_GetProperty(
    ctx: *mut JSContext,
    this_obj: JSValue,
    prop: JSAtom,
  ) -> JSValue;
  fn JS_GetOwnProperty(
    ctx: *mut JSContext,
    desc: *mut JSPropertyDescriptor,
    this_obj: JSValue,
    prop: JSAtom,
  ) -> c_int;
  fn JS_DefinePropertyValueStr(
    ctx: *mut JSContext,
    this_obj: JSValue,
    prop: *const c_char,
    val: JSValue,
    flags: c_int,
  ) -> c_int;
}

#[derive(Clone)]
pub(crate) struct SnapshotBlob {
  pub default_context: Option<ContextSnapshot>,
  pub contexts: Vec<ContextSnapshot>,
  pub isolate_data: Vec<Vec<u8>>,
}

#[derive(Clone)]
pub(crate) struct ContextSnapshot {
  pub global_object: Option<Vec<u8>>,
  pub globals: Vec<GlobalEntry>,
  pub lexical_globals: Vec<GlobalEntry>,
  pub embedder_data: Vec<Option<Vec<u8>>>,
  pub context_data: Vec<Vec<u8>>,
}

#[derive(Clone)]
pub(crate) enum GlobalEntry {
  Value {
    name: String,
    bytes: Vec<u8>,
    enumerable: bool,
  },
  LexicalValue {
    name: String,
    bytes: Vec<u8>,
  },
  Function {
    name: String,
    callback: ExternalRefSlot,
    data: Option<ExternalRefSlot>,
    length: i32,
    constructable: bool,
    enumerable: bool,
  },
}

#[derive(Clone, Copy)]
pub(crate) struct ExternalRefSlot {
  index: u32,
  raw: usize,
}

impl ExternalRefSlot {
  fn new(raw: usize, external_refs: &[usize]) -> Self {
    let index = external_refs
      .iter()
      .position(|&value| value == raw)
      .map(|index| index.min(NO_REF_INDEX as usize - 1) as u32)
      .unwrap_or(NO_REF_INDEX);
    Self { index, raw }
  }

  fn resolve(self, external_refs: &[usize]) -> usize {
    if self.index != NO_REF_INDEX {
      if let Some(&value) = external_refs.get(self.index as usize) {
        if value != 0 {
          return value;
        }
      }
    }
    self.raw
  }
}

thread_local! {
  static SNAPSHOT_EXTERNAL_REFERENCES: std::cell::RefCell<Vec<usize>> =
    const { std::cell::RefCell::new(Vec::new()) };
}

fn with_snapshot_external_references<T>(
  external_refs: &[usize],
  f: impl FnOnce() -> T,
) -> T {
  struct Guard(Vec<usize>);

  impl Drop for Guard {
    fn drop(&mut self) {
      SNAPSHOT_EXTERNAL_REFERENCES.with(|refs| {
        refs.replace(std::mem::take(&mut self.0));
      });
    }
  }

  let previous = SNAPSHOT_EXTERNAL_REFERENCES
    .with(|refs| refs.replace(external_refs.to_vec()));
  let _guard = Guard(previous);
  f()
}

fn current_snapshot_external_references() -> Vec<usize> {
  SNAPSHOT_EXTERNAL_REFERENCES.with(|refs| refs.borrow().clone())
}

/// Structured-clone one JS value to bytes (no bytecode: plain data graph).
#[cfg(test)]
fn serialize_value(ctx: *mut JSContext, v: JSValue) -> Option<Vec<u8>> {
  serialize_value_with_refs(ctx, v, &[])
}

pub(crate) fn serialize_value_with_refs(
  ctx: *mut JSContext,
  v: JSValue,
  external_refs: &[usize],
) -> Option<Vec<u8>> {
  serialize_value_with_refs_inner(ctx, v, external_refs, false)
}

fn serialize_global_value_with_refs(
  ctx: *mut JSContext,
  v: JSValue,
  external_refs: &[usize],
) -> Option<Vec<u8>> {
  serialize_value_with_refs_inner(ctx, v, external_refs, true)
}

fn serialize_value_with_refs_inner(
  ctx: *mut JSContext,
  v: JSValue,
  external_refs: &[usize],
  allow_function_source: bool,
) -> Option<Vec<u8>> {
  if let Some(info) = super::module::module_snapshot_info_for_value(v) {
    return Some(encode_module_data(
      &info.name,
      info.source.as_deref(),
      info.evaluated,
    ));
  }
  if let Some(info) = super::function::snapshot_function_info(v) {
    return Some(encode_function_data(info, external_refs));
  }
  if !ctx.is_null() && unsafe { JS_IsFunction(ctx, v) } {
    if let Some(bytes) = write_object_bytes(ctx, v, external_refs) {
      return Some(encode_js_function_data(&bytes));
    }
    if allow_function_source
      && let Some(source) = function_source(ctx, v)
      && is_eval_safe_function_source(&source)
    {
      return Some(encode_js_function_source_data(&source));
    }
    return Some(encode_noop_function_data());
  }
  let Some(out) = write_object_bytes(ctx, v, external_refs) else {
    if v.tag == JS_TAG_OBJECT {
      return serialize_host_object(ctx, v, external_refs);
    }
    return None;
  };
  Some(out)
}

fn write_object_bytes(
  ctx: *mut JSContext,
  v: JSValue,
  external_refs: &[usize],
) -> Option<Vec<u8>> {
  let mut size: usize = 0;
  let buf = with_snapshot_external_references(external_refs, || {
    super::exception::with_prepare_stack_suppressed(|| unsafe {
      JS_WriteObject(
        ctx,
        &mut size,
        v,
        JS_WRITE_OBJ_BYTECODE | JS_WRITE_OBJ_SAB | JS_WRITE_OBJ_REFERENCE,
      )
    })
  });
  if buf.is_null() {
    // Clear the pending exception JS_WriteObject leaves behind.
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return None;
  }
  let out = unsafe { std::slice::from_raw_parts(buf, size) }.to_vec();
  unsafe { js_free(ctx, buf as *mut std::os::raw::c_void) };
  Some(out)
}

fn function_source(ctx: *mut JSContext, v: JSValue) -> Option<String> {
  let mut len: usize = 0;
  let cstr = unsafe { JS_ToCStringLen(ctx, &mut len, v) };
  if cstr.is_null() {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return None;
  }
  let bytes = unsafe { std::slice::from_raw_parts(cstr as *const u8, len) };
  let out = String::from_utf8(bytes.to_vec()).ok();
  unsafe { JS_FreeCString(ctx, cstr) };
  out
}

fn is_eval_safe_function_source(source: &str) -> bool {
  let source = source.trim_start();
  !source.contains("[native code]")
    && (source.starts_with("function")
      || source.starts_with("async function")
      || source.starts_with("class")
      || source.contains("=>"))
}

pub(crate) fn deserialize_value_with_refs(
  ctx: *mut JSContext,
  bytes: &[u8],
  external_refs: &[usize],
) -> Option<JSValue> {
  if ctx.is_null() || bytes.is_empty() {
    return None;
  }
  if bytes.starts_with(DATA_MAGIC) {
    return decode_special_data(ctx, bytes, external_refs);
  }
  let value = with_snapshot_external_references(external_refs, || {
    super::exception::with_prepare_stack_suppressed(|| unsafe {
      JS_ReadObject(ctx, bytes.as_ptr(), bytes.len(), SNAPSHOT_READ_FLAGS)
    })
  });
  if value.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return None;
  }
  if value.tag == JS_TAG_FUNCTION_BYTECODE {
    let value = unsafe { JS_EvalFunction(ctx, value) };
    if value.tag == JS_TAG_EXCEPTION {
      let exc = unsafe { JS_GetException(ctx) };
      unsafe { JS_FreeValue(ctx, exc) };
      return None;
    }
    return Some(value);
  }
  Some(value)
}

fn serialize_host_object(
  ctx: *mut JSContext,
  v: JSValue,
  external_refs: &[usize],
) -> Option<Vec<u8>> {
  let mut seen = HashSet::new();
  super::exception::with_prepare_stack_suppressed(|| {
    serialize_host_object_inner(ctx, v, external_refs, &mut seen, 0)
  })
}

fn serialize_host_object_inner(
  ctx: *mut JSContext,
  v: JSValue,
  external_refs: &[usize],
  seen: &mut HashSet<usize>,
  depth: usize,
) -> Option<Vec<u8>> {
  if ctx.is_null() {
    return None;
  }
  if super::module::module_name_for_value(v).is_some()
    || super::function::snapshot_function_info(v).is_some()
    || unsafe { JS_IsFunction(ctx, v) }
    || v.tag >= 0
  {
    return serialize_value_with_refs(ctx, v, external_refs);
  }
  if depth > 8 {
    return Some(encode_object_data());
  }
  let key = unsafe { v.u.ptr as usize };
  if key == 0 || !seen.insert(key) {
    return Some(encode_object_data());
  }

  let mut props_out = Vec::new();
  unsafe {
    let mut tab: *mut JSPropertyEnum = ptr::null_mut();
    let mut len = 0u32;
    let rc =
      JS_GetOwnPropertyNames(ctx, &mut tab, &mut len, v, JS_GPN_STRING_MASK);
    if rc < 0 {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      seen.remove(&key);
      return Some(encode_object_data());
    }
    let props = std::slice::from_raw_parts(tab, len as usize);
    for prop in props {
      let Some(name) = atom_to_string(ctx, prop.atom) else {
        continue;
      };
      let mut desc = JSPropertyDescriptor {
        flags: 0,
        value: jsv_undefined(),
        getter: jsv_undefined(),
        setter: jsv_undefined(),
      };
      let rc = JS_GetOwnProperty(ctx, &mut desc, v, prop.atom);
      if rc <= 0 {
        if rc < 0 {
          let exc = JS_GetException(ctx);
          JS_FreeValue(ctx, exc);
        }
        continue;
      }
      if desc.flags & JS_PROP_GETSET != 0 {
        JS_FreeValue(ctx, desc.value);
        JS_FreeValue(ctx, desc.getter);
        JS_FreeValue(ctx, desc.setter);
        continue;
      }
      JS_FreeValue(ctx, desc.getter);
      JS_FreeValue(ctx, desc.setter);
      let value = desc.value;
      if value.tag == JS_TAG_EXCEPTION {
        let exc = JS_GetException(ctx);
        JS_FreeValue(ctx, exc);
        continue;
      }
      if let Some(bytes) =
        serialize_host_object_inner(ctx, value, external_refs, seen, depth + 1)
      {
        props_out.push((name, bytes));
      }
      JS_FreeValue(ctx, value);
    }
    JS_FreePropertyEnum(ctx, tab, len);
  }

  seen.remove(&key);
  Some(encode_host_object_data(&props_out))
}

fn encode_module_data(
  name: &str,
  source: Option<&str>,
  evaluated: bool,
) -> Vec<u8> {
  let mut out = Vec::with_capacity(
    DATA_MAGIC.len() + 2 + 8 + name.len() + source.map_or(0, str::len),
  );
  out.extend_from_slice(DATA_MAGIC);
  out.push(SNAPSHOT_DATA_MODULE);
  put_str(&mut out, name);
  if let Some(source) = source {
    put_str(&mut out, source);
  }
  out.push(u8::from(evaluated));
  out
}

fn encode_object_data() -> Vec<u8> {
  let mut out = Vec::with_capacity(DATA_MAGIC.len() + 1);
  out.extend_from_slice(DATA_MAGIC);
  out.push(SNAPSHOT_DATA_OBJECT);
  out
}

fn encode_function_data(
  info: super::function::SnapshotFunctionInfo,
  external_refs: &[usize],
) -> Vec<u8> {
  let mut out = Vec::new();
  out.extend_from_slice(DATA_MAGIC);
  out.push(SNAPSHOT_DATA_HOST_FUNCTION);
  put_ref_slot(
    &mut out,
    ExternalRefSlot::new(info.callback as usize, external_refs),
  );
  put_opt_ref_slot(
    &mut out,
    info
      .data_external
      .map(|ptr| ExternalRefSlot::new(ptr as usize, external_refs)),
  );
  put_i32(&mut out, info.length);
  out.push(u8::from(info.constructable));
  out
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn v82jsc_snapshot_write_host_object(
  _ctx: *mut JSContext,
  value: JSValue,
  output: *mut u8,
  size: *mut usize,
) -> c_int {
  if size.is_null() {
    return -1;
  }
  let Some(info) = super::function::snapshot_function_info(value) else {
    return 0;
  };
  let external_refs = current_snapshot_external_references();
  let bytes = encode_function_data(info, &external_refs);
  if output.is_null() {
    unsafe { *size = bytes.len() };
    return 1;
  }
  if unsafe { *size } < bytes.len() {
    unsafe { *size = bytes.len() };
    return -1;
  }
  unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), output, bytes.len()) };
  unsafe { *size = bytes.len() };
  1
}

#[unsafe(no_mangle)]
pub extern "C" fn v82jsc_snapshot_host_object_has_prototype(
  value: JSValue,
) -> bool {
  super::function::snapshot_function_info(value)
    .is_some_and(|info| info.constructable)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn v82jsc_snapshot_read_host_object(
  ctx: *mut JSContext,
  input: *const u8,
  size: usize,
  output: *mut JSValue,
) -> c_int {
  if ctx.is_null() || input.is_null() || output.is_null() {
    return -1;
  }
  let bytes = unsafe { std::slice::from_raw_parts(input, size) };
  let external_refs = current_snapshot_external_references();
  let Some(value) = decode_special_data(ctx, bytes, &external_refs) else {
    return 0;
  };
  unsafe { *output = value };
  1
}

fn encode_js_function_data(bytes: &[u8]) -> Vec<u8> {
  let mut out = Vec::new();
  out.extend_from_slice(DATA_MAGIC);
  out.push(SNAPSHOT_DATA_JS_FUNCTION);
  put_bytes(&mut out, bytes);
  out
}

fn encode_js_function_source_data(source: &str) -> Vec<u8> {
  let mut out = Vec::new();
  out.extend_from_slice(DATA_MAGIC);
  out.push(SNAPSHOT_DATA_JS_FUNCTION_SOURCE);
  put_str(&mut out, source);
  out
}

fn encode_noop_function_data() -> Vec<u8> {
  let mut out = Vec::new();
  out.extend_from_slice(DATA_MAGIC);
  out.push(SNAPSHOT_DATA_NOOP_FUNCTION);
  out
}

fn encode_host_object_data(props: &[(String, Vec<u8>)]) -> Vec<u8> {
  let mut out = Vec::new();
  out.extend_from_slice(DATA_MAGIC);
  out.push(SNAPSHOT_DATA_HOST_OBJECT);
  put_vec(&mut out, props, |out, (name, bytes)| {
    put_str(out, name);
    put_bytes(out, bytes);
  });
  out
}

pub(crate) fn serialize_function_template(
  ctx: *mut JSContext,
  template: *const FunctionTemplate,
  external_refs: &[usize],
) -> Option<Vec<u8>> {
  let info = super::function::snapshot_function_template_info(template)?;
  let cached_proto = info
    .cached_proto
    .and_then(|proto| serialize_host_object(ctx, proto, external_refs));
  let mut out = Vec::new();
  out.extend_from_slice(DATA_MAGIC);
  out.push(SNAPSHOT_DATA_FUNCTION_TEMPLATE);
  put_ref_slot(
    &mut out,
    ExternalRefSlot::new(info.callback as usize, external_refs),
  );
  put_opt_ref_slot(
    &mut out,
    info
      .data_external
      .map(|ptr| ExternalRefSlot::new(ptr as usize, external_refs)),
  );
  put_i32(&mut out, info.length);
  out.push(u8::from(info.constructable));
  match info.class_name {
    Some(name) => {
      out.push(1);
      put_str(&mut out, &name);
    }
    None => out.push(0),
  }
  put_opt_bytes(&mut out, &cached_proto);
  put_i32(&mut out, info.instance_internal_field_count);
  Some(out)
}

pub(crate) fn deserialize_function_template(
  ctx: *mut JSContext,
  bytes: &[u8],
  external_refs: &[usize],
) -> Option<*const Data> {
  if !bytes.starts_with(DATA_MAGIC) {
    return None;
  }
  let mut input = Reader {
    bytes,
    pos: DATA_MAGIC.len(),
  };
  if input.get_u8()? != SNAPSHOT_DATA_FUNCTION_TEMPLATE {
    return None;
  }
  let callback = input.get_ref_slot()?.resolve(external_refs);
  if callback == 0 {
    return None;
  }
  let callback =
    unsafe { std::mem::transmute::<usize, FunctionCallback>(callback) };
  let data_external = input
    .get_opt_ref_slot()?
    .map(|slot| slot.resolve(external_refs) as *mut c_void)
    .filter(|ptr| !ptr.is_null());
  let length = input.get_i32()?;
  let constructable = input.get_u8()? != 0;
  let class_name = match input.get_u8()? {
    0 => None,
    1 => Some(input.get_string()?),
    _ => return None,
  };
  let cached_proto = input
    .get_opt_bytes()?
    .and_then(|bytes| deserialize_value_with_refs(ctx, &bytes, external_refs));
  let instance_internal_field_count = input.get_i32()?;
  Some(super::function::restore_function_template_from_snapshot(
    callback,
    data_external,
    length,
    constructable,
    class_name,
    cached_proto,
    instance_internal_field_count,
  ) as *const Data)
}

fn decode_special_data(
  ctx: *mut JSContext,
  bytes: &[u8],
  external_refs: &[usize],
) -> Option<JSValue> {
  if !bytes.starts_with(DATA_MAGIC) {
    return None;
  }
  let mut input = Reader {
    bytes,
    pos: DATA_MAGIC.len(),
  };
  match input.get_u8()? {
    SNAPSHOT_DATA_MODULE => {
      let name = input.get_string()?;
      if let Some(source) = input.get_string()
        && !source.is_empty()
      {
        super::module::register_module_source(&name, &source);
      }
      let evaluated = input.get_u8().unwrap_or(0) != 0;
      let module = super::module::tape_make_module_handle(ctx, &name);
      if module.is_null() {
        return None;
      }
      super::module::refresh_tape_module_state(ctx, module);
      if evaluated {
        super::module::mark_tape_module_evaluated(module);
      }
      Some(unsafe { JS_DupValue(ctx, super::core::jsval_of(module)) })
    }
    SNAPSHOT_DATA_HOST_FUNCTION => {
      let callback_slot = input.get_ref_slot()?;
      let callback = callback_slot.resolve(external_refs);
      if callback == 0 {
        return None;
      }
      let callback =
        unsafe { std::mem::transmute::<usize, FunctionCallback>(callback) };
      let data = input
        .get_opt_ref_slot()?
        .map(|slot| slot.resolve(external_refs))
        .filter(|raw| *raw != 0)
        .map(|raw| {
          super::function::make_external_jsvalue(
            super::core::current_iso(),
            ctx,
            raw as *mut c_void,
          )
        })
        .unwrap_or_else(jsv_undefined);
      let length = input.get_i32()?;
      let constructable = input.get_u8()? != 0;
      let function = unsafe {
        super::function::make_function_len(
          ctx,
          callback,
          data,
          length,
          constructable,
        )
      };
      if !jsv_is_undefined(&data) {
        unsafe { JS_FreeValue(ctx, data) };
      }
      Some(function)
    }
    SNAPSHOT_DATA_JS_FUNCTION => {
      let bytes = input.get_bytes()?;
      let mut value = with_snapshot_external_references(external_refs, || {
        super::exception::with_prepare_stack_suppressed(|| unsafe {
          JS_ReadObject(ctx, bytes.as_ptr(), bytes.len(), SNAPSHOT_READ_FLAGS)
        })
      });
      if value.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return None;
      }
      if value.tag == JS_TAG_FUNCTION_BYTECODE {
        value = unsafe { JS_EvalFunction(ctx, value) };
        if value.tag == JS_TAG_EXCEPTION {
          let exc = unsafe { JS_GetException(ctx) };
          unsafe { JS_FreeValue(ctx, exc) };
          return None;
        }
      }
      Some(value)
    }
    SNAPSHOT_DATA_JS_FUNCTION_SOURCE => {
      let source = input.get_string()?;
      let wrapped = format!("({source})");
      let Ok(csrc) = CString::new(wrapped) else {
        return None;
      };
      let value = super::exception::with_prepare_stack_suppressed(|| unsafe {
        JS_Eval(
          ctx,
          csrc.as_ptr(),
          csrc.as_bytes().len(),
          c"<v8x-snapshot-function>".as_ptr(),
          JS_EVAL_TYPE_GLOBAL,
        )
      });
      if value.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        if std::env::var_os("QJS_DEBUG_SNAPSHOT").is_some() {
          let mut len = 0usize;
          let cstr = unsafe { JS_ToCStringLen(ctx, &mut len, exc) };
          let message = if cstr.is_null() {
            "<unstringifiable>".into()
          } else {
            let bytes =
              unsafe { std::slice::from_raw_parts(cstr as *const u8, len) };
            let message = String::from_utf8_lossy(bytes).into_owned();
            unsafe { JS_FreeCString(ctx, cstr) };
            message
          };
          eprintln!(
            "[qjs snapshot] function source replay failed: {message}; source={:?}",
            source.chars().take(240).collect::<String>()
          );
        }
        unsafe { JS_FreeValue(ctx, exc) };
        return None;
      }
      Some(value)
    }
    SNAPSHOT_DATA_NOOP_FUNCTION => {
      let value = unsafe {
        JS_Eval(
          ctx,
          NOOP_FUNCTION_SRC.as_ptr() as *const c_char,
          NOOP_FUNCTION_SRC.len() - 1,
          c"<v8x-noop-function>".as_ptr(),
          JS_EVAL_TYPE_GLOBAL,
        )
      };
      if value.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return None;
      }
      Some(value)
    }
    SNAPSHOT_DATA_OBJECT => {
      let value = unsafe { JS_NewObject(ctx) };
      if value.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return None;
      }
      Some(value)
    }
    SNAPSHOT_DATA_HOST_OBJECT => {
      let value = unsafe { JS_NewObject(ctx) };
      if value.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return None;
      }
      let len = input.get_u32()? as usize;
      for _ in 0..len {
        let name = input.get_string()?;
        let bytes = input.get_bytes()?;
        let Ok(name) = CString::new(name) else {
          continue;
        };
        let Some(prop_value) =
          deserialize_value_with_refs(ctx, &bytes, external_refs)
        else {
          continue;
        };
        unsafe { JS_SetPropertyStr(ctx, value, name.as_ptr(), prop_value) };
      }
      Some(value)
    }
    _ => None,
  }
}

pub(crate) fn external_references_from_params(
  raw_params: *const crate::isolate_create_params::raw::CreateParams,
) -> Vec<usize> {
  if raw_params.is_null() {
    return Vec::new();
  }
  let refs = unsafe { (*raw_params).external_references };
  if refs.is_null() {
    return Vec::new();
  }
  let mut out = Vec::new();
  let mut offset = 0usize;
  loop {
    let value = unsafe { *(refs.add(offset)) as usize };
    out.push(value);
    offset += 1;
    if value == 0 {
      break;
    }
  }
  out
}

pub(crate) fn blob_from_params(
  raw_params: *const crate::isolate_create_params::raw::CreateParams,
) -> Option<SnapshotBlob> {
  if raw_params.is_null() {
    return None;
  }
  let startup = unsafe { (*raw_params).snapshot_blob };
  if startup.is_null() {
    return None;
  }
  let raw_size = unsafe { (*startup).raw_size };
  let data = unsafe { (*startup).data };
  if data.is_null() || raw_size <= 0 {
    return None;
  }
  let bytes =
    unsafe { std::slice::from_raw_parts(data as *const u8, raw_size as usize) };
  decode_blob(bytes)
}

pub(crate) fn encode_blob(blob: &SnapshotBlob) -> Box<[u8]> {
  let mut out = Vec::new();
  out.extend_from_slice(MAGIC);
  put_opt_context(&mut out, blob.default_context.as_ref());
  put_vec(&mut out, &blob.contexts, put_context);
  put_vec(&mut out, &blob.isolate_data, |out, bytes| {
    put_bytes(out, bytes)
  });
  out.into_boxed_slice()
}

pub(crate) fn capture_context(
  ctx: *mut JSContext,
  external_refs: &[usize],
  context_data: &[Vec<u8>],
) -> ContextSnapshot {
  let global_object = unsafe {
    super::core::refresh_snapshot_intrinsics(ctx);
    let global = JS_GetGlobalObject(ctx);
    let bytes = write_object_bytes(ctx, global, external_refs);
    JS_FreeValue(ctx, global);
    bytes
  };
  ContextSnapshot {
    globals: if global_object.is_some() {
      Vec::new()
    } else {
      capture_globals(ctx, external_refs)
    },
    global_object,
    lexical_globals: capture_global_lexicals(ctx, external_refs),
    embedder_data: capture_embedder_data(ctx, external_refs),
    context_data: context_data.to_vec(),
  }
}

pub(crate) fn replay_context(
  isolate: *mut RealIsolate,
  ctx: *mut JSContext,
  snapshot: &ContextSnapshot,
  external_refs: &[usize],
) {
  if isolate.is_null() || ctx.is_null() {
    return;
  }
  unsafe { super::core::refresh_snapshot_intrinsics(ctx) };
  if let Some(bytes) = &snapshot.global_object
    && let Some(global) = deserialize_value_with_refs(ctx, bytes, external_refs)
  {
    unsafe { JS_FreeValue(ctx, global) };
  }
  unsafe {
    let global = JS_GetGlobalObject(ctx);
    for entry in &snapshot.globals {
      replay_global_entry(isolate, ctx, global, entry, external_refs);
    }
    JS_FreeValue(ctx, global);

    let lexical = v82jsc_global_var_obj(ctx);
    if lexical.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
    } else {
      for entry in &snapshot.lexical_globals {
        replay_lexical_global_entry(ctx, lexical, entry, external_refs);
      }
      JS_FreeValue(ctx, lexical);
    }
  }

  for (index, value) in snapshot.embedder_data.iter().enumerate() {
    let Some(bytes) = value else {
      continue;
    };
    let Some(value) = deserialize_value_with_refs(ctx, bytes, external_refs)
    else {
      continue;
    };
    super::misc::set_embedder_data_raw(ctx, index, value);
    unsafe { JS_FreeValue(ctx, value) };
  }
}

unsafe fn standard_global_names(
  ctx: *mut JSContext,
) -> Option<HashSet<String>> {
  let baseline_ctx = unsafe { JS_NewContext(JS_GetRuntime(ctx)) };
  if baseline_ctx.is_null() {
    return None;
  }
  let mut names = HashSet::new();
  unsafe {
    let global = JS_GetGlobalObject(baseline_ctx);
    let mut tab: *mut JSPropertyEnum = ptr::null_mut();
    let mut len = 0u32;
    if JS_GetOwnPropertyNames(
      baseline_ctx,
      &mut tab,
      &mut len,
      global,
      JS_GPN_STRING_MASK,
    ) >= 0
    {
      for prop in std::slice::from_raw_parts(tab, len as usize) {
        if let Some(name) = atom_to_string(baseline_ctx, prop.atom) {
          names.insert(name);
        }
      }
      JS_FreePropertyEnum(baseline_ctx, tab, len);
    } else {
      let exc = JS_GetException(baseline_ctx);
      JS_FreeValue(baseline_ctx, exc);
    }
    JS_FreeValue(baseline_ctx, global);
    JS_FreeContext(baseline_ctx);
  }
  Some(names)
}

fn capture_globals(
  ctx: *mut JSContext,
  external_refs: &[usize],
) -> Vec<GlobalEntry> {
  if ctx.is_null() {
    return Vec::new();
  }
  let mut out = Vec::new();
  unsafe {
    let standard_globals = standard_global_names(ctx);
    let global = JS_GetGlobalObject(ctx);
    let mut tab: *mut JSPropertyEnum = ptr::null_mut();
    let mut len = 0u32;
    let rc = JS_GetOwnPropertyNames(
      ctx,
      &mut tab,
      &mut len,
      global,
      JS_GPN_STRING_MASK | JS_GPN_SET_ENUM,
    );
    if rc < 0 {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      JS_FreeValue(ctx, global);
      return out;
    }
    let props = std::slice::from_raw_parts(tab, len as usize);
    for prop in props {
      let Some(name) = atom_to_string(ctx, prop.atom) else {
        continue;
      };
      if should_skip_global(&name) {
        continue;
      }
      if !prop.is_enumerable
        && standard_globals
          .as_ref()
          .is_none_or(|names| names.contains(&name))
      {
        continue;
      }
      let value = if prop.is_enumerable {
        JS_GetProperty(ctx, global, prop.atom)
      } else {
        let mut desc = JSPropertyDescriptor {
          flags: 0,
          value: jsv_undefined(),
          getter: jsv_undefined(),
          setter: jsv_undefined(),
        };
        let rc = JS_GetOwnProperty(ctx, &mut desc, global, prop.atom);
        if rc <= 0 {
          if rc < 0 {
            let exc = JS_GetException(ctx);
            JS_FreeValue(ctx, exc);
          }
          continue;
        }
        if desc.flags & JS_PROP_GETSET != 0 {
          JS_FreeValue(ctx, desc.value);
          JS_FreeValue(ctx, desc.getter);
          JS_FreeValue(ctx, desc.setter);
          continue;
        }
        JS_FreeValue(ctx, desc.getter);
        JS_FreeValue(ctx, desc.setter);
        desc.value
      };
      if value.tag == JS_TAG_EXCEPTION {
        let exc = JS_GetException(ctx);
        JS_FreeValue(ctx, exc);
        continue;
      }
      if let Some(info) = super::function::snapshot_function_info(value) {
        out.push(GlobalEntry::Function {
          name,
          callback: ExternalRefSlot::new(info.callback as usize, external_refs),
          data: info
            .data_external
            .map(|ptr| ExternalRefSlot::new(ptr as usize, external_refs)),
          length: info.length,
          constructable: info.constructable,
          enumerable: prop.is_enumerable,
        });
      } else if let Some(bytes) =
        serialize_global_value_with_refs(ctx, value, external_refs)
      {
        out.push(GlobalEntry::Value {
          name,
          bytes,
          enumerable: prop.is_enumerable,
        });
      }
      JS_FreeValue(ctx, value);
    }
    JS_FreePropertyEnum(ctx, tab, len);
    JS_FreeValue(ctx, global);
  }
  out
}

fn capture_global_lexicals(
  ctx: *mut JSContext,
  external_refs: &[usize],
) -> Vec<GlobalEntry> {
  if ctx.is_null() {
    return Vec::new();
  }
  let mut out = Vec::new();
  unsafe {
    let lexical = v82jsc_global_var_obj(ctx);
    if lexical.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      return out;
    }
    let mut tab: *mut JSPropertyEnum = ptr::null_mut();
    let mut len = 0u32;
    let rc = JS_GetOwnPropertyNames(
      ctx,
      &mut tab,
      &mut len,
      lexical,
      JS_GPN_STRING_MASK | JS_GPN_ENUM_ONLY,
    );
    if rc < 0 {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      JS_FreeValue(ctx, lexical);
      return out;
    }
    let props = std::slice::from_raw_parts(tab, len as usize);
    for prop in props {
      let Some(name) = atom_to_string(ctx, prop.atom) else {
        continue;
      };
      let value = JS_GetProperty(ctx, lexical, prop.atom);
      if value.tag == JS_TAG_EXCEPTION {
        let exc = JS_GetException(ctx);
        JS_FreeValue(ctx, exc);
        continue;
      }
      if value.tag != JS_TAG_UNINITIALIZED
        && let Some(bytes) =
          serialize_value_with_refs(ctx, value, external_refs)
      {
        out.push(GlobalEntry::LexicalValue { name, bytes });
      }
      JS_FreeValue(ctx, value);
    }
    JS_FreePropertyEnum(ctx, tab, len);
    JS_FreeValue(ctx, lexical);
  }
  out
}

fn capture_embedder_data(
  ctx: *mut JSContext,
  external_refs: &[usize],
) -> Vec<Option<Vec<u8>>> {
  super::misc::embedder_data_snapshot(ctx)
    .into_iter()
    .map(|value| {
      value
        .and_then(|value| serialize_value_with_refs(ctx, value, external_refs))
    })
    .collect()
}

unsafe fn replay_global_entry(
  isolate: *mut RealIsolate,
  ctx: *mut JSContext,
  global: JSValue,
  entry: &GlobalEntry,
  external_refs: &[usize],
) {
  match entry {
    GlobalEntry::Value {
      name,
      bytes,
      enumerable,
    } => {
      let Ok(name) = CString::new(name.as_str()) else {
        return;
      };
      let Some(value) = deserialize_value_with_refs(ctx, bytes, external_refs)
      else {
        return;
      };
      unsafe {
        define_snapshot_global(ctx, global, name.as_ptr(), value, *enumerable)
      };
    }
    GlobalEntry::Function {
      name,
      callback,
      data,
      length,
      constructable,
      enumerable,
    } => {
      let callback = callback.resolve(external_refs);
      if callback == 0 {
        return;
      }
      let Ok(name) = CString::new(name.as_str()) else {
        return;
      };
      let callback =
        unsafe { std::mem::transmute::<usize, FunctionCallback>(callback) };
      let data_value = data
        .map(|slot| slot.resolve(external_refs))
        .filter(|raw| *raw != 0)
        .map(|raw| {
          super::function::make_external_jsvalue(
            isolate,
            ctx,
            raw as *mut c_void,
          )
        })
        .unwrap_or_else(jsv_undefined);
      let function = unsafe {
        super::function::make_function_len(
          ctx,
          callback,
          data_value,
          *length,
          *constructable,
        )
      };
      if !jsv_is_undefined(&data_value) {
        unsafe { JS_FreeValue(ctx, data_value) };
      }
      unsafe {
        define_snapshot_global(
          ctx,
          global,
          name.as_ptr(),
          function,
          *enumerable,
        )
      };
    }
    GlobalEntry::LexicalValue { .. } => {}
  }
}

unsafe fn define_snapshot_global(
  ctx: *mut JSContext,
  global: JSValue,
  name: *const c_char,
  value: JSValue,
  enumerable: bool,
) {
  let mut flags = JS_PROP_CONFIGURABLE | JS_PROP_WRITABLE;
  if enumerable {
    flags |= JS_PROP_ENUMERABLE;
  }
  unsafe { JS_DefinePropertyValueStr(ctx, global, name, value, flags) };
}

unsafe fn replay_lexical_global_entry(
  ctx: *mut JSContext,
  lexical: JSValue,
  entry: &GlobalEntry,
  external_refs: &[usize],
) {
  let GlobalEntry::LexicalValue { name, bytes } = entry else {
    return;
  };
  let Ok(name) = CString::new(name.as_str()) else {
    return;
  };
  let Some(value) = deserialize_value_with_refs(ctx, bytes, external_refs)
  else {
    return;
  };
  unsafe { JS_SetPropertyStr(ctx, lexical, name.as_ptr(), value) };
}

unsafe fn atom_to_string(ctx: *mut JSContext, atom: JSAtom) -> Option<String> {
  let mut len = 0usize;
  let ptr = unsafe { JS_AtomToCStringLen(ctx, &mut len, atom) };
  if ptr.is_null() {
    return None;
  }
  let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
  let out = String::from_utf8(bytes.to_vec()).ok();
  unsafe { JS_FreeCString(ctx, ptr) };
  out
}

fn should_skip_global(name: &str) -> bool {
  matches!(
    name,
    "console"
      | "gc"
      | "Intl"
      | "Math"
      | "ShadowRealm"
      | "__v8x_import_source"
      | "__v8x_snapshot_intrinsics"
      | "__v8xKeptObjectsCleared"
      | "WeakRef"
      | "ArrayBuffer"
      | "WebAssembly"
      | "__v82jsc_wasm_src"
  )
}

fn decode_blob(bytes: &[u8]) -> Option<SnapshotBlob> {
  let mut input = Reader { bytes, pos: 0 };
  if input.take(MAGIC.len())? != MAGIC {
    return None;
  }
  Some(SnapshotBlob {
    default_context: input.get_opt_context()?,
    contexts: input.get_vec(Reader::get_context)?,
    isolate_data: input.get_vec(Reader::get_bytes)?,
  })
}

fn put_context(out: &mut Vec<u8>, context: &ContextSnapshot) {
  put_opt_bytes(out, &context.global_object);
  put_vec(out, &context.globals, put_global);
  put_vec(out, &context.lexical_globals, put_global);
  put_vec(out, &context.embedder_data, put_opt_bytes);
  put_vec(out, &context.context_data, |out, bytes| {
    put_bytes(out, bytes)
  });
}

fn put_global(out: &mut Vec<u8>, entry: &GlobalEntry) {
  match entry {
    GlobalEntry::Value {
      name,
      bytes,
      enumerable,
    } => {
      out.push(GLOBAL_VALUE);
      put_str(out, name);
      put_bytes(out, bytes);
      out.push(u8::from(*enumerable));
    }
    GlobalEntry::LexicalValue { name, bytes } => {
      out.push(GLOBAL_LEXICAL_VALUE);
      put_str(out, name);
      put_bytes(out, bytes);
    }
    GlobalEntry::Function {
      name,
      callback,
      data,
      length,
      constructable,
      enumerable,
    } => {
      out.push(GLOBAL_FUNCTION);
      put_str(out, name);
      put_ref_slot(out, *callback);
      put_opt_ref_slot(out, *data);
      put_i32(out, *length);
      out.push(u8::from(*constructable));
      out.push(u8::from(*enumerable));
    }
  }
}

fn put_opt_context(out: &mut Vec<u8>, context: Option<&ContextSnapshot>) {
  match context {
    Some(context) => {
      out.push(1);
      put_context(out, context);
    }
    None => out.push(0),
  }
}

fn put_opt_bytes(out: &mut Vec<u8>, bytes: &Option<Vec<u8>>) {
  match bytes {
    Some(bytes) => {
      out.push(1);
      put_bytes(out, bytes);
    }
    None => out.push(0),
  }
}

fn put_opt_ref_slot(out: &mut Vec<u8>, slot: Option<ExternalRefSlot>) {
  match slot {
    Some(slot) => {
      out.push(1);
      put_ref_slot(out, slot);
    }
    None => out.push(0),
  }
}

fn put_ref_slot(out: &mut Vec<u8>, slot: ExternalRefSlot) {
  put_u32(out, slot.index);
  put_u64(out, slot.raw as u64);
}

fn put_vec<T>(
  out: &mut Vec<u8>,
  values: &[T],
  mut put: impl FnMut(&mut Vec<u8>, &T),
) {
  put_u32(out, values.len().min(u32::MAX as usize) as u32);
  for value in values {
    put(out, value);
  }
}

fn put_str(out: &mut Vec<u8>, value: &str) {
  put_bytes(out, value.as_bytes());
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
  put_u32(out, bytes.len().min(u32::MAX as usize) as u32);
  out.extend_from_slice(bytes);
}

fn put_i32(out: &mut Vec<u8>, value: i32) {
  out.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
  out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
  out.extend_from_slice(&value.to_le_bytes());
}

struct Reader<'a> {
  bytes: &'a [u8],
  pos: usize,
}

impl<'a> Reader<'a> {
  fn take(&mut self, len: usize) -> Option<&'a [u8]> {
    let end = self.pos.checked_add(len)?;
    if end > self.bytes.len() {
      return None;
    }
    let out = &self.bytes[self.pos..end];
    self.pos = end;
    Some(out)
  }

  fn get_u8(&mut self) -> Option<u8> {
    Some(*self.take(1)?.first()?)
  }

  fn get_i32(&mut self) -> Option<i32> {
    Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
  }

  fn get_u32(&mut self) -> Option<u32> {
    Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
  }

  fn get_u64(&mut self) -> Option<u64> {
    Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
  }

  fn get_vec<T>(
    &mut self,
    mut get: impl FnMut(&mut Self) -> Option<T>,
  ) -> Option<Vec<T>> {
    let len = self.get_u32()? as usize;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
      out.push(get(self)?);
    }
    Some(out)
  }

  fn get_bytes(&mut self) -> Option<Vec<u8>> {
    let len = self.get_u32()? as usize;
    Some(self.take(len)?.to_vec())
  }

  fn get_string(&mut self) -> Option<String> {
    String::from_utf8(self.get_bytes()?).ok()
  }

  fn get_context(&mut self) -> Option<ContextSnapshot> {
    Some(ContextSnapshot {
      global_object: self.get_opt_bytes()?,
      globals: self.get_vec(Reader::get_global)?,
      lexical_globals: self.get_vec(Reader::get_global)?,
      embedder_data: self.get_vec(Reader::get_opt_bytes)?,
      context_data: self.get_vec(Reader::get_bytes)?,
    })
  }

  fn get_opt_context(&mut self) -> Option<Option<ContextSnapshot>> {
    match self.get_u8()? {
      0 => Some(None),
      1 => Some(Some(self.get_context()?)),
      _ => None,
    }
  }

  fn get_global(&mut self) -> Option<GlobalEntry> {
    match self.get_u8()? {
      GLOBAL_VALUE => Some(GlobalEntry::Value {
        name: self.get_string()?,
        bytes: self.get_bytes()?,
        enumerable: self.get_u8()? != 0,
      }),
      GLOBAL_LEXICAL_VALUE => Some(GlobalEntry::LexicalValue {
        name: self.get_string()?,
        bytes: self.get_bytes()?,
      }),
      GLOBAL_FUNCTION => Some(GlobalEntry::Function {
        name: self.get_string()?,
        callback: self.get_ref_slot()?,
        data: self.get_opt_ref_slot()?,
        length: self.get_i32()?,
        constructable: self.get_u8()? != 0,
        enumerable: self.get_u8()? != 0,
      }),
      _ => None,
    }
  }

  fn get_opt_bytes(&mut self) -> Option<Option<Vec<u8>>> {
    match self.get_u8()? {
      0 => Some(None),
      1 => Some(Some(self.get_bytes()?)),
      _ => None,
    }
  }

  fn get_opt_ref_slot(&mut self) -> Option<Option<ExternalRefSlot>> {
    match self.get_u8()? {
      0 => Some(None),
      1 => Some(Some(self.get_ref_slot()?)),
      _ => None,
    }
  }

  fn get_ref_slot(&mut self) -> Option<ExternalRefSlot> {
    Some(ExternalRefSlot {
      index: self.get_u32()?,
      raw: self.get_u64()? as usize,
    })
  }
}

thread_local! {
  static BLOBS: std::cell::RefCell<HashMap<usize, Box<[u8]>>> =
    Default::default();
}

pub(crate) fn leak_blob(bytes: Box<[u8]>) -> (*const u8, i32) {
  let data = bytes.as_ptr();
  let raw_size = bytes.len().min(i32::MAX as usize) as i32;
  BLOBS.with(|b| {
    b.borrow_mut().insert(data as usize, bytes);
  });
  (data, raw_size)
}

pub(crate) fn free_blob(ptr: *const u8) {
  if ptr.is_null() {
    return;
  }
  BLOBS.with(|b| {
    b.borrow_mut().remove(&(ptr as usize));
  });
}

#[cfg(test)]
mod tests {
  use super::*;

  struct TestContext {
    runtime: *mut JSRuntime,
    context: *mut JSContext,
  }

  impl TestContext {
    fn new() -> Self {
      let runtime = unsafe { JS_NewRuntime() };
      assert!(!runtime.is_null());
      let context = unsafe { JS_NewContext(runtime) };
      assert!(!context.is_null());
      crate::quickjs::core::install_default_globals(ptr::null_mut(), context);
      Self { runtime, context }
    }

    fn eval(&self, source: &str) -> JSValue {
      let source = CString::new(source).unwrap();
      let value = unsafe {
        JS_Eval(
          self.context,
          source.as_ptr(),
          source.as_bytes().len(),
          c"snapshot-test.js".as_ptr(),
          JS_EVAL_TYPE_GLOBAL,
        )
      };
      assert!(!jsv_is_exception(&value));
      value
    }
  }

  impl Drop for TestContext {
    fn drop(&mut self) {
      unsafe {
        JS_FreeContext(self.context);
        JS_FreeRuntime(self.runtime);
      }
    }
  }

  unsafe extern "C" fn snapshot_callback_a(
    _info: *const crate::FunctionCallbackInfo,
  ) {
  }

  unsafe extern "C" fn snapshot_callback_b(
    _info: *const crate::FunctionCallbackInfo,
  ) {
  }

  #[test]
  fn host_functions_resolve_external_reference_slots() {
    let test = TestContext::new();
    let function = unsafe {
      super::super::function::make_function_len(
        test.context,
        snapshot_callback_a,
        jsv_undefined(),
        0,
        false,
      )
    };
    let source_refs = [snapshot_callback_a as *const () as usize, 0];
    let bytes =
      serialize_value_with_refs(test.context, function, &source_refs).unwrap();
    assert_eq!(
      u32::from_le_bytes(bytes[9..13].try_into().unwrap()),
      0,
      "callback must be stored by external-reference index",
    );

    let target_refs = [snapshot_callback_b as *const () as usize, 0];
    let restored =
      deserialize_value_with_refs(test.context, &bytes, &target_refs).unwrap();
    let info =
      super::super::function::snapshot_function_info(restored).unwrap();
    assert_eq!(
      info.callback as *const () as usize,
      snapshot_callback_b as *const () as usize,
    );

    unsafe {
      JS_FreeValue(test.context, restored);
      JS_FreeValue(test.context, function);
    }
  }

  #[test]
  fn restored_host_functions_reuse_dispatch_entries() {
    let test = TestContext::new();
    let function = unsafe {
      super::super::function::make_function_len(
        test.context,
        snapshot_callback_a,
        jsv_undefined(),
        0,
        false,
      )
    };
    let bytes = serialize_value(test.context, function).unwrap();
    let dispatch_count = super::super::function::dispatch_entry_count();

    for _ in 0..128 {
      let restored =
        deserialize_value_with_refs(test.context, &bytes, &[]).unwrap();
      unsafe { JS_FreeValue(test.context, restored) };
    }

    assert_eq!(
      super::super::function::dispatch_entry_count(),
      dispatch_count,
    );
    unsafe { JS_FreeValue(test.context, function) };
  }

  #[test]
  fn host_function_constructor_prototypes_roundtrip() {
    let source = TestContext::new();
    let source_function = unsafe {
      super::super::function::make_function_len(
        source.context,
        snapshot_callback_a,
        jsv_undefined(),
        0,
        true,
      )
    };
    let source_prototype = source.eval("({ log() { return 42; } })");
    assert_eq!(
      unsafe {
        JS_DefinePropertyValueStr(
          source.context,
          source_function,
          c"prototype".as_ptr(),
          source_prototype,
          JS_PROP_WRITABLE,
        )
      },
      1,
    );
    let source_global = unsafe { JS_GetGlobalObject(source.context) };
    assert_eq!(
      unsafe {
        JS_SetPropertyStr(
          source.context,
          source_global,
          c"HostConstructor".as_ptr(),
          JS_DupValue(source.context, source_function),
        )
      },
      1,
    );
    unsafe { super::super::core::refresh_snapshot_intrinsics(source.context) };
    let result =
      source.eval("HostConstructor.prototype.constructor = HostConstructor");
    unsafe { JS_FreeValue(source.context, result) };
    let bytes = write_object_bytes(source.context, source_global, &[]).unwrap();

    let target = TestContext::new();
    let restored_global =
      deserialize_value_with_refs(target.context, &bytes, &[]).unwrap();

    unsafe {
      let restored = JS_GetPropertyStr(
        target.context,
        restored_global,
        c"HostConstructor".as_ptr(),
      );
      let prototype =
        JS_GetPropertyStr(target.context, restored, c"prototype".as_ptr());
      let constructor =
        JS_GetPropertyStr(target.context, prototype, c"constructor".as_ptr());
      assert_eq!(constructor.u.ptr, restored.u.ptr);
      let log = JS_GetPropertyStr(target.context, prototype, c"log".as_ptr());
      let result = JS_Call(target.context, log, prototype, 0, ptr::null_mut());
      assert!(!jsv_is_exception(&result));
      let mut number = 0;
      assert_eq!(JS_ToInt32(target.context, &mut number, result), 0);
      assert_eq!(number, 42);

      JS_FreeValue(target.context, result);
      JS_FreeValue(target.context, log);
      JS_FreeValue(target.context, constructor);
      JS_FreeValue(target.context, prototype);
      JS_FreeValue(target.context, restored);
      JS_FreeValue(target.context, restored_global);
      JS_FreeValue(source.context, source_global);
      JS_FreeValue(source.context, source_function);
    }
  }

  #[test]
  fn context_global_roundtrips_as_one_object_graph() {
    let source = TestContext::new();
    let setup = source.eval(
      "(() => {\
         const brand = Symbol('brand');\
         class Root {}\
         Object.setPrototypeOf(globalThis, Root.prototype);\
         globalThis[brand] = brand;\
         globalThis.alias = globalThis;\
         globalThis.verifySnapshot = () =>\
           (Object.getPrototypeOf(globalThis) === Root.prototype ? 1 : 0) +\
           (globalThis[brand] === brand ? 2 : 0) +\
           (globalThis.alias === globalThis ? 4 : 0);\
       })()",
    );
    unsafe { JS_FreeValue(source.context, setup) };
    let source_global = unsafe { JS_GetGlobalObject(source.context) };
    let bytes = write_object_bytes(source.context, source_global, &[]).unwrap();
    unsafe { JS_FreeValue(source.context, source_global) };

    let target = TestContext::new();
    let restored =
      deserialize_value_with_refs(target.context, &bytes, &[]).unwrap();

    unsafe {
      let target_global = JS_GetGlobalObject(target.context);
      assert_eq!(restored.tag, JS_TAG_OBJECT);
      assert_eq!(restored.u.ptr, target_global.u.ptr);
      let verify = JS_GetPropertyStr(
        target.context,
        target_global,
        c"verifySnapshot".as_ptr(),
      );
      let result =
        JS_Call(target.context, verify, target_global, 0, ptr::null_mut());
      assert!(!jsv_is_exception(&result));
      let mut checks = 0;
      assert_eq!(JS_ToInt32(target.context, &mut checks, result), 0);
      assert_eq!(checks, 7);

      JS_FreeValue(target.context, result);
      JS_FreeValue(target.context, verify);
      JS_FreeValue(target.context, target_global);
      JS_FreeValue(target.context, restored);
    }
  }

  #[test]
  fn bytecode_functions_roundtrip_shared_closure_state() {
    let test = TestContext::new();
    let source_value = test.eval(
      "(() => {\
         let state = 0;\
         const increment = () => ++state;\
         const read = () => state;\
         return { increment, read };\
       })()",
    );
    let bytes = serialize_value(test.context, source_value).unwrap();
    let restored =
      deserialize_value_with_refs(test.context, &bytes, &[]).unwrap();

    unsafe {
      let increment =
        JS_GetPropertyStr(test.context, restored, c"increment".as_ptr());
      let read = JS_GetPropertyStr(test.context, restored, c"read".as_ptr());
      let incremented =
        JS_Call(test.context, increment, restored, 0, ptr::null_mut());
      assert!(!jsv_is_exception(&incremented));
      let mut incremented_number = 0;
      assert_eq!(
        JS_ToInt32(test.context, &mut incremented_number, incremented),
        0
      );
      assert_eq!(incremented_number, 1);

      let current = JS_Call(test.context, read, restored, 0, ptr::null_mut());
      assert!(!jsv_is_exception(&current));
      let mut current_number = 0;
      assert_eq!(JS_ToInt32(test.context, &mut current_number, current), 0);
      assert_eq!(current_number, 1);

      JS_FreeValue(test.context, current);
      JS_FreeValue(test.context, incremented);
      JS_FreeValue(test.context, read);
      JS_FreeValue(test.context, increment);
      JS_FreeValue(test.context, restored);
      JS_FreeValue(test.context, source_value);
    }
  }

  #[test]
  fn bytecode_function_roundtrips_recursive_closure() {
    let test = TestContext::new();
    let value =
      test.eval("(() => { let self; self = () => self; return self; })()");
    let bytes = serialize_value(test.context, value).unwrap();
    let restored =
      deserialize_value_with_refs(test.context, &bytes, &[]).unwrap();

    unsafe {
      let result =
        JS_Call(test.context, restored, jsv_undefined(), 0, ptr::null_mut());
      assert!(!jsv_is_exception(&result));
      assert_eq!(result.tag, JS_TAG_OBJECT);
      assert_eq!(result.u.ptr, restored.u.ptr);

      JS_FreeValue(test.context, result);
      JS_FreeValue(test.context, restored);
      JS_FreeValue(test.context, value);
    }
  }

  #[test]
  fn module_namespace_roundtrips_live_bindings() {
    let test = TestContext::new();
    let source = CString::new(
      "export let value = 1;\
       export function increment() { value++; }",
    )
    .unwrap();
    let module = unsafe {
      JS_Eval(
        test.context,
        source.as_ptr(),
        source.as_bytes().len(),
        c"snapshot-module.js".as_ptr(),
        JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_COMPILE_ONLY,
      )
    };
    assert_eq!(module.tag, JS_TAG_MODULE);
    let module_def = unsafe { module.u.ptr }.cast::<JSModuleDef>();
    let evaluated = unsafe {
      JS_EvalFunction(test.context, JS_DupValue(test.context, module))
    };
    assert!(!jsv_is_exception(&evaluated));
    let namespace = unsafe { JS_GetModuleNamespace(test.context, module_def) };
    assert!(!jsv_is_exception(&namespace));
    let bytes = serialize_value(test.context, namespace).unwrap();
    let restored =
      deserialize_value_with_refs(test.context, &bytes, &[]).unwrap();

    unsafe {
      let increment =
        JS_GetPropertyStr(test.context, restored, c"increment".as_ptr());
      let result =
        JS_Call(test.context, increment, restored, 0, ptr::null_mut());
      assert!(!jsv_is_exception(&result));
      let value = JS_GetPropertyStr(test.context, restored, c"value".as_ptr());
      let mut number = 0;
      assert_eq!(JS_ToInt32(test.context, &mut number, value), 0);
      assert_eq!(number, 2);

      JS_FreeValue(test.context, value);
      JS_FreeValue(test.context, result);
      JS_FreeValue(test.context, increment);
      JS_FreeValue(test.context, restored);
      JS_FreeValue(test.context, namespace);
      JS_FreeValue(test.context, evaluated);
      JS_FreeValue(test.context, module);
    }
  }

  #[test]
  fn bytecode_class_roundtrips_prototype() {
    let test = TestContext::new();
    let source_value =
      test.eval("class Window { method() { return 42; } }; Window");
    let bytes = serialize_value(test.context, source_value).unwrap();
    let restored =
      deserialize_value_with_refs(test.context, &bytes, &[]).unwrap();

    unsafe {
      assert!(JS_IsConstructor(test.context, restored));
      let instance =
        JS_CallConstructor(test.context, restored, 0, ptr::null_mut());
      assert_eq!(instance.tag, JS_TAG_OBJECT);
      let prototype =
        JS_GetPropertyStr(test.context, restored, c"prototype".as_ptr());
      assert_eq!(prototype.tag, JS_TAG_OBJECT);
      let constructor =
        JS_GetPropertyStr(test.context, prototype, c"constructor".as_ptr());
      assert_eq!(constructor.tag, JS_TAG_OBJECT);
      assert_eq!(constructor.u.ptr, restored.u.ptr);
      let method =
        JS_GetPropertyStr(test.context, instance, c"method".as_ptr());
      let result = JS_Call(test.context, method, instance, 0, ptr::null_mut());
      assert!(!jsv_is_exception(&result));
      let mut number = 0;
      assert_eq!(JS_ToInt32(test.context, &mut number, result), 0);
      assert_eq!(number, 42);

      JS_FreeValue(test.context, result);
      JS_FreeValue(test.context, method);
      JS_FreeValue(test.context, constructor);
      JS_FreeValue(test.context, prototype);
      JS_FreeValue(test.context, instance);
      JS_FreeValue(test.context, restored);
      JS_FreeValue(test.context, source_value);
    }
  }

  #[test]
  fn bytecode_function_roundtrips_array_named_properties() {
    let test = TestContext::new();
    let source_value = test.eval(
      "(() => {\
         const converters = [];\
         converters.DOMString = (value) => String(value);\
         converters.suffix = 42;\
         return (value) => converters.DOMString(value) + converters.suffix;\
       })()",
    );
    let bytes = serialize_value(test.context, source_value).unwrap();
    let restored =
      deserialize_value_with_refs(test.context, &bytes, &[]).unwrap();
    let argument = test.eval("'value-'");

    unsafe {
      let mut arguments = [argument];
      let result = JS_Call(
        test.context,
        restored,
        jsv_undefined(),
        arguments.len() as c_int,
        arguments.as_mut_ptr(),
      );
      assert!(!jsv_is_exception(&result));
      let mut len = 0;
      let text = JS_ToCStringLen(test.context, &mut len, result);
      assert!(!text.is_null());
      let bytes = std::slice::from_raw_parts(text as *const u8, len);
      assert_eq!(bytes, b"value-42");

      JS_FreeCString(test.context, text);
      JS_FreeValue(test.context, result);
      JS_FreeValue(test.context, argument);
      JS_FreeValue(test.context, restored);
      JS_FreeValue(test.context, source_value);
    }
  }

  #[test]
  fn native_bound_function_roundtrips() {
    let test = TestContext::new();
    let source_value =
      test.eval("Function.prototype.call.bind(Array.prototype.join)");
    let bytes = serialize_value(test.context, source_value).unwrap();
    let restored =
      deserialize_value_with_refs(test.context, &bytes, &[]).unwrap();
    let argument = test.eval("[1, 2]");
    let separator = test.eval("'-'");

    unsafe {
      let mut arguments = [argument, separator];
      let result = JS_Call(
        test.context,
        restored,
        jsv_undefined(),
        arguments.len() as c_int,
        arguments.as_mut_ptr(),
      );
      assert!(!jsv_is_exception(&result));
      let mut len = 0;
      let text = JS_ToCStringLen(test.context, &mut len, result);
      assert!(!text.is_null());
      let bytes = std::slice::from_raw_parts(text as *const u8, len);
      assert_eq!(bytes, b"1-2");

      JS_FreeCString(test.context, text);
      JS_FreeValue(test.context, result);
      JS_FreeValue(test.context, separator);
      JS_FreeValue(test.context, argument);
      JS_FreeValue(test.context, restored);
      JS_FreeValue(test.context, source_value);
    }
  }

  #[test]
  fn native_intrinsics_roundtrip() {
    let test = TestContext::new();
    for source in [
      "Array",
      "Array.fromAsync",
      "Atomics.notify",
      "Object.prototype.toLocaleString",
      "Object.getPrototypeOf((function*() {})()).next",
    ] {
      let source_value = test.eval(source);
      let bytes = serialize_value(test.context, source_value)
        .unwrap_or_else(|| panic!("failed to serialize {source}"));
      let restored = deserialize_value_with_refs(test.context, &bytes, &[])
        .unwrap_or_else(|| panic!("failed to deserialize {source}"));
      assert!(unsafe { JS_IsFunction(test.context, restored) }, "{source}");
      unsafe {
        JS_FreeValue(test.context, restored);
        JS_FreeValue(test.context, source_value);
      }
    }
  }

  #[test]
  fn regexp_references_roundtrip_with_identity() {
    let test = TestContext::new();
    let source_value =
      test.eval("(() => { const re = /x/g; return { re, same: re }; })()");
    let bytes = serialize_value(test.context, source_value).unwrap();
    let restored =
      deserialize_value_with_refs(test.context, &bytes, &[]).unwrap();

    unsafe {
      let re = JS_GetPropertyStr(test.context, restored, c"re".as_ptr());
      let same = JS_GetPropertyStr(test.context, restored, c"same".as_ptr());
      assert_eq!(re.tag, JS_TAG_OBJECT);
      assert_eq!(re.u.ptr, same.u.ptr);

      JS_FreeValue(test.context, same);
      JS_FreeValue(test.context, re);
      JS_FreeValue(test.context, restored);
      JS_FreeValue(test.context, source_value);
    }
  }

  #[test]
  fn weak_collections_roundtrip_with_shared_key() {
    let test = TestContext::new();
    let source_value = test.eval(
      "(() => {\
         const key = {};\
         return {\
           key,\
           map: new WeakMap([[key, 42]]),\
           set: new WeakSet([key]),\
         };\
       })()",
    );
    let bytes = serialize_value(test.context, source_value).unwrap();
    let restored =
      deserialize_value_with_refs(test.context, &bytes, &[]).unwrap();

    unsafe {
      let key = JS_GetPropertyStr(test.context, restored, c"key".as_ptr());
      let map = JS_GetPropertyStr(test.context, restored, c"map".as_ptr());
      let set = JS_GetPropertyStr(test.context, restored, c"set".as_ptr());
      let get = JS_GetPropertyStr(test.context, map, c"get".as_ptr());
      let has = JS_GetPropertyStr(test.context, set, c"has".as_ptr());
      let mut args = [key];
      let mapped = JS_Call(test.context, get, map, 1, args.as_mut_ptr());
      let contained = JS_Call(test.context, has, set, 1, args.as_mut_ptr());
      assert!(!jsv_is_exception(&mapped));
      assert!(!jsv_is_exception(&contained));
      let mut mapped_number = 0;
      assert_eq!(JS_ToInt32(test.context, &mut mapped_number, mapped), 0);
      assert_eq!(mapped_number, 42);
      assert_ne!(JS_ToBool(test.context, contained), 0);

      JS_FreeValue(test.context, contained);
      JS_FreeValue(test.context, mapped);
      JS_FreeValue(test.context, has);
      JS_FreeValue(test.context, get);
      JS_FreeValue(test.context, set);
      JS_FreeValue(test.context, map);
      JS_FreeValue(test.context, key);
      JS_FreeValue(test.context, restored);
      JS_FreeValue(test.context, source_value);
    }
  }

  #[test]
  fn finalization_registry_roundtrips_live_registration() {
    let test = TestContext::new();
    let source_value = test.eval(
      "(() => {\
         const calls = [];\
         const key = {};\
         const token = {};\
         const registry = new FinalizationRegistry(value => calls.push(value));\
         registry.register(key, 42, token);\
         return { registry, key, token, calls };\
       })()",
    );
    let bytes = serialize_value(test.context, source_value).unwrap();
    let restored =
      deserialize_value_with_refs(test.context, &bytes, &[]).unwrap();

    unsafe {
      let registry =
        JS_GetPropertyStr(test.context, restored, c"registry".as_ptr());
      let token = JS_GetPropertyStr(test.context, restored, c"token".as_ptr());
      let unregister =
        JS_GetPropertyStr(test.context, registry, c"unregister".as_ptr());
      let mut args = [token];
      let removed =
        JS_Call(test.context, unregister, registry, 1, args.as_mut_ptr());
      let removed_again =
        JS_Call(test.context, unregister, registry, 1, args.as_mut_ptr());
      assert!(!jsv_is_exception(&removed));
      assert!(!jsv_is_exception(&removed_again));
      assert_ne!(JS_ToBool(test.context, removed), 0);
      assert_eq!(JS_ToBool(test.context, removed_again), 0);

      JS_FreeValue(test.context, removed_again);
      JS_FreeValue(test.context, removed);
      JS_FreeValue(test.context, unregister);
      JS_FreeValue(test.context, token);
      JS_FreeValue(test.context, registry);
      JS_FreeValue(test.context, restored);
      JS_FreeValue(test.context, source_value);
    }
  }

  #[test]
  fn proxies_roundtrip_cycles_and_revocation() {
    let test = TestContext::new();
    let source_value = test.eval(
      "(() => {\
         const target = { value: 41 };\
         const handler = {\
           get(target, key, receiver) {\
             if (key === 'answer') return target.value + 1;\
             return Reflect.get(target, key, receiver);\
           },\
         };\
         const proxy = new Proxy(target, handler);\
         target.proxy = proxy;\
         const revocable = Proxy.revocable({ value: 7 }, {});\
         revocable.revoke();\
         return {\
           proxy,\
           target,\
           revokedProxy: revocable.proxy,\
         };\
       })()",
    );
    let bytes = serialize_value(test.context, source_value).unwrap();
    let restored =
      deserialize_value_with_refs(test.context, &bytes, &[]).unwrap();

    unsafe {
      let proxy = JS_GetPropertyStr(test.context, restored, c"proxy".as_ptr());
      let target =
        JS_GetPropertyStr(test.context, restored, c"target".as_ptr());
      let answer = JS_GetPropertyStr(test.context, proxy, c"answer".as_ptr());
      let target_proxy =
        JS_GetPropertyStr(test.context, target, c"proxy".as_ptr());
      let mut answer_number = 0;
      assert_eq!(JS_ToInt32(test.context, &mut answer_number, answer), 0);
      assert_eq!(answer_number, 42);
      assert_eq!(target_proxy.tag, JS_TAG_OBJECT);
      assert_eq!(target_proxy.u.ptr, proxy.u.ptr);

      let revoked_proxy =
        JS_GetPropertyStr(test.context, restored, c"revokedProxy".as_ptr());
      let revoked_value =
        JS_GetPropertyStr(test.context, revoked_proxy, c"value".as_ptr());
      assert!(jsv_is_exception(&revoked_value));
      let exception = JS_GetException(test.context);

      JS_FreeValue(test.context, exception);
      JS_FreeValue(test.context, revoked_proxy);
      JS_FreeValue(test.context, target_proxy);
      JS_FreeValue(test.context, answer);
      JS_FreeValue(test.context, target);
      JS_FreeValue(test.context, proxy);
      JS_FreeValue(test.context, restored);
      JS_FreeValue(test.context, source_value);
    }
  }

  #[test]
  fn unsupported_object_roundtrips_data_properties_without_invoking_accessors()
  {
    let test = TestContext::new();
    let source_value = test.eval(
      "({\
         core: { ops: { value: 42 } },\
         get unsupported() { throw new Error('must not run'); }\
       })",
    );
    let bytes = serialize_value(test.context, source_value).unwrap();
    let restored =
      deserialize_value_with_refs(test.context, &bytes, &[]).unwrap();

    unsafe {
      let core = JS_GetPropertyStr(test.context, restored, c"core".as_ptr());
      let ops = JS_GetPropertyStr(test.context, core, c"ops".as_ptr());
      let property_value =
        JS_GetPropertyStr(test.context, ops, c"value".as_ptr());
      let mut number = 0;
      assert_eq!(JS_ToInt32(test.context, &mut number, property_value), 0);
      assert_eq!(number, 42);

      JS_FreeValue(test.context, property_value);
      JS_FreeValue(test.context, ops);
      JS_FreeValue(test.context, core);
      JS_FreeValue(test.context, restored);
      JS_FreeValue(test.context, source_value);
    }
  }
}
