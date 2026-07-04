//! C-API record/replay snapshots ("tape v2").
//!
//! Goal: STOCK deno_core. Real V8 serializes its heap; the previous replay
//! design (snapshot.rs) re-ran only JS and needed deno_core's cooperation
//! (v8x_snapshot_init_boundary, forced InitMode::New) because heap state
//! built through Rust C-API calls — `Deno.core`, op functions, primordial
//! wiring — was invisible to a JS-only tape.
//!
//! This module records the C-ABI calls themselves. While a `SnapshotCreator`
//! isolate is live and control is OUTSIDE JS (see the depth guard), every
//! handle-producing or heap-mutating `v8__*` call appends a [`TapeOp`].
//! Replaying the tape against a fresh isolate reproduces the creator's heap
//! without the embedder re-running any of its init — exactly the contract
//! deno_core's `InitMode::FromSnapshot` expects.
//!
//! Three pillars:
//!
//! * **Handle IDs** — every recorded call that returns a handle assigns the
//!   result a `u32` id; later calls reference arguments by id. The record-time
//!   map is keyed by arena pointer, latest-wins: an arena slot reused after a
//!   scope pop simply rebinds the pointer to the newest (only live) handle.
//!
//! * **External references** — function/data pointers (op callbacks, op-ctx
//!   pointers, externalized source strings) cannot cross processes. They are
//!   encoded as INDICES into `CreateParams.external_references`, the same
//!   zero-terminated table real V8 uses to make snapshots ASLR-safe; the
//!   restoring process resolves indices through its own copy of the table.
//!   A pointer that is not in the table marks the tape incomplete (V8 fails
//!   the same way: "Unknown external reference").
//!
//! * **JS-depth guard** — C-API calls made while JS is executing (ops fired
//!   during an extension-module eval) are NOT recorded: replaying the eval
//!   re-fires the ops live in the restore process, which re-issues those
//!   calls itself. Recording them too would double-apply the effects.
//!
//! Script/module entries carry QuickJS BYTECODE (`JS_WriteObject`), so a
//! restore skips every parse — this is also the compiled-binary startup fix.

#![allow(dead_code)]

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashMap;

use super::quickjs_sys::*;

unsafe extern "C" {
  fn JS_ValueToAtom(ctx: *mut JSContext, val: JSValue) -> JSAtom;
  fn JS_SetProperty(
    ctx: *mut JSContext,
    this_obj: JSValue,
    prop: JSAtom,
    val: JSValue,
  ) -> std::os::raw::c_int;
  fn JS_DefinePropertyValue(
    ctx: *mut JSContext,
    this_obj: JSValue,
    prop: JSAtom,
    val: JSValue,
    flags: std::os::raw::c_int,
  ) -> std::os::raw::c_int;
  fn JS_GetProperty(
    ctx: *mut JSContext,
    this_obj: JSValue,
    prop: JSAtom,
  ) -> JSValue;
  fn JS_SetPrototype(
    ctx: *mut JSContext,
    obj: JSValue,
    proto_val: JSValue,
  ) -> std::os::raw::c_int;
}

// ---------------------------------------------------------------------------
// Tape operations
// ---------------------------------------------------------------------------

pub(crate) type HandleId = u32;

#[derive(Clone, Debug)]
pub(crate) enum TapeOp {
  /// Bind id → a fresh context (creator called Context::New). Replay creates
  /// a real JSContext; `Context::from_snapshot(index)` / the default context
  /// resolve to these.
  ContextNew {
    id: HandleId,
  },
  /// Bind id → an existing context's global object.
  ContextGlobal {
    id: HandleId,
    ctx: HandleId,
  },

  StringNew {
    id: HandleId,
    utf8: Vec<u8>,
  },
  NumberNew {
    id: HandleId,
    value: f64,
  },
  BoolNew {
    id: HandleId,
    value: bool,
  },
  UndefinedNew {
    id: HandleId,
  },
  NullNew {
    id: HandleId,
  },
  ObjectNew {
    id: HandleId,
  },
  ArrayNew {
    id: HandleId,
    length: u32,
  },
  /// v8::External / embedder pointer wrapped as a value; pointer encoded as
  /// an external-reference-table index.
  ExternalNew {
    id: HandleId,
    ext_ref: u32,
  },
  /// v8::Function::New with a native callback (ext-ref index) and optional
  /// data value.
  FunctionNew {
    id: HandleId,
    ctx: HandleId,
    cb_ext_ref: u32,
    data: Option<HandleId>,
    /// Template-path data: an External whose pointer lives in the ext-ref
    /// table (op ctx) — used when the data value was never a recorded handle.
    data_ext: Option<u32>,
    name: Option<HandleId>,
    length: i32,
  },

  /// obj[key] = value (Object::Set / CreateDataProperty).
  SetProp {
    ctx: HandleId,
    obj: HandleId,
    key: HandleId,
    value: HandleId,
  },
  /// Object::DefineOwnProperty with attributes (bitmask: 1=readonly,
  /// 2=dontenum, 4=dontdelete — v8::PropertyAttribute).
  DefineProp {
    ctx: HandleId,
    obj: HandleId,
    key: HandleId,
    value: HandleId,
    attrs: u32,
  },
  /// A read that produced a handle later referenced by the tape.
  GetProp {
    id: HandleId,
    ctx: HandleId,
    obj: HandleId,
    key: HandleId,
  },
  SetPrototype {
    ctx: HandleId,
    obj: HandleId,
    proto: HandleId,
  },

  /// Compile+run a script from bytecode. `result` binds the completion value.
  ScriptRun {
    result: HandleId,
    ctx: HandleId,
    bytecode: Vec<u8>,
    /// Original source, kept for cache-miss fallback + stack traces.
    source: Vec<u8>,
    filename: Vec<u8>,
    eval_flags: i32,
  },
  /// Register a module source (feeds the loader during later evals).
  ModuleSource {
    name: Vec<u8>,
    source: Vec<u8>,
  },
  /// Compile+instantiate+evaluate a module graph root from bytecode.
  ModuleEval {
    result: HandleId,
    ctx: HandleId,
    name: Vec<u8>,
    bytecode: Vec<u8>,
    source: Vec<u8>,
  },

  /// SnapshotCreator::AddData(context, handle) — restore hands the handle
  /// back through GetDataFromSnapshotOnce(index_in_ctx).
  AddContextData {
    ctx: HandleId,
    value: HandleId,
  },
  AddIsolateData {
    value: HandleId,
  },
  /// Creator's SetDefaultContext / AddContext — establishes which tape
  /// context materializes for Context::new vs Context::from_snapshot(i).
  SetDefaultContext {
    ctx: HandleId,
  },
  AddContext {
    ctx: HandleId,
  },

  /// Context embedder-data value slot (structured-clone bytes).
  SetEmbedderData {
    ctx: HandleId,
    index: u32,
    value: HandleId,
  },
  /// Read of an embedder-data value slot that produced a referenced handle.
  GetEmbedderData {
    id: HandleId,
    ctx: HandleId,
    index: u32,
  },
  /// Rust-driven JS call during embedder init (e.g. deno_core invoking its
  /// setUpAsyncStub JS helper per async op). Replay re-invokes it.
  FunctionCall {
    result: HandleId,
    ctx: HandleId,
    callee: HandleId,
    recv: HandleId,
    args: Vec<HandleId>,
  },
  /// Module wrapper handle from ScriptCompiler::CompileModule (the module's
  /// side-table state is reconstructed by name; sources ride ModuleSource).
  ModuleCompile {
    id: HandleId,
    ctx: HandleId,
    name: Vec<u8>,
  },
  /// Context::GetExtrasBindingObject result.
  ExtrasBinding {
    id: HandleId,
    ctx: HandleId,
  },
  /// FunctionTemplate::New — template handles are shim-side structs, not JS
  /// values; they carry an id so template context data survives a restore.
  TemplateNew {
    id: HandleId,
    cb_ext_ref: u32,
    data_ext: Option<u32>,
    length: i32,
    constructable: bool,
  },
  /// Structured-clone bytes of a JS-born value (AddData fallback: the value
  /// was created inside an op, invisible to the tape). Empty bytes = plain
  /// object placeholder; the restoring runtime's re-run init refills it.
  ClonedValue {
    id: HandleId,
    bytes: Vec<u8>,
  },
  /// Positional marker: the creator SKIPPED a per-process wiring call here
  /// (a Rust-driven Function::Call with untapeable args, e.g. __setTickInfo
  /// with a TypedArray over a process-local buffer). The restoring embedder
  /// re-issues those calls live; tape entries recorded after the last one
  /// (user-phase JS) wait until the restore has completed as many.
  WiringCall,
  /// Positional marker: the creator stored its per-runtime state here
  /// (Isolate::SetData slot ≥ 1). Everything before it demonstrably ran
  /// WITHOUT embedder state on the creator, so it replays eagerly; JS after
  /// it waits for the restoring runtime's own SetData.
  StateReady,
}

// ---------------------------------------------------------------------------
// Recording state
// ---------------------------------------------------------------------------

#[derive(Default)]
pub(crate) struct Recorder {
  pub ops: Vec<TapeOp>,
  /// Arena pointer → last handle id bound to it (latest-wins).
  ptr_ids: HashMap<usize, HandleId>,
  /// JSContext pointer → id of its ContextNew entry.
  ctx_ids: HashMap<usize, HandleId>,
  next_id: HandleId,
  /// External pointer → index in CreateParams.external_references.
  ext_refs: HashMap<usize, u32>,
  /// Set when a pointer outside the ext-ref table or an argument without an
  /// id was seen: the tape cannot faithfully restore. CreateBlob fails (like
  /// V8's "Unknown external reference") instead of emitting a broken blob.
  pub incomplete: Vec<String>,
}

thread_local! {
  /// Depth of native→JS calls in flight (JS_Eval / JS_Call / module eval
  /// issued by the shim). While > 0, C-API calls originate from op callbacks
  /// whose effects the tape's JS entries already reproduce — don't record.
  static JS_DEPTH: Cell<u32> = const { Cell::new(0) };
}

pub(crate) struct JsDepthGuard;
impl JsDepthGuard {
  pub fn enter() -> JsDepthGuard {
    JS_DEPTH.with(|d| d.set(d.get() + 1));
    JsDepthGuard
  }
}
impl Drop for JsDepthGuard {
  fn drop(&mut self) {
    JS_DEPTH.with(|d| d.set(d.get() - 1));
  }
}

pub(crate) fn in_js() -> bool {
  JS_DEPTH.with(|d| d.get()) > 0
}

impl Recorder {
  pub fn new(external_references: *const isize) -> Self {
    let mut ext_refs = HashMap::new();
    if !external_references.is_null() {
      // Zero-terminated intptr_t array (V8/rusty_v8 convention).
      let mut i = 0usize;
      loop {
        let p = unsafe { *external_references.add(i) };
        if p == 0 {
          break;
        }
        ext_refs.entry(p as usize).or_insert(i as u32);
        i += 1;
        if i > 1_000_000 {
          break; // defensive: unterminated table
        }
      }
    }
    Recorder {
      ext_refs,
      ..Default::default()
    }
  }

  fn fresh_id(&mut self) -> HandleId {
    let id = self.next_id;
    self.next_id += 1;
    id
  }

  /// Bind a freshly produced handle pointer to a new id.
  pub fn produced(&mut self, ptr: *const std::ffi::c_void) -> HandleId {
    let id = self.fresh_id();
    self.ptr_ids.insert(ptr as usize, id);
    id
  }

  /// Resolve an argument handle to its id; marks the tape incomplete when the
  /// value was produced by an unrecorded call.
  pub fn arg(&mut self, ptr: *const std::ffi::c_void, what: &str) -> HandleId {
    match self.ptr_ids.get(&(ptr as usize)) {
      Some(&id) => id,
      None => {
        // Read the handle's JSValue tag for diagnosis (arena slot layout).
        let tag = unsafe { (*(ptr as *const JSValue)).tag };
        self
          .incomplete
          .push(format!("arg without id: {what} tag={tag} ({ptr:?})"));
        u32::MAX
      }
    }
  }

  /// Non-fatal arg lookup (reads): None when the value was never recorded.
  pub fn try_arg(&mut self, ptr: *const std::ffi::c_void) -> Option<HandleId> {
    self.ptr_ids.get(&(ptr as usize)).copied()
  }

  pub fn ctx_id(&mut self, ctx: *mut JSContext) -> HandleId {
    match self.ctx_ids.get(&(ctx as usize)) {
      Some(&id) => id,
      None => {
        let id = self.fresh_id();
        self.ctx_ids.insert(ctx as usize, id);
        self.ops.push(TapeOp::ContextNew { id });
        id
      }
    }
  }

  /// Bind an existing tape id to a live pointer (chained creators: values
  /// handed out by GetDataFromSnapshotOnce keep their original ids).
  pub fn bind(&mut self, ptr: *const std::ffi::c_void, id: HandleId) {
    self.ptr_ids.insert(ptr as usize, id);
  }

  /// Handle copy (Local::New / escape / Global::New): the new pointer names
  /// the same recorded value — propagate its id. No tape op is emitted.
  pub fn alias(
    &mut self,
    from: *const std::ffi::c_void,
    to: *const std::ffi::c_void,
  ) {
    if let Some(&id) = self.ptr_ids.get(&(from as usize)) {
      self.ptr_ids.insert(to as usize, id);
    }
  }

  /// Seed a chained creator from the blob it was constructed on top of: the
  /// old ops replay first at restore, so they PREFIX the new tape; fresh ids
  /// continue after the old range and the replayed contexts keep their ids.
  pub fn seed_from(
    &mut self,
    ops: &[TapeOp],
    contexts: &HashMap<HandleId, *mut JSContext>,
  ) {
    let mut max_id = 0u32;
    for op in ops {
      for id in op_ids(op) {
        if id != u32::MAX {
          max_id = max_id.max(id.saturating_add(1));
        }
      }
    }
    self.next_id = self.next_id.max(max_id);
    // Drop stale WiringCall markers: they described the live wiring calls of
    // THIS process's restore (already done). Only the markers this recorder
    // emits itself describe the wiring the next restore will perform.
    self
      .ops
      .extend(ops.iter().filter(|op| !matches!(op, TapeOp::WiringCall)).cloned());
    for (&id, &ctx) in contexts {
      self.ctx_ids.insert(ctx as usize, id);
    }
  }

  pub fn ext_ref(&mut self, ptr: usize, what: &str) -> u32 {
    match self.ext_refs.get(&ptr) {
      Some(&i) => i,
      None => {
        self
          .incomplete
          .push(format!("pointer not in external_references: {what}"));
        u32::MAX
      }
    }
  }
}

// ---------------------------------------------------------------------------
// Serialization (same length-prefixed style as snapshot.rs; magic V8XTAPE1)
// ---------------------------------------------------------------------------

pub(crate) const TAPE_MAGIC: &[u8; 8] = b"V8XTAPE1";

/// The ids an op PRODUCES (results only) — taint sources for the deferred
/// replay phase.
fn op_result_ids(op: &TapeOp) -> Vec<HandleId> {
  use TapeOp::*;
  match op {
    ContextNew { id }
    | ContextGlobal { id, .. }
    | StringNew { id, .. }
    | NumberNew { id, .. }
    | BoolNew { id, .. }
    | UndefinedNew { id }
    | NullNew { id }
    | ObjectNew { id }
    | ArrayNew { id, .. }
    | ExternalNew { id, .. }
    | FunctionNew { id, .. }
    | GetProp { id, .. }
    | GetEmbedderData { id, .. }
    | ModuleCompile { id, .. }
    | ExtrasBinding { id, .. }
    | TemplateNew { id, .. } => vec![*id],
    ScriptRun { result, .. }
    | ModuleEval { result, .. }
    | FunctionCall { result, .. } => vec![*result],
    _ => vec![],
  }
}

/// Every id an op mentions (result + argument ids) — used to advance a
/// chained creator's id counter past the seeded range.
fn op_ids(op: &TapeOp) -> Vec<HandleId> {
  use TapeOp::*;
  match op {
    ContextNew { id }
    | UndefinedNew { id }
    | NullNew { id }
    | ObjectNew { id } => vec![*id],
    ContextGlobal { id, ctx } => vec![*id, *ctx],
    StringNew { id, .. }
    | NumberNew { id, .. }
    | BoolNew { id, .. }
    | ArrayNew { id, .. }
    | ExternalNew { id, .. } => vec![*id],
    FunctionNew {
      id,
      ctx,
      data,
      name,
      ..
    } => {
      let mut v = vec![*id, *ctx];
      if let Some(d) = data {
        v.push(*d);
      }
      if let Some(n) = name {
        v.push(*n);
      }
      v
    }
    SetProp {
      ctx,
      obj,
      key,
      value,
    } => vec![*ctx, *obj, *key, *value],
    DefineProp {
      ctx,
      obj,
      key,
      value,
      ..
    } => vec![*ctx, *obj, *key, *value],
    GetProp { id, ctx, obj, key } => vec![*id, *ctx, *obj, *key],
    SetPrototype { ctx, obj, proto } => vec![*ctx, *obj, *proto],
    ScriptRun { result, ctx, .. } => vec![*result, *ctx],
    ModuleSource { .. } => vec![],
    ModuleEval { result, ctx, .. } => vec![*result, *ctx],
    AddContextData { ctx, value } => vec![*ctx, *value],
    AddIsolateData { value } => vec![*value],
    SetDefaultContext { ctx } | AddContext { ctx } => vec![*ctx],
    SetEmbedderData { ctx, value, .. } => vec![*ctx, *value],
    GetEmbedderData { id, ctx, .. } => vec![*id, *ctx],
    FunctionCall {
      result,
      ctx,
      callee,
      recv,
      args,
    } => {
      let mut v = vec![*result, *ctx, *callee, *recv];
      v.extend_from_slice(args);
      v
    }
    ModuleCompile { id, ctx, .. } => vec![*id, *ctx],
    ExtrasBinding { id, ctx } => vec![*id, *ctx],
    TemplateNew { id, .. } => vec![*id],
    StateReady | WiringCall => vec![],
    ClonedValue { id, .. } => vec![*id],
  }
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
  out.extend_from_slice(&v.to_le_bytes());
}
fn put_i32(out: &mut Vec<u8>, v: i32) {
  out.extend_from_slice(&v.to_le_bytes());
}
fn put_f64(out: &mut Vec<u8>, v: f64) {
  out.extend_from_slice(&v.to_le_bytes());
}
fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
  put_u32(out, b.len() as u32);
  out.extend_from_slice(b);
}
fn put_opt(out: &mut Vec<u8>, v: Option<HandleId>) {
  match v {
    Some(id) => {
      out.push(1);
      put_u32(out, id);
    }
    None => out.push(0),
  }
}

pub(crate) fn serialize(ops: &[TapeOp]) -> Vec<u8> {
  let mut out = Vec::new();
  out.extend_from_slice(TAPE_MAGIC);
  put_u32(&mut out, ops.len() as u32);
  for op in ops {
    match op {
      TapeOp::ContextNew { id } => {
        out.push(0);
        put_u32(&mut out, *id);
      }
      TapeOp::ContextGlobal { id, ctx } => {
        out.push(1);
        put_u32(&mut out, *id);
        put_u32(&mut out, *ctx);
      }
      TapeOp::StringNew { id, utf8 } => {
        out.push(2);
        put_u32(&mut out, *id);
        put_bytes(&mut out, utf8);
      }
      TapeOp::NumberNew { id, value } => {
        out.push(3);
        put_u32(&mut out, *id);
        put_f64(&mut out, *value);
      }
      TapeOp::BoolNew { id, value } => {
        out.push(4);
        put_u32(&mut out, *id);
        out.push(*value as u8);
      }
      TapeOp::UndefinedNew { id } => {
        out.push(5);
        put_u32(&mut out, *id);
      }
      TapeOp::NullNew { id } => {
        out.push(6);
        put_u32(&mut out, *id);
      }
      TapeOp::ObjectNew { id } => {
        out.push(7);
        put_u32(&mut out, *id);
      }
      TapeOp::ArrayNew { id, length } => {
        out.push(8);
        put_u32(&mut out, *id);
        put_u32(&mut out, *length);
      }
      TapeOp::ExternalNew { id, ext_ref } => {
        out.push(9);
        put_u32(&mut out, *id);
        put_u32(&mut out, *ext_ref);
      }
      TapeOp::FunctionNew {
        id,
        ctx,
        cb_ext_ref,
        data,
        data_ext,
        name,
        length,
      } => {
        out.push(10);
        put_u32(&mut out, *id);
        put_u32(&mut out, *ctx);
        put_u32(&mut out, *cb_ext_ref);
        put_opt(&mut out, *data);
        put_opt(&mut out, *data_ext);
        put_opt(&mut out, *name);
        put_i32(&mut out, *length);
      }
      TapeOp::SetProp {
        ctx,
        obj,
        key,
        value,
      } => {
        out.push(11);
        put_u32(&mut out, *ctx);
        put_u32(&mut out, *obj);
        put_u32(&mut out, *key);
        put_u32(&mut out, *value);
      }
      TapeOp::DefineProp {
        ctx,
        obj,
        key,
        value,
        attrs,
      } => {
        out.push(12);
        put_u32(&mut out, *ctx);
        put_u32(&mut out, *obj);
        put_u32(&mut out, *key);
        put_u32(&mut out, *value);
        put_u32(&mut out, *attrs);
      }
      TapeOp::GetProp { id, ctx, obj, key } => {
        out.push(13);
        put_u32(&mut out, *id);
        put_u32(&mut out, *ctx);
        put_u32(&mut out, *obj);
        put_u32(&mut out, *key);
      }
      TapeOp::SetPrototype { ctx, obj, proto } => {
        out.push(14);
        put_u32(&mut out, *ctx);
        put_u32(&mut out, *obj);
        put_u32(&mut out, *proto);
      }
      TapeOp::ScriptRun {
        result,
        ctx,
        bytecode,
        source,
        filename,
        eval_flags,
      } => {
        out.push(15);
        put_u32(&mut out, *result);
        put_u32(&mut out, *ctx);
        put_bytes(&mut out, bytecode);
        put_bytes(&mut out, source);
        put_bytes(&mut out, filename);
        put_i32(&mut out, *eval_flags);
      }
      TapeOp::ModuleSource { name, source } => {
        out.push(16);
        put_bytes(&mut out, name);
        put_bytes(&mut out, source);
      }
      TapeOp::ModuleEval {
        result,
        ctx,
        name,
        bytecode,
        source,
      } => {
        out.push(17);
        put_u32(&mut out, *result);
        put_u32(&mut out, *ctx);
        put_bytes(&mut out, name);
        put_bytes(&mut out, bytecode);
        put_bytes(&mut out, source);
      }
      TapeOp::AddContextData { ctx, value } => {
        out.push(18);
        put_u32(&mut out, *ctx);
        put_u32(&mut out, *value);
      }
      TapeOp::AddIsolateData { value } => {
        out.push(19);
        put_u32(&mut out, *value);
      }
      TapeOp::SetDefaultContext { ctx } => {
        out.push(20);
        put_u32(&mut out, *ctx);
      }
      TapeOp::AddContext { ctx } => {
        out.push(21);
        put_u32(&mut out, *ctx);
      }
      TapeOp::SetEmbedderData { ctx, index, value } => {
        out.push(22);
        put_u32(&mut out, *ctx);
        put_u32(&mut out, *index);
        put_u32(&mut out, *value);
      }
      TapeOp::GetEmbedderData { id, ctx, index } => {
        out.push(23);
        put_u32(&mut out, *id);
        put_u32(&mut out, *ctx);
        put_u32(&mut out, *index);
      }
      TapeOp::FunctionCall {
        result,
        ctx,
        callee,
        recv,
        args,
      } => {
        out.push(24);
        put_u32(&mut out, *result);
        put_u32(&mut out, *ctx);
        put_u32(&mut out, *callee);
        put_u32(&mut out, *recv);
        put_u32(&mut out, args.len() as u32);
        for a in args {
          put_u32(&mut out, *a);
        }
      }
      TapeOp::ModuleCompile { id, ctx, name } => {
        out.push(25);
        put_u32(&mut out, *id);
        put_u32(&mut out, *ctx);
        put_bytes(&mut out, name);
      }
      TapeOp::ExtrasBinding { id, ctx } => {
        out.push(26);
        put_u32(&mut out, *id);
        put_u32(&mut out, *ctx);
      }
      TapeOp::TemplateNew {
        id,
        cb_ext_ref,
        data_ext,
        length,
        constructable,
      } => {
        out.push(27);
        put_u32(&mut out, *id);
        put_u32(&mut out, *cb_ext_ref);
        put_opt(&mut out, *data_ext);
        put_i32(&mut out, *length);
        out.push(*constructable as u8);
      }
      TapeOp::StateReady => out.push(28),
      TapeOp::WiringCall => out.push(30),
      TapeOp::ClonedValue { id, bytes } => {
        out.push(29);
        put_u32(&mut out, *id);
        put_bytes(&mut out, bytes);
      }
    }
  }
  out
}

struct Reader<'a> {
  buf: &'a [u8],
  pos: usize,
}

impl<'a> Reader<'a> {
  fn u8(&mut self) -> Option<u8> {
    let b = *self.buf.get(self.pos)?;
    self.pos += 1;
    Some(b)
  }
  fn u32(&mut self) -> Option<u32> {
    let b = self.buf.get(self.pos..self.pos + 4)?;
    self.pos += 4;
    Some(u32::from_le_bytes(b.try_into().unwrap()))
  }
  fn i32(&mut self) -> Option<i32> {
    Some(self.u32()? as i32)
  }
  fn f64(&mut self) -> Option<f64> {
    let b = self.buf.get(self.pos..self.pos + 8)?;
    self.pos += 8;
    Some(f64::from_le_bytes(b.try_into().unwrap()))
  }
  fn bytes(&mut self) -> Option<Vec<u8>> {
    let len = self.u32()? as usize;
    let b = self.buf.get(self.pos..self.pos + len)?;
    self.pos += len;
    Some(b.to_vec())
  }
  fn opt(&mut self) -> Option<Option<HandleId>> {
    match self.u8()? {
      0 => Some(None),
      _ => Some(Some(self.u32()?)),
    }
  }
}

pub(crate) fn deserialize(bytes: &[u8]) -> Option<Vec<TapeOp>> {
  if bytes.len() < 8 || &bytes[..8] != TAPE_MAGIC {
    return None;
  }
  let mut r = Reader { buf: bytes, pos: 8 };
  let n = r.u32()?;
  let mut ops = Vec::with_capacity(n as usize);
  for _ in 0..n {
    let op = match r.u8()? {
      0 => TapeOp::ContextNew { id: r.u32()? },
      1 => TapeOp::ContextGlobal {
        id: r.u32()?,
        ctx: r.u32()?,
      },
      2 => TapeOp::StringNew {
        id: r.u32()?,
        utf8: r.bytes()?,
      },
      3 => TapeOp::NumberNew {
        id: r.u32()?,
        value: r.f64()?,
      },
      4 => TapeOp::BoolNew {
        id: r.u32()?,
        value: r.u8()? != 0,
      },
      5 => TapeOp::UndefinedNew { id: r.u32()? },
      6 => TapeOp::NullNew { id: r.u32()? },
      7 => TapeOp::ObjectNew { id: r.u32()? },
      8 => TapeOp::ArrayNew {
        id: r.u32()?,
        length: r.u32()?,
      },
      9 => TapeOp::ExternalNew {
        id: r.u32()?,
        ext_ref: r.u32()?,
      },
      10 => TapeOp::FunctionNew {
        id: r.u32()?,
        ctx: r.u32()?,
        cb_ext_ref: r.u32()?,
        data: r.opt()?,
        data_ext: r.opt()?,
        name: r.opt()?,
        length: r.i32()?,
      },
      11 => TapeOp::SetProp {
        ctx: r.u32()?,
        obj: r.u32()?,
        key: r.u32()?,
        value: r.u32()?,
      },
      12 => TapeOp::DefineProp {
        ctx: r.u32()?,
        obj: r.u32()?,
        key: r.u32()?,
        value: r.u32()?,
        attrs: r.u32()?,
      },
      13 => TapeOp::GetProp {
        id: r.u32()?,
        ctx: r.u32()?,
        obj: r.u32()?,
        key: r.u32()?,
      },
      14 => TapeOp::SetPrototype {
        ctx: r.u32()?,
        obj: r.u32()?,
        proto: r.u32()?,
      },
      15 => TapeOp::ScriptRun {
        result: r.u32()?,
        ctx: r.u32()?,
        bytecode: r.bytes()?,
        source: r.bytes()?,
        filename: r.bytes()?,
        eval_flags: r.i32()?,
      },
      16 => TapeOp::ModuleSource {
        name: r.bytes()?,
        source: r.bytes()?,
      },
      17 => TapeOp::ModuleEval {
        result: r.u32()?,
        ctx: r.u32()?,
        name: r.bytes()?,
        bytecode: r.bytes()?,
        source: r.bytes()?,
      },
      18 => TapeOp::AddContextData {
        ctx: r.u32()?,
        value: r.u32()?,
      },
      19 => TapeOp::AddIsolateData { value: r.u32()? },
      20 => TapeOp::SetDefaultContext { ctx: r.u32()? },
      21 => TapeOp::AddContext { ctx: r.u32()? },
      22 => TapeOp::SetEmbedderData {
        ctx: r.u32()?,
        index: r.u32()?,
        value: r.u32()?,
      },
      23 => TapeOp::GetEmbedderData {
        id: r.u32()?,
        ctx: r.u32()?,
        index: r.u32()?,
      },
      24 => {
        let result = r.u32()?;
        let ctx = r.u32()?;
        let callee = r.u32()?;
        let recv = r.u32()?;
        let n = r.u32()?;
        let mut args = Vec::with_capacity(n as usize);
        for _ in 0..n {
          args.push(r.u32()?);
        }
        TapeOp::FunctionCall {
          result,
          ctx,
          callee,
          recv,
          args,
        }
      }
      25 => TapeOp::ModuleCompile {
        id: r.u32()?,
        ctx: r.u32()?,
        name: r.bytes()?,
      },
      26 => TapeOp::ExtrasBinding {
        id: r.u32()?,
        ctx: r.u32()?,
      },
      27 => TapeOp::TemplateNew {
        id: r.u32()?,
        cb_ext_ref: r.u32()?,
        data_ext: r.opt()?,
        length: r.i32()?,
        constructable: r.u8()? != 0,
      },
      28 => TapeOp::StateReady,
      30 => TapeOp::WiringCall,
      29 => TapeOp::ClonedValue {
        id: r.u32()?,
        bytes: r.bytes()?,
      },
      _ => return None,
    };
    ops.push(op);
  }
  Some(ops)
}

// ---------------------------------------------------------------------------
// Recording plumbing used by the shim hooks
// ---------------------------------------------------------------------------

use super::core::current_iso;
use super::core::iso_state;

/// Run `f` against the current isolate's tape recorder, if recording is
/// active and control is outside JS. One-liner hook helper:
/// `capi_tape::rec(|r| { ... });`
#[inline]
pub(crate) fn rec<F: FnOnce(&mut Recorder)>(f: F) {
  if in_js() {
    return;
  }
  let iso = current_iso();
  if iso.is_null() {
    return;
  }
  if let Some(r) = iso_state(iso).tape_rec.as_deref_mut() {
    f(r);
  }
}

/// True when the current isolate records a tape (and we are outside JS) —
/// for hooks that must compute extra data (e.g. bytecode) only when needed.
#[inline]
pub(crate) fn recording() -> bool {
  if in_js() {
    return false;
  }
  let iso = current_iso();
  !iso.is_null() && iso_state(iso).tape_rec.is_some()
}

// ---------------------------------------------------------------------------
// Replay
// ---------------------------------------------------------------------------

/// Restore-side state: parsed tape + materialized handles. Lives on the
/// isolate; the tape replays once, when the embedder asks for the first
/// context (`Context::new` or `Context::from_snapshot`).
pub(crate) struct TapeRestore {
  pub ops: Vec<TapeOp>,
  /// Copy kept after replay for chained-creator seeding.
  pub seeded_ops: Vec<TapeOp>,
  /// JS-executing entries (and everything data-dependent on their results),
  /// deferred until the embedder stores its state pointer (SetData): ops
  /// fired by replayed extension code read that state, and real V8 executes
  /// nothing at restore so embedders never guard for it.
  pub deferred: Vec<TapeOp>,
  pub deferred_done: bool,
  /// User-phase entries (after the creator's last WiringCall) + how many
  /// live wiring calls must complete first.
  pub deferred_user: Vec<TapeOp>,
  pub wiring_pending: u32,
  pub user_done: bool,
  pub replayed: bool,
  /// id → owned (+1) JSValue. Everything stays alive until isolate dispose:
  /// GetDataFromSnapshotOnce may be called long after replay.
  pub handles: HashMap<HandleId, (JSValue, *mut JSContext)>,
  pub contexts: HashMap<HandleId, *mut JSContext>,
  pub default_ctx: Option<HandleId>,
  pub added: Vec<HandleId>,
  /// Once-consumable AddData values, in AddData order.
  pub ctx_data: HashMap<HandleId, Vec<Option<HandleId>>>,
  pub iso_data: Vec<Option<HandleId>>,
  /// Restore-process external-reference table (index → pointer).
  pub ext_table: Vec<usize>,
  /// Tape id → module WRAPPER handle pointer (module identity in deno_core's
  /// side tables is the handle pointer itself, so GetDataFromSnapshotOnce
  /// must return the exact wrapper, not a re-interned copy).
  pub module_handles: HashMap<HandleId, usize>,
  /// Tape id → replayed FunctionTemplate handle pointer (shim-side struct).
  pub template_handles: HashMap<HandleId, usize>,
  /// First materialized tape context — deterministic home for value
  /// creation and by-name module lookups (HashMap order is random).
  pub primary_ctx: *mut JSContext,
  /// Placeholder fixups: cells (arena handles / Global boxes — both are
  /// Box<JSValue>) handed out for values whose producing tape entry is still
  /// deferred. When the deferred phase materializes the value, every
  /// registered cell is patched in place, so Globals the embedder took early
  /// see the real value by the time it types-checks them.
  /// (pending id, cell ptr, placeholder object ptr). The object ptr guards
  /// against patching an arena slot that was popped and RECYCLED before the
  /// deferred phase ran — only cells still holding the placeholder patch.
  pub fixups: Vec<(HandleId, usize, usize)>,
  /// Cell → (pending id, placeholder object ptr).
  pub fixup_cells: HashMap<usize, (HandleId, usize)>,
}

impl TapeRestore {
  pub fn new(ops: Vec<TapeOp>, external_references: *const isize) -> Self {
    let mut ext_table = Vec::new();
    if !external_references.is_null() {
      let mut i = 0usize;
      loop {
        let p = unsafe { *external_references.add(i) };
        if p == 0 {
          break;
        }
        ext_table.push(p as usize);
        i += 1;
        if i > 1_000_000 {
          break;
        }
      }
    }
    TapeRestore {
      seeded_ops: ops.clone(),
      ops,
      deferred: Vec::new(),
      deferred_done: false,
      deferred_user: Vec::new(),
      wiring_pending: 0,
      user_done: false,
      module_handles: HashMap::new(),
      template_handles: HashMap::new(),
      primary_ctx: std::ptr::null_mut(),
      fixups: Vec::new(),
      fixup_cells: HashMap::new(),
      replayed: false,
      handles: HashMap::new(),
      contexts: HashMap::new(),
      default_ctx: None,
      added: Vec::new(),
      ctx_data: HashMap::new(),
      iso_data: Vec::new(),
      ext_table,
    }
  }

  fn ext_ptr(&self, idx: u32) -> *mut std::ffi::c_void {
    self.ext_table.get(idx as usize).copied().unwrap_or(0)
      as *mut std::ffi::c_void
  }
}

fn dbg_on() -> bool {
  std::env::var_os("QJS_DEBUG_TAPE").is_some()
}

/// Materialize the whole tape against `iso`. The FIRST ContextNew claims the
/// isolate's bootstrap context (mirrors `v8__Context__New` semantics); later
/// ones get fresh JSContexts.
pub(crate) fn replay(iso: *mut crate::RealIsolate) {
  let st = iso_state(iso);
  let Some(restore) = st.tape_restore.as_deref_mut() else {
    return;
  };
  if restore.replayed {
    return;
  }
  restore.replayed = true;
  let all_ops = std::mem::take(&mut restore.ops);
  // Positional split at the StateReady marker (see the op's docs): before it
  // everything replays eagerly, after it JS-executing entries wait for the
  // restoring runtime's own state install. Pure C ops and data-slot
  // registration always run eagerly — the latter keeps AddData indices in
  // tape order; reads of not-yet-materialized values get placeholders.
  let mut ops = Vec::new();
  let mut deferred = Vec::new();
  let mut after_marker = false;
  for op in all_ops {
    if matches!(op, TapeOp::StateReady) {
      after_marker = true;
      continue;
    }
    let is_js = matches!(
      op,
      TapeOp::ScriptRun { .. }
        | TapeOp::ModuleEval { .. }
        | TapeOp::FunctionCall { .. }
    );
    if after_marker && is_js {
      deferred.push(op);
    } else if after_marker
      && !matches!(
        op,
        TapeOp::AddContextData { .. }
          | TapeOp::AddIsolateData { .. }
          | TapeOp::SetDefaultContext { .. }
          | TapeOp::AddContext { .. }
          | TapeOp::ContextNew { .. }
          | TapeOp::ContextGlobal { .. }
          | TapeOp::ObjectNew { .. }
          | TapeOp::ArrayNew { .. }
          | TapeOp::FunctionNew { .. }
          | TapeOp::ExtrasBinding { .. }
          | TapeOp::ModuleSource { .. }
          | TapeOp::ModuleCompile { .. }
          | TapeOp::TemplateNew { .. }
          | TapeOp::StringNew { .. }
          | TapeOp::NumberNew { .. }
          | TapeOp::BoolNew { .. }
          | TapeOp::UndefinedNew { .. }
          | TapeOp::NullNew { .. }
          | TapeOp::ExternalNew { .. }
          | TapeOp::ClonedValue { .. }
      )
    {
      // Post-marker mutations/reads may reference post-marker JS results:
      // keep them ordered WITH the JS.
      deferred.push(op);
    } else {
      ops.push(op);
    }
  }
  // P1/P2 split: user-phase entries follow the creator's last WiringCall.
  let wiring_total =
    deferred.iter().filter(|o| matches!(o, TapeOp::WiringCall)).count() as u32;
  let mut deferred_user = Vec::new();
  if wiring_total > 0 {
    let last_wiring = deferred
      .iter()
      .rposition(|o| matches!(o, TapeOp::WiringCall))
      .unwrap();
    deferred_user = deferred.split_off(last_wiring + 1);
  }
  restore.wiring_pending = wiring_total;
  restore.deferred_user = deferred_user;
  restore.deferred = deferred;
  let rt = st.rt;

  let mut first_ctx = true;
  for (i, op) in ops.iter().enumerate() {
    let _ = i;
    match op {
      TapeOp::ContextNew { id } => {
        let ctx = if first_ctx && !st.main_ctx_claimed {
          st.main_ctx_claimed = true;
          super::core::install_default_globals(st.ctx);
          st.ctx
        } else {
          let c = unsafe { JS_NewContext(rt) };
          if std::env::var_os("QJS_NO_WASM").is_none() {
            super::wasm::install_webassembly(c);
          }
          st.extra_contexts.push(c);
          super::core::install_default_globals(c);
          c
        };
        first_ctx = false;
        let restore = st.tape_restore.as_deref_mut().unwrap();
        if restore.primary_ctx.is_null() {
          restore.primary_ctx = ctx;
        }
        restore.contexts.insert(*id, ctx);
      }
      _ => {
        // All other ops need the restore struct; split borrow.
        let restore = st.tape_restore.as_deref_mut().unwrap();
        replay_one(rt, restore, op);
      }
    }
  }
  if dbg_on() {
    let restore = st.tape_restore.as_deref_mut().unwrap();
    for (id, &c) in &restore.contexts {
      let probe =
        c"[typeof globalThis.__bootstrap, typeof globalThis.Deno].join()";
      let v = unsafe {
        JS_Eval(
          c,
          probe.as_ptr(),
          62,
          c"<probe>".as_ptr(),
          JS_EVAL_TYPE_GLOBAL,
        )
      };
      let mut l = 0usize;
      let cs = unsafe { JS_ToCStringLen(c, &mut l, v) };
      if !cs.is_null() {
        let b = unsafe { std::slice::from_raw_parts(cs as *const u8, l) };
        eprintln!(
          "[qjs tape] eager probe ctx id={id} ptr={c:?}: {}",
          String::from_utf8_lossy(b)
        );
        unsafe { JS_FreeCString(c, cs) };
      }
      unsafe { JS_FreeValue(c, v) };
    }
    eprintln!(
      "[qjs tape] replayed: {} handles, {} ctx, default={:?}, added={}, modules={}, tmpls={}, ctx_data={:?}",
      restore.handles.len(),
      restore.contexts.len(),
      restore.default_ctx,
      restore.added.len(),
      restore.module_handles.len(),
      restore.template_handles.len(),
      restore
        .ctx_data
        .iter()
        .map(|(k, v)| (*k, v.len()))
        .collect::<Vec<_>>()
    );
  }
}

/// Key handle → property atom (symbol/int keys round-trip via JS_ValueToAtom).
unsafe fn atom_of(
  ctx: *mut JSContext,
  handles: &HashMap<HandleId, (JSValue, *mut JSContext)>,
  key: HandleId,
) -> JSAtom {
  match handles.get(&key) {
    Some((v, _)) => unsafe { JS_ValueToAtom(ctx, *v) },
    None => 0,
  }
}

/// Run the deferred (JS-executing) phase. Triggered by the embedder's first
/// meaningful Isolate::SetData — deno_core stores its JsRuntimeState right
/// after context/bindings setup and before any code could need it.
/// True when a restored isolate still has JS-executing tape entries pending.
pub(crate) fn has_pending_deferred(iso: *mut crate::RealIsolate) -> bool {
  if iso.is_null() {
    return false;
  }
  iso_state(iso)
    .tape_restore
    .as_deref()
    .map(|r| r.replayed && !r.deferred_done)
    .unwrap_or(false)
}

/// Register a placeholder cell for a not-yet-materialized tape value.
pub(crate) fn register_fixup(
  iso: *mut crate::RealIsolate,
  hid: HandleId,
  cell: *const std::ffi::c_void,
) {
  let obj_ptr = unsafe { (*(cell as *const JSValue)).u.ptr } as usize;
  if let Some(r) = iso_state(iso).tape_restore.as_deref_mut() {
    r.fixups.push((hid, cell as usize, obj_ptr));
    r.fixup_cells.insert(cell as usize, (hid, obj_ptr));
  }
}

/// A handle copy (Local::New / Global::New) of a placeholder must be patched
/// too — chain the new cell onto the same pending id.
pub(crate) fn propagate_fixup(
  iso: *mut crate::RealIsolate,
  from: *const std::ffi::c_void,
  to: *const std::ffi::c_void,
) {
  if iso.is_null() {
    return;
  }
  let Some(r) = iso_state(iso).tape_restore.as_deref_mut() else {
    return;
  };
  if let Some(&(hid, obj)) = r.fixup_cells.get(&(from as usize)) {
    r.fixups.push((hid, to as usize, obj));
    r.fixup_cells.insert(to as usize, (hid, obj));
  }
}

pub(crate) fn replay_deferred(iso: *mut crate::RealIsolate) {
  let st = iso_state(iso);
  let Some(restore) = st.tape_restore.as_deref_mut() else {
    return;
  };
  if !restore.replayed || restore.deferred_done {
    return;
  }
  restore.deferred_done = true;
  let rt = st.rt;
  let ops = std::mem::take(&mut restore.deferred);
  if dbg_on() {
    eprintln!(
      "[qjs tape] deferred phase START ({} ops), backtrace:\n{}",
      ops.len(),
      std::backtrace::Backtrace::force_capture()
    );
  }
  for (i, op) in ops.iter().enumerate() {
    if matches!(op, TapeOp::ContextNew { .. }) {
      continue;
    }
    if dbg_on() {
      let kind = format!("{op:?}");
      eprintln!("[qjs tape] deferred[{i}] {}", &kind[..kind.len().min(90)]);
    }
    let restore = iso_state(iso).tape_restore.as_deref_mut().unwrap();
    replay_one(rt, restore, op);
  }
  finish_phase(iso);
  if dbg_on() {
    eprintln!("[qjs tape] deferred phase done ({} ops)", ops.len());
  }
  // No per-process wiring recorded: the user phase has nothing to wait for.
  let st = iso_state(iso);
  if let Some(r) = st.tape_restore.as_deref_mut() {
    if r.wiring_pending == 0 && !r.user_done {
      r.user_done = true;
      let user_ops = std::mem::take(&mut r.deferred_user);
      if dbg_on() {
        eprintln!("[qjs tape] user phase (tail) START ({} ops)", user_ops.len());
      }
      for (i, op) in user_ops.iter().enumerate() {
        if matches!(op, TapeOp::ContextNew { .. }) {
          continue;
        }
        if dbg_on() {
          let kind = format!("{op:?}");
          eprintln!("[qjs tape] user[{i}] {}", &kind[..kind.len().min(90)]);
        }
        let restore = iso_state(iso).tape_restore.as_deref_mut().unwrap();
        replay_one(rt, restore, op);
      }
      finish_phase(iso);
    }
  }
}

/// Post-phase bookkeeping, run after BOTH the deferred (P1) and user (P2)
/// phases: lift module wrapper defs/statuses to engine truth and patch any
/// placeholder cells whose values have materialized. Fixups whose values are
/// still pending stay queued for the next phase.
fn finish_phase(iso: *mut crate::RealIsolate) {
  // Module wrappers were created before their ModuleEval entries ran: pull
  // their defs/status up to engine truth.
  {
    let st0 = iso_state(iso);
    let ctx0 = st0
      .tape_restore
      .as_deref()
      .map(|r| r.primary_ctx)
      .filter(|c| !c.is_null())
      .unwrap_or(st0.ctx);
    let wrappers: Vec<usize> = iso_state(iso)
      .tape_restore
      .as_deref()
      .map(|r| r.module_handles.values().copied().collect())
      .unwrap_or_default();
    for w in wrappers {
      super::module::refresh_tape_module_state(ctx0, w as *const crate::Module);
    }
  }
  // Patch every placeholder cell whose value now exists.
  let st = iso_state(iso);
  if let Some(restore) = st.tape_restore.as_deref_mut() {
    let fixups = std::mem::take(&mut restore.fixups);
    let mut kept = Vec::new();
    let mut patched = 0usize;
    for (hid, cell, expected_obj) in fixups {
      if let Some(&(v, vctx)) = restore.handles.get(&hid) {
        unsafe {
          let slot = cell as *mut JSValue;
          // Skip recycled cells: only patch if the slot still holds OUR
          // placeholder object.
          if ((*slot).u.ptr as usize) != expected_obj {
            continue;
          }
          let old = *slot;
          *slot = JS_DupValue(vctx, v);
          JS_FreeValue(vctx, old);
        }
        patched += 1;
      } else {
        kept.push((hid, cell, expected_obj));
      }
    }
    restore.fixups = kept;
    if dbg_on() && patched > 0 {
      eprintln!("[qjs tape] patched {patched} placeholder cells");
    }
  }
}

/// A live Rust-driven Function::Call completed on a tape-restored isolate:
/// count down the creator's recorded wiring calls; at zero, the heap has all
/// per-process buffers wired and the user-phase tape can run.
pub(crate) fn on_wiring_call_done(iso: *mut crate::RealIsolate) {
  let st = iso_state(iso);
  let Some(restore) = st.tape_restore.as_deref_mut() else {
    return;
  };
  if !restore.deferred_done || restore.user_done {
    return;
  }
  if restore.wiring_pending > 0 {
    restore.wiring_pending -= 1;
  }
  if restore.wiring_pending > 0 {
    return;
  }
  restore.user_done = true;
  let rt = st.rt;
  let ops = std::mem::take(
    &mut iso_state(iso).tape_restore.as_deref_mut().unwrap().deferred_user,
  );
  if dbg_on() {
    eprintln!("[qjs tape] user phase START ({} ops)", ops.len());
  }
  for (i, op) in ops.iter().enumerate() {
    if matches!(op, TapeOp::ContextNew { .. }) {
      continue;
    }
    if dbg_on() {
      let kind = format!("{op:?}");
      eprintln!("[qjs tape] user[{i}] {}", &kind[..kind.len().min(90)]);
    }
    let restore = iso_state(iso).tape_restore.as_deref_mut().unwrap();
    replay_one(rt, restore, op);
  }
  finish_phase(iso);
}

fn replay_one(rt: *mut JSRuntime, r: &mut TapeRestore, op: &TapeOp) {
  let _ = rt;
  // Resolve a context id (fallback: default context).
  macro_rules! ctxof {
    ($id:expr) => {
      match r.contexts.get($id) {
        Some(&c) => c,
        None => {
          if dbg_on() {
            eprintln!("[qjs tape] missing ctx id {}", $id);
          }
          return;
        }
      }
    };
  }
  macro_rules! val {
    ($id:expr) => {
      match r.handles.get($id) {
        Some(&(v, _)) => v,
        None => {
          if dbg_on() {
            eprintln!("[qjs tape] missing handle id {}", $id);
          }
          return;
        }
      }
    };
  }

  match op {
    TapeOp::ContextNew { .. } => unreachable!("handled by replay()"),
    TapeOp::ContextGlobal { id, ctx } => {
      let c = ctxof!(ctx);
      let g = unsafe { JS_GetGlobalObject(c) };
      r.handles.insert(*id, (g, c));
    }
    TapeOp::StringNew { id, utf8 } => {
      let c = r.primary_ctx;
      if c.is_null() {
        return;
      }
      let v = unsafe {
        JS_NewStringLen(
          c,
          utf8.as_ptr() as *const std::os::raw::c_char,
          utf8.len(),
        )
      };
      r.handles.insert(*id, (v, c));
    }
    TapeOp::NumberNew { id, value } => {
      let c = r.primary_ctx;
      let v = unsafe { JS_NewFloat64(c, *value) };
      r.handles.insert(*id, (v, c));
    }
    TapeOp::BoolNew { id, value } => {
      let c = r.primary_ctx;
      let v = unsafe { JS_NewBool(c, *value as i32) };
      r.handles.insert(*id, (v, c));
    }
    TapeOp::UndefinedNew { id } => {
      let c = r.primary_ctx;
      r.handles.insert(*id, (jsv_undefined(), c));
    }
    TapeOp::NullNew { id } => {
      let c = r.primary_ctx;
      r.handles.insert(*id, (jsv_null(), c));
    }
    TapeOp::ObjectNew { id } => {
      let c = r.primary_ctx;
      let v = unsafe { JS_NewObject(c) };
      r.handles.insert(*id, (v, c));
    }
    TapeOp::ArrayNew { id, length } => {
      let c = r.primary_ctx;
      let v = unsafe { JS_NewArray(c) };
      if *length > 0 {
        let lv = unsafe { JS_NewFloat64(c, *length as f64) };
        let atom = unsafe { JS_NewAtom(c, c"length".as_ptr()) };
        unsafe { JS_SetProperty(c, v, atom, lv) };
        unsafe { JS_FreeAtom(c, atom) };
      }
      r.handles.insert(*id, (v, c));
    }
    TapeOp::ExternalNew { id, ext_ref } => {
      let ptr = r.ext_ptr(*ext_ref);
      let c = r.primary_ctx;
      let v = super::function::make_external_jsvalue(current_iso(), c, ptr);
      r.handles.insert(*id, (v, c));
    }
    TapeOp::FunctionNew {
      id,
      ctx,
      cb_ext_ref,
      data,
      data_ext,
      name,
      length,
    } => {
      let c = ctxof!(ctx);
      let cb_ptr = r.ext_ptr(*cb_ext_ref);
      if cb_ptr.is_null() {
        if dbg_on() {
          eprintln!("[qjs tape] FunctionNew: unresolved ext ref {cb_ext_ref}");
        }
        return;
      }
      let cb: crate::FunctionCallback = unsafe { std::mem::transmute(cb_ptr) };
      let data_v = match (data, data_ext) {
        (Some(d), _) => val!(d),
        (None, Some(e)) => super::function::make_external_jsvalue(
          current_iso(),
          c,
          r.ext_ptr(*e),
        ),
        (None, None) => jsv_undefined(),
      };
      let f = unsafe {
        super::function::make_function_len(c, cb, data_v, *length, true)
      };
      if let Some(n) = name {
        let nv = val!(n);
        let atom = unsafe { JS_NewAtom(c, c"name".as_ptr()) };
        let dup = unsafe { JS_DupValue(c, nv) };
        unsafe {
          JS_DefinePropertyValue(
            c, f, atom, dup, 0, /* not writable/enum */
          )
        };
        unsafe { JS_FreeAtom(c, atom) };
      }
      r.handles.insert(*id, (f, c));
    }
    TapeOp::SetProp {
      ctx,
      obj,
      key,
      value,
    } => {
      let c = ctxof!(ctx);
      let o = val!(obj);
      let v = val!(value);
      let atom = unsafe { atom_of(c, &r.handles, *key) };
      if atom != 0 {
        let dup = unsafe { JS_DupValue(c, v) };
        unsafe { JS_SetProperty(c, o, atom, dup) };
        unsafe { JS_FreeAtom(c, atom) };
      }
    }
    TapeOp::DefineProp {
      ctx,
      obj,
      key,
      value,
      attrs,
    } => {
      let c = ctxof!(ctx);
      let o = val!(obj);
      let v = val!(value);
      let atom = unsafe { atom_of(c, &r.handles, *key) };
      if atom != 0 {
        // v8::PropertyAttribute: 1=ReadOnly, 2=DontEnum, 4=DontDelete.
        let mut flags = 0;
        if attrs & 1 == 0 {
          flags |= JS_PROP_WRITABLE;
        }
        if attrs & 2 == 0 {
          flags |= JS_PROP_ENUMERABLE;
        }
        if attrs & 4 == 0 {
          flags |= JS_PROP_CONFIGURABLE;
        }
        let dup = unsafe { JS_DupValue(c, v) };
        unsafe { JS_DefinePropertyValue(c, o, atom, dup, flags) };
        unsafe { JS_FreeAtom(c, atom) };
      }
    }
    TapeOp::GetProp { id, ctx, obj, key } => {
      let c = ctxof!(ctx);
      let o = val!(obj);
      let atom = unsafe { atom_of(c, &r.handles, *key) };
      if atom != 0 {
        let v = unsafe { JS_GetProperty(c, o, atom) };
        unsafe { JS_FreeAtom(c, atom) };
        r.handles.insert(*id, (v, c));
      }
    }
    TapeOp::SetPrototype { ctx, obj, proto } => {
      let c = ctxof!(ctx);
      let o = val!(obj);
      let p = val!(proto);
      unsafe { JS_SetPrototype(c, o, p) };
    }
    TapeOp::ScriptRun {
      result,
      ctx,
      bytecode,
      source,
      filename,
      eval_flags,
    } => {
      let c = ctxof!(ctx);
      super::core::push_entered_ctx(current_iso(), c);
      let mut v = JSValue {
        u: JSValueUnion { int32: 0 },
        tag: JS_TAG_UNINITIALIZED,
      };
      if !bytecode.is_empty() {
        let obj =
          unsafe { JS_ReadObject(c, bytecode.as_ptr(), bytecode.len(), 1) };
        if obj.tag == JS_TAG_EXCEPTION {
          let e = unsafe { JS_GetException(c) };
          unsafe { JS_FreeValue(c, e) };
        } else {
          v = unsafe { JS_EvalFunction(c, obj) };
        }
      }
      if v.tag == JS_TAG_UNINITIALIZED {
        // Bytecode-version mismatch fallback: parse the kept source.
        let mut src = source.clone();
        src.push(0);
        let mut fname = filename.clone();
        fname.push(0);
        v = unsafe {
          JS_Eval(
            c,
            src.as_ptr() as *const std::os::raw::c_char,
            src.len() - 1,
            fname.as_ptr() as *const std::os::raw::c_char,
            *eval_flags,
          )
        };
      }
      super::core::pop_entered_ctx(current_iso());
      if v.tag == JS_TAG_EXCEPTION {
        if dbg_on() {
          let e = unsafe { JS_GetException(c) };
          let mut l = 0usize;
          let s = unsafe { JS_ToCStringLen(c, &mut l, e) };
          if !s.is_null() {
            let b = unsafe { std::slice::from_raw_parts(s as *const u8, l) };
            let head: String =
              String::from_utf8_lossy(source).chars().take(120).collect();
            eprintln!(
              "[qjs tape] ScriptRun(ctx={ctx} c={c:?}) threw: {} :: src={head:?}",
              String::from_utf8_lossy(b)
            );
            unsafe {
              let stk = JS_GetPropertyStr(c, e, c"stack".as_ptr());
              let mut sl = 0usize;
              let ss = JS_ToCStringLen(c, &mut sl, stk);
              if !ss.is_null() {
                let sb = std::slice::from_raw_parts(ss as *const u8, sl);
                eprintln!("[qjs tape] stack: {}", String::from_utf8_lossy(sb));
                JS_FreeCString(c, ss);
              }
              JS_FreeValue(c, stk);
            }
            unsafe { JS_FreeCString(c, s) };
          }
          unsafe { JS_FreeValue(c, e) };
        } else {
          let e = unsafe { JS_GetException(c) };
          unsafe { JS_FreeValue(c, e) };
        }
        return;
      }
      r.handles.insert(*result, (v, c));
    }
    TapeOp::ModuleSource { name, source } => {
      let n = String::from_utf8_lossy(name);
      let s = String::from_utf8_lossy(source);
      super::module::register_module_source(&n, &s);
    }
    TapeOp::ModuleEval {
      result,
      ctx,
      name,
      bytecode,
      source,
    } => {
      let c = ctxof!(ctx);
      super::core::push_entered_ctx(current_iso(), c);
      let mut v = JSValue {
        u: JSValueUnion { int32: 0 },
        tag: JS_TAG_UNINITIALIZED,
      };
      if !bytecode.is_empty() {
        let obj =
          unsafe { JS_ReadObject(c, bytecode.as_ptr(), bytecode.len(), 1) };
        if obj.tag == JS_TAG_EXCEPTION {
          let e = unsafe { JS_GetException(c) };
          unsafe { JS_FreeValue(c, e) };
        } else {
          v = unsafe { JS_EvalFunction(c, obj) };
        }
      }
      if v.tag == JS_TAG_UNINITIALIZED {
        let mut src = source.to_vec();
        src.push(0);
        let mut fname = name.to_vec();
        fname.push(0);
        v = unsafe {
          JS_Eval(
            c,
            src.as_ptr() as *const std::os::raw::c_char,
            src.len() - 1,
            fname.as_ptr() as *const std::os::raw::c_char,
            JS_EVAL_TYPE_MODULE,
          )
        };
      }
      super::core::pop_entered_ctx(current_iso());
      if v.tag == JS_TAG_EXCEPTION {
        let e = unsafe { JS_GetException(c) };
        if dbg_on() {
          let mut l = 0usize;
          let s = unsafe { JS_ToCStringLen(c, &mut l, e) };
          if !s.is_null() {
            let b = unsafe { std::slice::from_raw_parts(s as *const u8, l) };
            eprintln!(
              "[qjs tape] ModuleEval {} threw: {}",
              String::from_utf8_lossy(name),
              String::from_utf8_lossy(b)
            );
            unsafe { JS_FreeCString(c, s) };
          }
        }
        unsafe { JS_FreeValue(c, e) };
        return;
      }
      r.handles.insert(*result, (v, c));
    }
    TapeOp::AddContextData { ctx, value } => {
      r.ctx_data.entry(*ctx).or_default().push(Some(*value));
    }
    TapeOp::AddIsolateData { value } => {
      r.iso_data.push(Some(*value));
    }
    TapeOp::SetDefaultContext { ctx } => {
      r.default_ctx = Some(*ctx);
    }
    TapeOp::AddContext { ctx } => {
      r.added.push(*ctx);
    }
    TapeOp::SetEmbedderData { ctx, index, value } => {
      let c = ctxof!(ctx);
      let v = val!(value);
      super::misc::set_embedder_data_raw(c, *index as usize, v);
    }
    TapeOp::GetEmbedderData { id, ctx, index } => {
      let c = ctxof!(ctx);
      let slots = super::misc::embedder_data_snapshot(c);
      let v = slots
        .get(*index as usize)
        .copied()
        .flatten()
        .unwrap_or(jsv_undefined());
      let dup = unsafe { JS_DupValue(c, v) };
      r.handles.insert(*id, (dup, c));
    }
    TapeOp::FunctionCall {
      result,
      ctx,
      callee,
      recv,
      args,
    } => {
      let c = ctxof!(ctx);
      let f = val!(callee);
      let this = val!(recv);
      let mut argv: Vec<JSValue> = Vec::with_capacity(args.len());
      for a in args {
        argv.push(val!(a));
      }
      super::core::push_entered_ctx(current_iso(), c);
      let v =
        unsafe { JS_Call(c, f, this, argv.len() as i32, argv.as_mut_ptr()) };
      super::core::pop_entered_ctx(current_iso());
      if v.tag == JS_TAG_EXCEPTION {
        if dbg_on() {
          let e = unsafe { JS_GetException(c) };
          let mut l = 0usize;
          let cs = unsafe { JS_ToCStringLen(c, &mut l, e) };
          if !cs.is_null() {
            let b = unsafe { std::slice::from_raw_parts(cs as *const u8, l) };
            eprintln!(
              "[qjs tape] FunctionCall threw: {}",
              String::from_utf8_lossy(b)
            );
            unsafe { JS_FreeCString(c, cs) };
          }
          unsafe { JS_FreeValue(c, e) };
        } else {
          let e = unsafe { JS_GetException(c) };
          unsafe { JS_FreeValue(c, e) };
        }
        return;
      }
      r.handles.insert(*result, (v, c));
    }
    TapeOp::ModuleCompile { id, ctx, name } => {
      let c = ctxof!(ctx);
      let n = String::from_utf8_lossy(name).into_owned();
      let h = super::module::tape_make_module_handle(c, &n);
      if !h.is_null() {
        // The wrapper handle IS the identity deno_core keeps; hold a dup of
        // its JSValue so the id resolves like any other.
        let v = unsafe { JS_DupValue(c, super::core::jsval_of(h)) };
        r.handles.insert(*id, (v, c));
        r.module_handles.insert(*id, h as usize);
      }
    }
    TapeOp::ExtrasBinding { id, ctx } => {
      let c = ctxof!(ctx);
      let h = super::isolate::extras_binding_for_ctx(c);
      if !h.is_null() {
        let v = unsafe { JS_DupValue(c, super::core::jsval_of(h)) };
        r.handles.insert(*id, (v, c));
      }
    }
    TapeOp::StateReady => {}
    TapeOp::WiringCall => {}
    TapeOp::ClonedValue { id, bytes } => {
      let c = r.primary_ctx;
      if c.is_null() {
        return;
      }
      let v = if bytes.is_empty() {
        unsafe { JS_NewObject(c) }
      } else {
        let read = unsafe { JS_ReadObject(c, bytes.as_ptr(), bytes.len(), 0) };
        if read.tag == JS_TAG_EXCEPTION {
          let e = unsafe { JS_GetException(c) };
          unsafe { JS_FreeValue(c, e) };
          unsafe { JS_NewObject(c) }
        } else {
          read
        }
      };
      r.handles.insert(*id, (v, c));
    }
    TapeOp::TemplateNew {
      id,
      cb_ext_ref,
      data_ext,
      length,
      constructable,
    } => {
      let cb_ptr = r.ext_ptr(*cb_ext_ref);
      if cb_ptr.is_null() {
        if dbg_on() {
          eprintln!("[qjs tape] TemplateNew: unresolved ext ref {cb_ext_ref}");
        }
        return;
      }
      let data_ptr = data_ext.map(|e| r.ext_ptr(e));
      let h = super::function::tape_make_template(
        unsafe {
          std::mem::transmute::<*mut std::ffi::c_void, crate::FunctionCallback>(
            cb_ptr,
          )
        },
        data_ptr,
        *length,
        *constructable,
      );
      r.template_handles.insert(*id, h as usize);
    }
  }
}
