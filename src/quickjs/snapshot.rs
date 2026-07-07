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

use super::quickjs_sys::*;

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

thread_local! {
  static BLOBS: std::cell::RefCell<HashMap<usize, Box<[u8]>>> =
    Default::default();
}

pub(crate) fn free_blob(ptr: *const u8) {
  if ptr.is_null() {
    return;
  }
  BLOBS.with(|b| {
    b.borrow_mut().remove(&(ptr as usize));
  });
}
