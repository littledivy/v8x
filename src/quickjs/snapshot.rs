//! Snapshot support utilities for the QuickJS backend.
//!
//! The snapshot FORMAT lives in `capi_tape.rs` (C-API record/replay tape,
//! magic `V8XTAPE1`): a `SnapshotCreator` records the embedder's C-ABI calls
//! and `CreateBlob` serializes them; restoring replays the calls against a
//! fresh runtime. This module keeps only the shared value/blob plumbing:
//!
//! - `serialize_value` / `deserialize_value`: structured-clone one JS value
//!   to/from bytes (`JS_WriteObject`/`JS_ReadObject`, no bytecode) — used for
//!   tape `ClonedValue` entries and embedder-data snapshots.
//! - `leak_blob` / `free_blob`: hand a serialized blob across the C ABI with
//!   `StartupData`-compatible ownership.

use std::collections::HashMap;
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use super::quickjs_sys::*;
use crate::FunctionCallback;
use crate::RealIsolate;

const MAGIC: &[u8; 8] = b"V8XSNP1\0";
const NO_REF_INDEX: u32 = u32::MAX;
const GLOBAL_VALUE: u8 = 0;
const GLOBAL_FUNCTION: u8 = 1;
const JS_GPN_STRING_MASK: c_int = 1 << 0;
const JS_GPN_ENUM_ONLY: c_int = 1 << 4;

#[repr(C)]
struct JSPropertyEnum {
  is_enumerable: bool,
  atom: JSAtom,
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
}

#[derive(Clone)]
pub(crate) struct SnapshotBlob {
  pub default_context: Option<ContextSnapshot>,
  pub contexts: Vec<ContextSnapshot>,
  pub isolate_data: Vec<Vec<u8>>,
}

#[derive(Clone)]
pub(crate) struct ContextSnapshot {
  pub globals: Vec<GlobalEntry>,
  pub embedder_data: Vec<Option<Vec<u8>>>,
  pub context_data: Vec<Vec<u8>>,
}

#[derive(Clone)]
pub(crate) enum GlobalEntry {
  Value {
    name: String,
    bytes: Vec<u8>,
  },
  Function {
    name: String,
    callback: ExternalRefSlot,
    data: Option<ExternalRefSlot>,
    length: i32,
    constructable: bool,
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

/// Structured-clone one JS value to bytes (no bytecode: plain data graph).
pub(crate) fn serialize_value(
  ctx: *mut JSContext,
  v: JSValue,
) -> Option<Vec<u8>> {
  let mut size: usize = 0;
  let buf = unsafe { JS_WriteObject(ctx, &mut size, v, 0) };
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

pub(crate) fn deserialize_value(
  ctx: *mut JSContext,
  bytes: &[u8],
) -> Option<JSValue> {
  if ctx.is_null() || bytes.is_empty() {
    return None;
  }
  let value = unsafe { JS_ReadObject(ctx, bytes.as_ptr(), bytes.len(), 0) };
  if value.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return None;
  }
  Some(value)
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
  ContextSnapshot {
    globals: capture_globals(ctx, external_refs),
    embedder_data: capture_embedder_data(ctx),
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
  unsafe {
    let global = JS_GetGlobalObject(ctx);
    for entry in &snapshot.globals {
      replay_global_entry(isolate, ctx, global, entry, external_refs);
    }
    JS_FreeValue(ctx, global);
  }

  for (index, value) in snapshot.embedder_data.iter().enumerate() {
    let Some(bytes) = value else {
      continue;
    };
    let Some(value) = deserialize_value(ctx, bytes) else {
      continue;
    };
    super::misc::set_embedder_data_raw(ctx, index, value);
    unsafe { JS_FreeValue(ctx, value) };
  }
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
    let global = JS_GetGlobalObject(ctx);
    let mut tab: *mut JSPropertyEnum = ptr::null_mut();
    let mut len = 0u32;
    let rc = JS_GetOwnPropertyNames(
      ctx,
      &mut tab,
      &mut len,
      global,
      JS_GPN_STRING_MASK | JS_GPN_ENUM_ONLY,
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
      let value = JS_GetProperty(ctx, global, prop.atom);
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
        });
      } else if let Some(bytes) = serialize_value(ctx, value) {
        out.push(GlobalEntry::Value { name, bytes });
      }
      JS_FreeValue(ctx, value);
    }
    JS_FreePropertyEnum(ctx, tab, len);
    JS_FreeValue(ctx, global);
  }
  out
}

fn capture_embedder_data(ctx: *mut JSContext) -> Vec<Option<Vec<u8>>> {
  super::misc::embedder_data_snapshot(ctx)
    .into_iter()
    .map(|value| value.and_then(|value| serialize_value(ctx, value)))
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
    GlobalEntry::Value { name, bytes } => {
      let Ok(name) = CString::new(name.as_str()) else {
        return;
      };
      let Some(value) = deserialize_value(ctx, bytes) else {
        return;
      };
      unsafe { JS_SetPropertyStr(ctx, global, name.as_ptr(), value) };
    }
    GlobalEntry::Function {
      name,
      callback,
      data,
      length,
      constructable,
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
      unsafe { JS_SetPropertyStr(ctx, global, name.as_ptr(), function) };
    }
  }
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
      | "__v8xKeptObjectsCleared"
      | "WeakRef"
      | "ArrayBuffer"
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
  put_vec(out, &context.globals, put_global);
  put_vec(out, &context.embedder_data, put_opt_bytes);
  put_vec(out, &context.context_data, |out, bytes| {
    put_bytes(out, bytes)
  });
}

fn put_global(out: &mut Vec<u8>, entry: &GlobalEntry) {
  match entry {
    GlobalEntry::Value { name, bytes } => {
      out.push(GLOBAL_VALUE);
      put_str(out, name);
      put_bytes(out, bytes);
    }
    GlobalEntry::Function {
      name,
      callback,
      data,
      length,
      constructable,
    } => {
      out.push(GLOBAL_FUNCTION);
      put_str(out, name);
      put_ref_slot(out, *callback);
      put_opt_ref_slot(out, *data);
      put_i32(out, *length);
      out.push(u8::from(*constructable));
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
      globals: self.get_vec(Reader::get_global)?,
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
      }),
      GLOBAL_FUNCTION => Some(GlobalEntry::Function {
        name: self.get_string()?,
        callback: self.get_ref_slot()?,
        data: self.get_opt_ref_slot()?,
        length: self.get_i32()?,
        constructable: self.get_u8()? != 0,
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
