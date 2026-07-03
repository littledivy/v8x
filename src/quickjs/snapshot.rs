//! Replay-based snapshot support for the QuickJS backend.
//!
//! V8 snapshots serialize the heap after running init JS; QuickJS has no heap
//! serializer, so we polyfill with a *replay tape*: while a `SnapshotCreator`
//! isolate is live, every successfully executed script and evaluated module is
//! recorded (source-level). `CreateBlob` packs the per-context tapes plus any
//! `AddData` values (structured-clone bytes via `JS_WriteObject`). Restoring
//! an isolate from such a blob replays the default-context tape into each new
//! `Context::New` (and the indexed tapes for `Context::FromSnapshot`), which
//! recreates the same JS heap state the creator observed. Deterministic init
//! code — the only thing snapshots are used for in practice — replays exactly.
//!
//! ## Init/user phase split (embedder-managed restores)
//!
//! Some embedders (deno_core) build part of the snapshot heap with *Rust*
//! C-API calls (`Deno.core` bindings, primordials, op functions) that a JS
//! tape cannot capture, then run init scripts that DEPEND on that state.
//! Replaying such a tape at `Context::New` throws (`Deno.core` is undefined).
//! Signal: those embedders call `Isolate::SetData(slot 0, state)` when their
//! init completes. So the tape is split at the creator's first `SetData(0)`
//! into an *init* tape and a *user* tape, and the blob is marked
//! "embedder-managed". Restoring a managed blob: the init tape is DROPPED
//! (the embedder re-runs its native init from scratch — v8x-patched deno_core
//! takes the InitMode::New path even with a snapshot), and the user tape is
//! replayed when the restoring runtime performs its own `SetData(0)` —
//! i.e. exactly when its heap reaches the state the creator's had. Blobs from
//! creators that never call `SetData(0)` (the vendored rusty_v8 tests) keep
//! the old semantics: the whole tape replays eagerly at `Context::New`.

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::CString;

use super::core::current_iso;
use super::core::iso_state;
use super::quickjs_sys::*;

pub(crate) const SNAP_MAGIC: &[u8; 8] = b"V8XSNAP2";

#[derive(Clone, Debug)]
pub(crate) enum TapeEntry {
  /// A script that ran via `v8__Script__Run` (global eval).
  Script { source: String },
  /// A module source registered at compile time (feeds the module loader).
  ModuleSource { name: String, source: String },
  /// A module that was evaluated (root of an eval; deps load via the loader).
  ModuleEval { name: String },
}

/// One context's worth of snapshot: its replay tapes (split at the creator's
/// first `SetData(0)`), its `AddData` values and its embedder-data slots
/// (captured at `CreateBlob` time).
#[derive(Clone, Default, Debug)]
pub(crate) struct ContextImage {
  pub init_tape: Vec<TapeEntry>,
  pub user_tape: Vec<TapeEntry>,
  pub ctx_data: Vec<Vec<u8>>,
  pub embedder: Vec<Option<Vec<u8>>>,
}

/// Recording side: lives on the isolate created by `SnapshotCreator`.
#[derive(Default)]
pub(crate) struct SnapState {
  /// Init-phase tape per live JSContext (keyed by pointer value).
  pub tapes: HashMap<usize, Vec<TapeEntry>>,
  /// User-phase tape (entries recorded after the first `SetData(0)`).
  pub user_tapes: HashMap<usize, Vec<TapeEntry>>,
  /// Set once the embedder calls `SetData(0)`: subsequent recordings are
  /// user-phase, and the blob is marked embedder-managed.
  pub user_phase: bool,
  /// `AddData(context, ..)` values per context, serialized eagerly.
  pub ctx_data: HashMap<usize, Vec<Vec<u8>>>,
  /// `AddData(isolate-level)` values, serialized eagerly.
  pub iso_data: Vec<Vec<u8>>,
  /// Context passed to `SetDefaultContext`.
  pub default_ctx: usize,
  /// Contexts passed to `AddContext`, in index order.
  pub added: Vec<usize>,
}

impl SnapState {
  fn tape_for(&mut self, ctx: usize) -> &mut Vec<TapeEntry> {
    if self.user_phase {
      self.user_tapes.entry(ctx).or_default()
    } else {
      self.tapes.entry(ctx).or_default()
    }
  }
}

/// Restore side: parsed blob + once-consumable data slots.
pub(crate) struct RestoredSnap {
  pub default_image: ContextImage,
  pub indexed: Vec<ContextImage>,
  /// Whether the creator called `SetData(0)` (see module docs): init tapes are
  /// dropped and user tapes replay at the restoring runtime's `SetData(0)`.
  pub embedder_managed: bool,
  /// Managed mode: user tapes waiting for the embedder's `SetData(0)`.
  pub pending_user: Vec<(usize, Vec<TapeEntry>)>,
  /// Isolate-level AddData values; `None` once consumed.
  pub iso_data: Vec<Option<Vec<u8>>>,
  /// Armed per live context at replay time; `None` once consumed.
  pub ctx_data: HashMap<usize, Vec<Option<Vec<u8>>>>,
}

thread_local! {
  /// Non-zero while a tape is replaying: suppresses re-recording the replayed
  /// entries into a chained creator's own tape (they are seeded explicitly).
  static REPLAYING: Cell<u32> = const { Cell::new(0) };
}

fn snap_of_current() -> Option<&'static mut SnapState> {
  if REPLAYING.with(|r| r.get()) > 0 {
    return None;
  }
  let iso = current_iso();
  if iso.is_null() {
    return None;
  }
  iso_state(iso).snap.as_deref_mut()
}

pub(crate) fn record_script(ctx: *mut JSContext, source: &str) {
  if let Some(snap) = snap_of_current() {
    snap.tape_for(ctx as usize).push(TapeEntry::Script {
      source: source.to_string(),
    });
  }
}

pub(crate) fn record_module_source(
  ctx: *mut JSContext,
  name: &str,
  source: &str,
) {
  if name.is_empty() {
    return;
  }
  if let Some(snap) = snap_of_current() {
    snap.tape_for(ctx as usize).push(TapeEntry::ModuleSource {
      name: name.to_string(),
      source: source.to_string(),
    });
  }
}

pub(crate) fn record_module_eval(ctx: *mut JSContext, name: &str) {
  if name.is_empty() {
    return;
  }
  if let Some(snap) = snap_of_current() {
    snap.tape_for(ctx as usize).push(TapeEntry::ModuleEval {
      name: name.to_string(),
    });
  }
}

/// Explicit init/user phase boundary (see module docs), reached via the
/// public `v8::v8x_snapshot_init_boundary` API. The embedder calls it exactly
/// when its native runtime init is complete. On a creator: flip recording to
/// the user phase. On a restored embedder-managed isolate: the embedder's
/// native init just finished — replay the pending user tapes.
pub(crate) fn init_boundary(iso: *mut crate::RealIsolate) {
  if iso.is_null() {
    return;
  }
  if let Some(snap) = iso_state(iso).snap.as_deref_mut() {
    snap.user_phase = true;
  }
  let pending = match iso_state(iso).restored.as_deref_mut() {
    Some(r) if r.embedder_managed => std::mem::take(&mut r.pending_user),
    _ => return,
  };
  for (ctx_key, tape) in pending {
    let ctx = ctx_key as *mut JSContext;
    replay_tape(ctx, &tape);
    // Chained creator: the replayed user entries belong to the NEW blob's
    // user tape (recording during replay is suppressed by REPLAYING).
    if let Some(snap) = iso_state(iso).snap.as_deref_mut() {
      snap
        .user_tapes
        .entry(ctx_key)
        .or_default()
        .extend(tape.iter().cloned());
    }
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

pub(crate) fn deserialize_value(ctx: *mut JSContext, bytes: &[u8]) -> JSValue {
  unsafe { JS_ReadObject(ctx, bytes.as_ptr(), bytes.len(), 0) }
}

/// Replay one tape into `ctx`. Returns false if any entry threw.
fn replay_tape(ctx: *mut JSContext, tape: &[TapeEntry]) -> bool {
  REPLAYING.with(|r| r.set(r.get() + 1));
  let mut ok = true;
  for entry in tape {
    match entry {
      TapeEntry::Script { source } => {
        let Ok(csrc) = CString::new(source.as_str()) else {
          ok = false;
          continue;
        };
        let v = unsafe {
          JS_Eval(
            ctx,
            csrc.as_ptr(),
            csrc.as_bytes().len(),
            c"<snapshot>".as_ptr(),
            JS_EVAL_TYPE_GLOBAL,
          )
        };
        if v.tag == JS_TAG_EXCEPTION {
          report_replay_exception(ctx, "<script>");
          ok = false;
        } else {
          unsafe { JS_FreeValue(ctx, v) };
        }
      }
      TapeEntry::ModuleSource { name, source } => {
        super::module::register_module_source(name, source);
      }
      TapeEntry::ModuleEval { name } => {
        let Some(source) = super::module::lookup_module_source(name) else {
          ok = false;
          continue;
        };
        let (Ok(csrc), Ok(cname)) =
          (CString::new(source), CString::new(name.as_str()))
        else {
          ok = false;
          continue;
        };
        let v = unsafe {
          JS_Eval(
            ctx,
            csrc.as_ptr(),
            csrc.as_bytes().len(),
            cname.as_ptr(),
            JS_EVAL_TYPE_MODULE,
          )
        };
        if v.tag == JS_TAG_EXCEPTION {
          report_replay_exception(ctx, name);
          ok = false;
        } else {
          unsafe { JS_FreeValue(ctx, v) };
        }
      }
    }
  }
  REPLAYING.with(|r| r.set(r.get() - 1));
  ok
}

/// Materialize one context image into `ctx` at context-creation time.
///
/// Always: restore embedder-data slots and arm the once-consumable AddData
/// slots. Tape: eager (init + user, old semantics) unless the blob is
/// embedder-managed — then the init tape is dropped and the user tape parks in
/// `pending_user` until `SetData(0)` (see module docs).
pub(crate) fn replay_into(
  iso: *mut crate::RealIsolate,
  ctx: *mut JSContext,
  image: &ContextImage,
) -> bool {
  let managed = iso_state(iso)
    .restored
    .as_deref()
    .map(|r| r.embedder_managed)
    .unwrap_or(false);

  let mut ok = true;
  if !managed {
    ok &= replay_tape(ctx, &image.init_tape);
    ok &= replay_tape(ctx, &image.user_tape);
  }

  // Restore embedder-data value slots.
  REPLAYING.with(|r| r.set(r.get() + 1));
  for (idx, slot) in image.embedder.iter().enumerate() {
    if let Some(bytes) = slot {
      let v = deserialize_value(ctx, bytes);
      if v.tag != JS_TAG_EXCEPTION {
        super::misc::set_embedder_data_raw(ctx, idx, v);
        unsafe { JS_FreeValue(ctx, v) };
      }
    }
  }
  REPLAYING.with(|r| r.set(r.get() - 1));

  let st = iso_state(iso);
  // Arm this context's once-consumable AddData slots (+ park the user tape in
  // managed mode).
  if let Some(restored) = st.restored.as_deref_mut() {
    restored.ctx_data.insert(
      ctx as usize,
      image.ctx_data.iter().map(|b| Some(b.clone())).collect(),
    );
    if managed && !image.user_tape.is_empty() {
      restored
        .pending_user
        .push((ctx as usize, image.user_tape.clone()));
    }
  }
  // Chained creator (`snapshot_creator_from_existing_snapshot`): seed the new
  // tapes with the replayed ones so the next CreateBlob emits old + new.
  // (Managed user tapes are seeded when they replay, in on_isolate_set_data.)
  if let Some(snap) = st.snap.as_deref_mut() {
    if !managed {
      snap
        .tapes
        .entry(ctx as usize)
        .or_default()
        .extend(image.init_tape.iter().cloned());
      snap
        .user_tapes
        .entry(ctx as usize)
        .or_default()
        .extend(image.user_tape.iter().cloned());
    }
  }
  ok
}

fn report_replay_exception(ctx: *mut JSContext, what: &str) {
  unsafe {
    let exc = JS_GetException(ctx);
    if std::env::var_os("QJS_DEBUG_SNAPSHOT").is_some() {
      let mut len = 0usize;
      let s = JS_ToCStringLen(ctx, &mut len, exc);
      if !s.is_null() {
        let bytes = std::slice::from_raw_parts(s as *const u8, len);
        eprintln!(
          "[qjs snapshot] replay of {what} threw: {}",
          String::from_utf8_lossy(bytes)
        );
        JS_FreeCString(ctx, s);
      }
    }
    JS_FreeValue(ctx, exc);
  }
}

// ---------------------------------------------------------------------------
// Blob wire format (little-endian, length-prefixed):
//   magic[8] | u8 embedder_managed
//   default_image | u32 n_indexed | indexed images
//   u32 n_iso_data | (u32 len, bytes)*
// image := u32 n_init_tape | entries | u32 n_user_tape | entries
//          u32 n_ctx_data | (u32 len, bytes)*
//          u32 n_embedder | (u8 present, [u32 len, bytes])*
// tape entry := u8 kind | fields (strings as u32 len + utf8)
// ---------------------------------------------------------------------------

fn put_u32(out: &mut Vec<u8>, v: u32) {
  out.extend_from_slice(&v.to_le_bytes());
}
fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
  put_u32(out, b.len() as u32);
  out.extend_from_slice(b);
}
fn put_str(out: &mut Vec<u8>, s: &str) {
  put_bytes(out, s.as_bytes());
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
  fn bytes(&mut self) -> Option<&'a [u8]> {
    let len = self.u32()? as usize;
    let b = self.buf.get(self.pos..self.pos + len)?;
    self.pos += len;
    Some(b)
  }
  fn string(&mut self) -> Option<String> {
    Some(String::from_utf8_lossy(self.bytes()?).into_owned())
  }
}

fn write_tape(out: &mut Vec<u8>, tape: &[TapeEntry]) {
  put_u32(out, tape.len() as u32);
  for e in tape {
    match e {
      TapeEntry::Script { source } => {
        out.push(0);
        put_str(out, source);
      }
      TapeEntry::ModuleSource { name, source } => {
        out.push(1);
        put_str(out, name);
        put_str(out, source);
      }
      TapeEntry::ModuleEval { name } => {
        out.push(2);
        put_str(out, name);
      }
    }
  }
}

fn read_tape(r: &mut Reader) -> Option<Vec<TapeEntry>> {
  let n = r.u32()?;
  let mut tape = Vec::with_capacity(n as usize);
  for _ in 0..n {
    let entry = match r.u8()? {
      0 => TapeEntry::Script {
        source: r.string()?,
      },
      1 => TapeEntry::ModuleSource {
        name: r.string()?,
        source: r.string()?,
      },
      2 => TapeEntry::ModuleEval { name: r.string()? },
      _ => return None,
    };
    tape.push(entry);
  }
  Some(tape)
}

fn write_image(out: &mut Vec<u8>, image: &ContextImage) {
  write_tape(out, &image.init_tape);
  write_tape(out, &image.user_tape);
  put_u32(out, image.ctx_data.len() as u32);
  for d in &image.ctx_data {
    put_bytes(out, d);
  }
  put_u32(out, image.embedder.len() as u32);
  for slot in &image.embedder {
    match slot {
      Some(b) => {
        out.push(1);
        put_bytes(out, b);
      }
      None => out.push(0),
    }
  }
}

fn read_image(r: &mut Reader) -> Option<ContextImage> {
  let init_tape = read_tape(r)?;
  let user_tape = read_tape(r)?;
  let n_data = r.u32()?;
  let mut ctx_data = Vec::with_capacity(n_data as usize);
  for _ in 0..n_data {
    ctx_data.push(r.bytes()?.to_vec());
  }
  let n_emb = r.u32()?;
  let mut embedder = Vec::with_capacity(n_emb as usize);
  for _ in 0..n_emb {
    embedder.push(match r.u8()? {
      1 => Some(r.bytes()?.to_vec()),
      _ => None,
    });
  }
  Some(ContextImage {
    init_tape,
    user_tape,
    ctx_data,
    embedder,
  })
}

/// Assemble the blob from a creator's recorded state. Embedder-data slots are
/// captured here (at blob time), serialized with the context still live.
pub(crate) fn create_blob(snap: &SnapState) -> Vec<u8> {
  let image_for = |ctx_key: usize| -> ContextImage {
    let ctx = ctx_key as *mut JSContext;
    let embedder = if ctx.is_null() {
      Vec::new()
    } else {
      super::misc::embedder_data_snapshot(ctx)
        .into_iter()
        .map(|v| v.and_then(|v| serialize_value(ctx, v)))
        .collect()
    };
    ContextImage {
      init_tape: snap.tapes.get(&ctx_key).cloned().unwrap_or_default(),
      user_tape: snap.user_tapes.get(&ctx_key).cloned().unwrap_or_default(),
      ctx_data: snap.ctx_data.get(&ctx_key).cloned().unwrap_or_default(),
      embedder,
    }
  };

  let mut out = Vec::new();
  out.extend_from_slice(SNAP_MAGIC);
  out.push(snap.user_phase as u8);
  write_image(&mut out, &image_for(snap.default_ctx));
  put_u32(&mut out, snap.added.len() as u32);
  for &ctx_key in &snap.added {
    write_image(&mut out, &image_for(ctx_key));
  }
  put_u32(&mut out, snap.iso_data.len() as u32);
  for d in &snap.iso_data {
    put_bytes(&mut out, d);
  }
  out
}

pub(crate) fn parse_blob(bytes: &[u8]) -> Option<RestoredSnap> {
  if bytes.len() < 8 || &bytes[..8] != SNAP_MAGIC {
    return None;
  }
  let mut r = Reader { buf: bytes, pos: 8 };
  let embedder_managed = r.u8()? != 0;
  let default_image = read_image(&mut r)?;
  let n_indexed = r.u32()?;
  let mut indexed = Vec::with_capacity(n_indexed as usize);
  for _ in 0..n_indexed {
    indexed.push(read_image(&mut r)?);
  }
  let n_iso = r.u32()?;
  let mut iso_data = Vec::with_capacity(n_iso as usize);
  for _ in 0..n_iso {
    iso_data.push(Some(r.bytes()?.to_vec()));
  }
  Some(RestoredSnap {
    default_image,
    indexed,
    embedder_managed,
    pending_user: Vec::new(),
    iso_data,
    ctx_data: HashMap::new(),
  })
}

// ---------------------------------------------------------------------------
// Blob buffers returned by CreateBlob: handed to Rust as raw (ptr, len); the
// StartupData Drop calls v8__StartupData__data__DELETE(ptr). Keep ownership in
// a registry so DELETE can free exactly what we allocated.
// ---------------------------------------------------------------------------

thread_local! {
  static BLOBS: std::cell::RefCell<HashMap<usize, Box<[u8]>>> =
    std::cell::RefCell::new(HashMap::new());
}

pub(crate) fn leak_blob(bytes: Vec<u8>) -> (*const u8, usize) {
  let boxed: Box<[u8]> = bytes.into_boxed_slice();
  let len = boxed.len();
  let ptr = boxed.as_ptr();
  BLOBS.with(|b| b.borrow_mut().insert(ptr as usize, boxed));
  (ptr, len)
}

pub(crate) fn free_blob(ptr: *const u8) {
  if ptr.is_null() {
    return;
  }
  BLOBS.with(|b| {
    b.borrow_mut().remove(&(ptr as usize));
  });
}
