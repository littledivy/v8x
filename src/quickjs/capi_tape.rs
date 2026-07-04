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
        self
          .incomplete
          .push(format!("arg without id: {what} ({ptr:?})"));
        u32::MAX
      }
    }
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
        name,
        length,
      } => {
        out.push(10);
        put_u32(&mut out, *id);
        put_u32(&mut out, *ctx);
        put_u32(&mut out, *cb_ext_ref);
        put_opt(&mut out, *data);
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
      _ => return None,
    };
    ops.push(op);
  }
  Some(ops)
}
