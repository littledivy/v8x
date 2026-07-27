//! Real Hermes backend: the hello-world path of the v8 C-ABI, authored in
//! Rust against a C++/JSI bridge (`src/hermes/hermes_shim.cpp`).
//!
//! ## Why this shape (mirrors src/quickjs/core.rs, with C++-side storage)
//!
//! Hermes is C++-only JSI. A `jsi::Value` is a move-only 16-byte C++ struct
//! (like QuickJS's `JSValue`), not a pointer, so it cannot itself be a v8
//! `Local<T>` (which the vendored surface treats as `*const T`). QuickJS
//! solves this with a Rust-side arena of boxed `JSValue`s. Hermes cannot: a
//! `jsi::Value` can only be created, copied, and destroyed *through its
//! Runtime*, and it must not outlive that Runtime (a C2 lifetime rule). So the
//! arena lives on the **C++ side**, inside the runtime wrapper: a
//! `std::vector<jsi::Value>` handle table. A v8 `Local` is an index into that
//! table.
//!
//! ## Handle encoding
//!
//! A slot index `i` is handed to Rust as the tagged pointer `((i << 1) | 1)`
//! so every live handle is a non-null `*const Data` (slot 0 would otherwise
//! collide with a null handle). `slot_of(ptr)` recovers `i`.
//!
//! ## HandleScope = watermark
//!
//! `CONSTRUCT` records the current C++ table length; `DESTRUCT` truncates the
//! table back to it, releasing every `jsi::Value` created since (while the
//! Runtime is still alive, per the C2 rule). This is the QuickJS
//! handle-scope-pop model with the storage on the C++ side.
//!
//! ## One runtime per thread
//!
//! Hermes keeps thread-local runtime state (a C2 finding), so there is one
//! Isolate/Runtime bound to the creating thread. A thread-local tracks the
//! current isolate and context, exactly like the QuickJS backend.

#![allow(non_snake_case)]

use crate::support::{MaybeBool, SharedPtrBase};
use crate::{
  Allocator, Array, ArrayBuffer, Boolean, Context, Data, External, Function,
  Integer, Number, Object, Platform, Primitive, RealIsolate, Script,
  String as V8String, UniquePtr, Value,
};
use std::cell::Cell;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};

// The extern "C" bridge into src/hermes/hermes_shim.cpp. A `RuntimeWrapper*`
// is an opaque `*mut c_void` on this side.
unsafe extern "C" {
  fn v8x_hermes_runtime_new() -> *mut c_void;
  fn v8x_hermes_runtime_free(rtw: *mut c_void);
  fn v8x_hermes_handles_len(rtw: *mut c_void) -> usize;
  fn v8x_hermes_handles_truncate(rtw: *mut c_void, watermark: usize);
  fn v8x_hermes_global(rtw: *mut c_void) -> i64;
  fn v8x_hermes_string_new_utf8(
    rtw: *mut c_void,
    data: *const c_char,
    len: usize,
  ) -> i64;
  fn v8x_hermes_run(rtw: *mut c_void, src_slot: i64, ok: *mut c_int) -> i64;
  fn v8x_hermes_value_to_utf8(
    rtw: *mut c_void,
    slot: i64,
    out: *mut c_char,
    cap: usize,
  ) -> usize;
  #[allow(dead_code)]
  fn v8x_hermes_value_is_string(rtw: *mut c_void, slot: i64) -> c_int;
  fn v8x_hermes_value_is_object(rtw: *mut c_void, slot: i64) -> c_int;
  // C4: object identity. See docs/hermes-spike/experiments/C4-hermes-identity.md.
  fn v8x_hermes_strict_equals(rtw: *mut c_void, a: i64, b: i64) -> c_int;
  fn v8x_hermes_get_identity_hash(rtw: *mut c_void, slot: i64) -> i64;

  // C6: Object / Array / Number / Integer / Boolean / Function. See
  // docs/hermes-spike/experiments/C6-hermes-surface.md.
  fn v8x_hermes_object_new(rtw: *mut c_void) -> i64;
  fn v8x_hermes_object_get(rtw: *mut c_void, obj_slot: i64, key_slot: i64) -> i64;
  fn v8x_hermes_object_set(
    rtw: *mut c_void,
    obj_slot: i64,
    key_slot: i64,
    value_slot: i64,
  ) -> c_int;
  fn v8x_hermes_object_has(rtw: *mut c_void, obj_slot: i64, key_slot: i64) -> c_int;
  fn v8x_hermes_array_new(rtw: *mut c_void, length: i64) -> i64;
  fn v8x_hermes_array_length(rtw: *mut c_void, slot: i64) -> i64;
  fn v8x_hermes_array_get_index(rtw: *mut c_void, slot: i64, index: u32) -> i64;
  fn v8x_hermes_array_set_index(
    rtw: *mut c_void,
    slot: i64,
    index: u32,
    value_slot: i64,
  ) -> c_int;
  fn v8x_hermes_number_new(rtw: *mut c_void, value: f64) -> i64;
  fn v8x_hermes_number_value(rtw: *mut c_void, slot: i64, out: *mut f64) -> c_int;
  fn v8x_hermes_boolean_new(rtw: *mut c_void, value: c_int) -> i64;
  fn v8x_hermes_boolean_value(rtw: *mut c_void, slot: i64) -> c_int;
  fn v8x_hermes_function_call(
    rtw: *mut c_void,
    fn_slot: i64,
    recv_slot: i64,
    arg_slots: *const i64,
    argc: usize,
    ok: *mut c_int,
  ) -> i64;
  fn v8x_hermes_value_is_array(rtw: *mut c_void, slot: i64) -> c_int;
  fn v8x_hermes_value_is_function(rtw: *mut c_void, slot: i64) -> c_int;
  fn v8x_hermes_value_is_number(rtw: *mut c_void, slot: i64) -> c_int;
  fn v8x_hermes_value_is_boolean(rtw: *mut c_void, slot: i64) -> c_int;
  fn v8x_hermes_undefined(rtw: *mut c_void) -> i64;
  fn v8x_hermes_null(rtw: *mut c_void) -> i64;

  // C7: External (opaque embedder void*), a few more Value predicates/coercions.
  fn v8x_hermes_external_new(rtw: *mut c_void, ptr: *mut c_void) -> i64;
  fn v8x_hermes_external_value(
    rtw: *mut c_void,
    slot: i64,
    found: *mut c_int,
  ) -> *mut c_void;
  fn v8x_hermes_value_is_external(rtw: *mut c_void, slot: i64) -> c_int;
  fn v8x_hermes_value_is_undefined(rtw: *mut c_void, slot: i64) -> c_int;
  fn v8x_hermes_uint32_value(
    rtw: *mut c_void,
    slot: i64,
    out: *mut u32,
  ) -> c_int;

  // C8: ArrayBuffer + TypedArray. See
  // docs/hermes-spike/experiments/C8-hermes-testapi.md.
  fn v8x_hermes_array_buffer_new(rtw: *mut c_void, byte_length: usize) -> i64;
  fn v8x_hermes_array_buffer_byte_length(rtw: *mut c_void, slot: i64) -> usize;
  fn v8x_hermes_array_buffer_data(rtw: *mut c_void, slot: i64) -> *mut c_void;
  fn v8x_hermes_typed_array_new(
    rtw: *mut c_void,
    ctor_name: *const c_char,
    buf_slot: i64,
    byte_offset: usize,
    length: usize,
  ) -> i64;
  fn v8x_hermes_typed_array_length(rtw: *mut c_void, slot: i64) -> usize;
}

/// The C++ null-slot sentinel (must match `V8X_HERMES_NULL_SLOT` in the shim).
const NULL_SLOT: i64 = -1;

/// Per-isolate state. One HermesRuntime, bound to the creating thread. Boxed;
/// its address is the `*mut RealIsolate` the vendored surface passes around.
pub(crate) struct IsoState {
  /// Opaque `RuntimeWrapper*` from the C++ shim (owns the runtime + handle
  /// table). Freed in `v8__Isolate__Dispose`.
  pub rtw: *mut c_void,
  /// Embedder data slots (v8__Isolate__Get/SetData). The vendored surface uses
  /// slot 0 for its `IsolateAnnex`; four is plenty for the hello-world path.
  pub data_slots: [*mut c_void; 4],
  /// Context aligned-pointer embedder-data fields
  /// (v8__Context__Get/SetAlignedPointerInEmbedderData). There is one context
  /// per Hermes isolate (its handle IS the isolate pointer), so these live on
  /// the isolate. Grows on demand; used by `Context::set_slot`'s annex.
  pub ctx_embedder_data: Vec<*mut c_void>,
  /// The context's MicrotaskQueue pointer (C8). Hermes drains promise jobs
  /// inside evaluateJavaScript and exposes no embedder MicrotaskQueue, so this
  /// is an inert non-null marker: `GetMicrotaskQueue` must not return null
  /// (the vendored `&*ptr` deref would SEGV and abort the whole test binary),
  /// and `SetMicrotaskQueue`/`GetMicrotaskQueue` round-trip the same pointer so
  /// the identity check in `microtask_queue_new` holds. Defaults to a stable
  /// per-isolate marker (the field's own address is used).
  pub microtask_queue: *mut c_void,
}

thread_local! {
  /// The current entered isolate for this thread (set by Enter, cleared by
  /// Exit). There is at most one live Hermes isolate per thread.
  static CURRENT_ISO: Cell<*mut RealIsolate> = const { Cell::new(ptr::null_mut()) };
}

#[inline]
pub(crate) fn iso_state<'a>(p: *mut RealIsolate) -> &'a mut IsoState {
  debug_assert!(!p.is_null());
  unsafe { &mut *(p as *mut IsoState) }
}

#[inline]
pub(crate) fn current_iso() -> *mut RealIsolate {
  CURRENT_ISO.with(|c| c.get())
}

/// Encode a C++ handle-table slot index as a non-null v8 `Local` pointer.
/// (`(i << 1) | 1` keeps slot 0 distinguishable from a null handle.)
#[inline]
fn slot_ptr<T>(slot: i64) -> *const T {
  if slot < 0 {
    return ptr::null();
  }
  (((slot as usize) << 1) | 1) as *const T
}

/// Recover the slot index from a tagged `Local` pointer, or `NULL_SLOT`.
#[inline]
fn slot_of<T>(ptr: *const T) -> i64 {
  let bits = ptr as usize;
  if bits == 0 || (bits & 1) == 0 {
    return NULL_SLOT;
  }
  (bits >> 1) as i64
}

/// The `RuntimeWrapper*` for the current thread's isolate, or null.
#[inline]
fn current_rtw() -> *mut c_void {
  let iso = current_iso();
  if iso.is_null() {
    return ptr::null_mut();
  }
  iso_state(iso).rtw
}

// ---- Isolate ---------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__New(_params: *const c_void) -> *mut RealIsolate {
  let rtw = unsafe { v8x_hermes_runtime_new() };
  assert!(
    !rtw.is_null(),
    "v8x_hermes_runtime_new failed (makeHermesRuntime threw)"
  );
  let mut st = Box::new(IsoState {
    rtw,
    data_slots: [ptr::null_mut(); 4],
    ctx_embedder_data: Vec::new(),
    microtask_queue: ptr::null_mut(),
  });
  // A stable non-null default marker: the box's own state address. Overwritten
  // by any later SetMicrotaskQueue.
  st.microtask_queue = (&*st as *const IsoState) as *mut c_void;
  Box::into_raw(st) as *mut RealIsolate
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__Dispose(this: *mut RealIsolate) {
  if this.is_null() {
    return;
  }
  // Drop the C++ runtime wrapper (clears the handle table Values while the
  // runtime is alive), then the Rust box.
  let st = unsafe { Box::from_raw(this as *mut IsoState) };
  unsafe { v8x_hermes_runtime_free(st.rtw) };
  if current_iso() == this {
    CURRENT_ISO.with(|c| c.set(ptr::null_mut()));
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__Enter(this: *mut RealIsolate) {
  CURRENT_ISO.with(|c| c.set(this));
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__Exit(_this: *mut RealIsolate) {
  CURRENT_ISO.with(|c| c.set(ptr::null_mut()));
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetCurrent() -> *mut RealIsolate {
  current_iso()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetNumberOfDataSlots(
  _this: *const RealIsolate,
) -> u32 {
  4
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetData(
  isolate: *const RealIsolate,
  slot: u32,
) -> *mut c_void {
  if isolate.is_null() {
    return ptr::null_mut();
  }
  let st = iso_state(isolate as *mut RealIsolate);
  *st.data_slots.get(slot as usize).unwrap_or(&ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetData(
  isolate: *const RealIsolate,
  slot: u32,
  data: *mut c_void,
) {
  if isolate.is_null() {
    return;
  }
  let st = iso_state(isolate as *mut RealIsolate);
  if let Some(s) = st.data_slots.get_mut(slot as usize) {
    *s = data;
  }
}

/// There is one context per Hermes isolate: its pointer is the isolate
/// pointer. `GetCurrentContext` therefore returns the current isolate reused as
/// a `*const Context` handle.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetCurrentContext(
  isolate: *mut RealIsolate,
) -> *const Context {
  isolate as *const Context
}

// ---- Global / Weak (persistent handles) ------------------------------------
//
// A v8 `Global<T>` is a persistent handle that survives handle scopes. Our
// handle table entries already survive until the runtime is torn down (a
// HandleScope only truncates its own watermark on exit, and `Global::new`
// copies the data pointer out of any enclosing scope), so a `Global` is
// modeled as the same handle-table slot pointer carried unchanged. This keeps
// the value reachable for the life of the isolate, which is correct for the
// context-slot and kept-context tests (they never rely on GC reclaiming a
// Global).
//
// `NewWeak` installs a would-be weak handle + finalizer. Hermes exposes no
// embedder weak-callback hook through JSI, so this returns the same data
// pointer as a non-firing weak: the value stays strongly reachable and the
// finalizer never runs. That is a conservative over-retention (a leak, not a
// use-after-free), acceptable for the current test surface; a real weak/GC
// integration is a later cycle. `Reset` is a safe no-op for the same reason.

#[unsafe(no_mangle)]
pub extern "C" fn v8__Global__New(
  _isolate: *mut RealIsolate,
  data: *const Data,
) -> *const Data {
  data
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Global__NewWeak(
  _isolate: *mut RealIsolate,
  data: *const Data,
  _parameter: *const c_void,
  _callback: unsafe extern "C" fn(*const c_void),
) -> *const Data {
  data
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Global__Reset(_data: *const Data) {}

// ---- HandleScope -----------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__HandleScope__CONSTRUCT(
  buf: *mut usize,
  isolate: *mut RealIsolate,
) {
  CURRENT_ISO.with(|c| c.set(isolate));
  let watermark = if isolate.is_null() {
    0
  } else {
    unsafe { v8x_hermes_handles_len(iso_state(isolate).rtw) }
  };
  unsafe {
    *buf.offset(0) = isolate as usize;
    *buf.offset(1) = watermark;
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__HandleScope__DESTRUCT(this: *mut usize) {
  unsafe {
    let isolate = *this.offset(0) as *mut RealIsolate;
    let watermark = *this.offset(1);
    if isolate.is_null() {
      return;
    }
    v8x_hermes_handles_truncate(iso_state(isolate).rtw, watermark);
  }
}

// EscapableHandleScope: reserve a slot in the parent, then move an escaping
// handle's value into it on scope exit. For the hello-world path we reserve a
// fresh slot (an undefined placeholder) and, on escape, copy the escaping
// value into a fresh parent slot. Because our handle table is append-only per
// scope and the escaping value's slot lives below the child watermark being
// truncated, we duplicate it into a new slot that survives the truncation.
#[unsafe(no_mangle)]
pub extern "C" fn v8__EscapeSlot__reserve(_isolate: *mut RealIsolate) -> usize {
  // No pre-reservation needed: escape() creates a fresh surviving slot. Return
  // a sentinel the escape path ignores.
  usize::MAX
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__EscapeSlot__escape(
  isolate: *mut RealIsolate,
  _index: usize,
  value: *const Data,
) -> *const Data {
  if isolate.is_null() || value.is_null() {
    return value;
  }
  let slot = slot_of(value);
  if slot < 0 {
    return value;
  }
  // Re-materialize the escaping value into a fresh slot that lives ABOVE the
  // child scope's watermark, so the child DESTRUCT's truncate does not reclaim
  // it. We coerce/copy through the shim: for the hello-world path the escaping
  // value is the script result (a string), which value_to_utf8 can always
  // re-read; but to preserve the exact Value we push a duplicate string slot.
  //
  // Simplest correct move for C3: read the value as a JS string and re-intern
  // it as a fresh string handle. This is lossy for non-string Values and is a
  // known C3 limitation (see the doc); the hello-world result is a string.
  let rtw = iso_state(isolate).rtw;
  let mut buf = vec![0u8; 256];
  let n = unsafe {
    v8x_hermes_value_to_utf8(
      rtw,
      slot,
      buf.as_mut_ptr() as *mut c_char,
      buf.len(),
    )
  };
  if n == usize::MAX {
    return value;
  }
  if n > buf.len() {
    buf = vec![0u8; n];
    unsafe {
      v8x_hermes_value_to_utf8(
        rtw,
        slot,
        buf.as_mut_ptr() as *mut c_char,
        buf.len(),
      );
    }
  }
  let copy = n.min(buf.len());
  let new_slot = unsafe {
    v8x_hermes_string_new_utf8(rtw, buf.as_ptr() as *const c_char, copy)
  };
  if new_slot < 0 {
    return value;
  }
  slot_ptr::<Data>(new_slot)
}

// ---- Context ---------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__New(
  isolate: *mut RealIsolate,
  _templ: *const c_void,
  _global_object: *const c_void,
  _microtask_queue: *mut c_void,
) -> *const Context {
  // One context per Hermes runtime; its handle is the isolate pointer.
  isolate as *const Context
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__Enter(this: *const Context) {
  // Binding the context also binds its isolate as current (they share a ptr).
  CURRENT_ISO.with(|c| c.set(this as *mut RealIsolate));
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__Exit(_this: *const Context) {
  // The enclosing HandleScope/Isolate scope restores the previous current
  // isolate on drop; nothing to unwind here for the single-context path.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__Global(this: *const Context) -> *const Object {
  if this.is_null() {
    return ptr::null();
  }
  let st = iso_state(this as *mut RealIsolate);
  let slot = unsafe { v8x_hermes_global(st.rtw) };
  slot_ptr::<Object>(slot)
}

/// `Context::get_microtask_queue`. Returns the per-isolate marker pointer (see
/// `IsoState::microtask_queue`). Never null: the vendored getter deref's the
/// result, so a null would SEGV and abort the whole test binary. Hermes has no
/// real embedder MicrotaskQueue; the marker only satisfies the pointer-identity
/// and non-null contracts, not microtask enqueue/flush semantics.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetMicrotaskQueue(
  this: *const Context,
) -> *mut c_void {
  if this.is_null() {
    return ptr::null_mut();
  }
  iso_state(this as *mut RealIsolate).microtask_queue
}

/// `Context::set_microtask_queue`. Stores the pointer so a later
/// `get_microtask_queue` returns the same value (the identity check in
/// `microtask_queue_new`). No real queue is installed.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__SetMicrotaskQueue(
  this: *const Context,
  queue: *mut c_void,
) {
  if this.is_null() {
    return;
  }
  iso_state(this as *mut RealIsolate).microtask_queue = queue;
}

// ---- Context embedder data (aligned-pointer fields) -----------------------
//
// `Context::set_slot`/`get_slot` store an annex pointer in embedder-data field
// 0 of the context, growing the field vector as needed. Our Context IS the
// isolate (one context per Hermes runtime), so those fields live on the
// `IsoState`. Only the aligned-pointer variants are needed by the context-slot
// tests; the `Value`-typed `Get/SetEmbedderData` pair is not on this surface.

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetNumberOfEmbedderDataFields(
  this: *const Context,
) -> u32 {
  if this.is_null() {
    return 0;
  }
  iso_state(this as *mut RealIsolate).ctx_embedder_data.len() as u32
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetAlignedPointerFromEmbedderData(
  this: *const Context,
  index: c_int,
) -> *mut c_void {
  if this.is_null() || index < 0 {
    return ptr::null_mut();
  }
  let st = iso_state(this as *mut RealIsolate);
  st.ctx_embedder_data
    .get(index as usize)
    .copied()
    .unwrap_or(ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__SetAlignedPointerInEmbedderData(
  this: *const Context,
  index: c_int,
  value: *mut c_void,
) {
  if this.is_null() || index < 0 {
    return;
  }
  let st = iso_state(this as *mut RealIsolate);
  let idx = index as usize;
  if idx >= st.ctx_embedder_data.len() {
    st.ctx_embedder_data.resize(idx + 1, ptr::null_mut());
  }
  st.ctx_embedder_data[idx] = value;
}

// ---- String ----------------------------------------------------------------

fn intern_string_utf8(isolate: *mut RealIsolate, bytes: &[u8]) -> *const V8String {
  if isolate.is_null() {
    return ptr::null();
  }
  let st = iso_state(isolate);
  let slot = unsafe {
    v8x_hermes_string_new_utf8(
      st.rtw,
      bytes.as_ptr() as *const c_char,
      bytes.len(),
    )
  };
  slot_ptr::<V8String>(slot)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewFromUtf8(
  isolate: *mut RealIsolate,
  data: *const c_char,
  _new_type: c_int,
  length: c_int,
) -> *const V8String {
  if data.is_null() {
    return ptr::null();
  }
  let len = if length < 0 {
    unsafe { std::ffi::CStr::from_ptr(data).to_bytes().len() }
  } else {
    length as usize
  };
  let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, len) };
  intern_string_utf8(isolate, bytes)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewFromOneByte(
  isolate: *mut RealIsolate,
  data: *const u8,
  _new_type: c_int,
  length: c_int,
) -> *const V8String {
  if data.is_null() {
    return ptr::null();
  }
  let len = if length < 0 {
    unsafe { std::ffi::CStr::from_ptr(data as *const c_char).to_bytes().len() }
  } else {
    length as usize
  };
  // Latin-1: each byte is a code point. Re-encode to UTF-8 so JSI gets valid
  // UTF-8 (bytes >= 0x80 are 2-byte UTF-8 sequences).
  let latin1 = unsafe { std::slice::from_raw_parts(data, len) };
  let utf8: String = latin1.iter().map(|&b| b as char).collect();
  intern_string_utf8(isolate, utf8.as_bytes())
}

/// UTF-16 code-unit length (v8 `String::Length`). We compute it from the
/// UTF-8 the shim gives us.
#[unsafe(no_mangle)]
pub extern "C" fn v8__String__Length(this: *const V8String) -> c_int {
  let rtw = current_rtw();
  if rtw.is_null() {
    return 0;
  }
  let s = read_string(rtw, slot_of(this));
  s.map(|s| s.encode_utf16().count() as c_int).unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__Utf8Length(
  this: *const V8String,
  isolate: *mut RealIsolate,
) -> c_int {
  let rtw = if isolate.is_null() {
    current_rtw()
  } else {
    iso_state(isolate).rtw
  };
  if rtw.is_null() {
    return 0;
  }
  let n = unsafe {
    v8x_hermes_value_to_utf8(rtw, slot_of(this), ptr::null_mut(), 0)
  };
  if n == usize::MAX {
    0
  } else {
    n as c_int
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__WriteUtf8_v2(
  this: *const V8String,
  isolate: *mut RealIsolate,
  buffer: *mut c_char,
  capacity: usize,
  _flags: c_int,
  processed_characters_return: *mut usize,
) -> c_int {
  let rtw = if isolate.is_null() {
    current_rtw()
  } else {
    iso_state(isolate).rtw
  };
  if rtw.is_null() || buffer.is_null() {
    return 0;
  }
  let full = unsafe { v8x_hermes_value_to_utf8(rtw, slot_of(this), buffer, capacity) };
  if full == usize::MAX {
    return 0;
  }
  let written = full.min(capacity);
  if !processed_characters_return.is_null() {
    // Best-effort: report the code-unit count we managed to emit. The
    // vendored to_rust_string_lossy path sizes the buffer to the full utf8
    // length first, so written == full in practice.
    unsafe { *processed_characters_return = written };
  }
  written as c_int
}

/// Read the JS string in `slot` back into a Rust `String` (coercing if needed).
fn read_string(rtw: *mut c_void, slot: i64) -> Option<String> {
  if rtw.is_null() || slot < 0 {
    return None;
  }
  let n = unsafe { v8x_hermes_value_to_utf8(rtw, slot, ptr::null_mut(), 0) };
  if n == usize::MAX {
    return None;
  }
  let mut buf = vec![0u8; n];
  let got = unsafe {
    v8x_hermes_value_to_utf8(rtw, slot, buf.as_mut_ptr() as *mut c_char, n)
  };
  if got == usize::MAX {
    return None;
  }
  buf.truncate(got.min(n));
  String::from_utf8(buf).ok()
}

// ---- Script ----------------------------------------------------------------

/// A compiled script handle. Because Hermes compiles-and-runs in one JSI call,
/// "compile" just remembers the source-string slot; "run" evaluates it. The
/// Script handle reuses the source-string slot's tagged pointer.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Script__Compile(
  context: *const Context,
  source: *const V8String,
  _origin: *const c_void,
) -> *const Script {
  if context.is_null() || source.is_null() {
    return ptr::null();
  }
  // Under `--use_strict` (V8 makes every top-level script strict), prepend a
  // `"use strict";` directive by re-interning the source. Otherwise the source
  // string is already a slot, carried directly as the Script handle.
  if USE_STRICT.load(Ordering::Relaxed) {
    let isolate = context as *mut RealIsolate;
    let rtw = iso_state(isolate).rtw;
    if let Some(src) = read_string(rtw, slot_of(source)) {
      let strict = format!("\"use strict\";\n{src}");
      let new_src = intern_string_utf8(isolate, strict.as_bytes());
      if !new_src.is_null() {
        return new_src as *const Script;
      }
    }
  }
  source as *const Script
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Script__Run(
  script: *const Script,
  context: *const Context,
) -> *const Value {
  if script.is_null() || context.is_null() {
    return ptr::null();
  }
  let st = iso_state(context as *mut RealIsolate);
  let src_slot = slot_of(script);
  let mut ok: c_int = 0;
  let out = unsafe { v8x_hermes_run(st.rtw, src_slot, &mut ok) };
  if ok == 0 || out < 0 {
    return ptr::null();
  }
  slot_ptr::<Value>(out)
}

// ---- String ValueView (the read fast-path) ---------------------------------
//
// `String::to_rust_string_lossy` reads a string through a `ValueView`: it
// CONSTRUCTs a view (which materializes the code units), asks whether the data
// is one-byte or two-byte, reads the pointer + length, then DESTRUCTs. We
// materialize the units from the shim's UTF-8 into an owned buffer stored in
// the view; DESTRUCT frees it. The vendored `ValueView` buffer is 32 bytes,
// which comfortably holds this `ViewState`.

#[repr(C)]
pub(super) struct ViewState {
  data: *mut c_void,
  len: usize,
  is_one_byte: bool,
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ValueView__CONSTRUCT(
  buf: *mut ViewState,
  isolate: *mut RealIsolate,
  string: *const V8String,
) {
  let rtw = if isolate.is_null() {
    current_rtw()
  } else {
    iso_state(isolate).rtw
  };
  let s = read_string(rtw, slot_of(string)).unwrap_or_default();
  let units: Vec<u16> = s.encode_utf16().collect();
  let is_one_byte = units.iter().all(|&u| u <= 0xFF);
  let len = units.len();
  let data = if is_one_byte {
    let bytes: Box<[u8]> = units.iter().map(|&u| u as u8).collect();
    Box::into_raw(bytes) as *mut c_void
  } else {
    Box::into_raw(units.into_boxed_slice()) as *mut c_void
  };
  unsafe {
    (*buf).data = data;
    (*buf).len = len;
    (*buf).is_one_byte = is_one_byte;
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ValueView__DESTRUCT(this: *mut ViewState) {
  unsafe {
    let st = &mut *this;
    if !st.data.is_null() {
      if st.is_one_byte {
        let slice = ptr::slice_from_raw_parts_mut(st.data as *mut u8, st.len);
        drop(Box::from_raw(slice));
      } else {
        let slice = ptr::slice_from_raw_parts_mut(st.data as *mut u16, st.len);
        drop(Box::from_raw(slice));
      }
      st.data = ptr::null_mut();
      st.len = 0;
      st.is_one_byte = true;
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ValueView__is_one_byte(
  this: *const ViewState,
) -> bool {
  unsafe { !this.is_null() && (*this).is_one_byte }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ValueView__data(
  this: *const ViewState,
) -> *const c_void {
  unsafe { (*this).data as *const c_void }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ValueView__length(
  this: *const ViewState,
) -> c_int {
  unsafe { (*this).len as c_int }
}

// ---- Value ----------------------------------------------------------------

/// `Value::ToString`: coerce a value to a JS string. Hermes has no by-reference
/// coercion that hands back a Value*, so we read the value's string form
/// through the shim and intern a fresh String handle holding it. For the
/// hello-world path the value is already a string, so this round-trips its
/// exact bytes.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToString(
  this: *const Value,
  context: *const Context,
) -> *const V8String {
  if this.is_null() || context.is_null() {
    return ptr::null();
  }
  let isolate = context as *mut RealIsolate;
  let rtw = iso_state(isolate).rtw;
  let slot = slot_of(this);
  let Some(s) = read_string(rtw, slot) else {
    return ptr::null();
  };
  intern_string_utf8(isolate, s.as_bytes())
}

/// Whether the value in a handle is a JS string. Not on the hello-world path
/// yet, but kept because the string-vs-coerce distinction is needed once more
/// of the Value surface (Value::IsString and friends) lands.
#[allow(dead_code)]
pub(crate) fn value_is_string(this: *const V8String) -> bool {
  let rtw = current_rtw();
  if rtw.is_null() {
    return false;
  }
  unsafe { v8x_hermes_value_is_string(rtw, slot_of(this)) != 0 }
}

/// `Value::IsObject`: needed so Rust can safely `Local<Value>::try_cast::<
/// Object>()` (the vendored `TryFrom` impl checks this first). Routes to
/// `jsi::Value::isObject`.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsObject(this: *const Value) -> bool {
  let rtw = current_rtw();
  if rtw.is_null() {
    return false;
  }
  let slot = slot_of(this);
  if slot < 0 {
    return false;
  }
  unsafe { v8x_hermes_value_is_object(rtw, slot) != 0 }
}

/// `Value::ToObject`: our `Local` is a handle-table slot, and `Value`/
/// `Object` are the same tagged-pointer representation over that table, so
/// when the value already holds a JS object this is the identity function on
/// the slot (re-tagged as an `Object` handle). Non-object values are boxed
/// the way JS `ToObject` would (e.g. a primitive wrapper) in real V8; that
/// coercion is out of scope here (returns null), since the hello-world/C4
/// surface only calls this on values already known to be objects.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToObject(
  this: *const Value,
  context: *const Context,
) -> *const Object {
  if this.is_null() || context.is_null() {
    return ptr::null();
  }
  let isolate = context as *mut RealIsolate;
  let rtw = iso_state(isolate).rtw;
  let slot = slot_of(this);
  if slot < 0 || unsafe { v8x_hermes_value_is_object(rtw, slot) } == 0 {
    return ptr::null();
  }
  this as *const Object
}

// ---- Object identity (C4) ---------------------------------------------------
//
// THE PROBLEM: a v8 `Local` here is a C++ handle-table slot index (see the
// module doc). JSI hands out no raw object pointer, so two Locals obtained
// for the SAME JS object (e.g. read back twice from a JS variable) are
// different slot indices with different tagged pointers. Naively comparing
// tagged-pointer bits (what a literal port of "V8 Value* identity" would do)
// is WRONG: it would call two handles to the same object "different" and
// (accidentally) never call two handles to different primitive values of the
// same slot index "same", since slots are never reused while live. So every
// V8 identity/hash entry point must reroute through the underlying JSI object
// identity instead of the slot's pointer bits. See
// docs/hermes-spike/experiments/C4-hermes-identity.md for the demonstration
// and the fix, proven by the `hermes_identity` test in mod.rs.

/// `Value::StrictEquals` (`===`): routes to `jsi::Value::strictEquals`, which
/// compares the underlying JSI value (by JS `===` semantics: identity for
/// objects/symbols, content for strings, value for numbers/booleans), NOT the
/// handle-table slot index. Two different slots holding the same JS object
/// correctly compare equal.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__StrictEquals(
  this: *const Value,
  other: *const Value,
) -> bool {
  let rtw = current_rtw();
  if rtw.is_null() {
    return false;
  }
  let a = slot_of(this);
  let b = slot_of(other);
  if a < 0 || b < 0 {
    return a == b;
  }
  let r = unsafe { v8x_hermes_strict_equals(rtw, a, b) };
  r == 1
}

/// `Value::SameValue` (`Object.is`): for the hello-world/C4 surface this is
/// implemented identically to StrictEquals, EXCEPT SameValue additionally
/// treats `NaN === NaN` as true and `+0`/`-0` as distinct, unlike `===`. JSI's
/// `Value` does not expose bit-level float inspection needed to special-case
/// signed zero, so this is a known simplification: correct for the object-
/// identity surface this experiment targets (objects/strings/booleans), and
/// for ordinary (non-NaN, non-zero) numbers; NaN/+-0 edge cases are not yet
/// exact. Flagged in the C4 doc as a residual risk, not silently papered over.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__SameValue(
  this: *const Value,
  other: *const Value,
) -> bool {
  v8__Value__StrictEquals(this, other)
}

/// `Object::GetIdentityHash`: a STABLE per-object integer (v8's contract:
/// same object -> same hash across repeated calls and across different
/// Locals/handles to it; never 0). JSI has no built-in identity hash, so this
/// uses the standard embedder trick (see hermes_shim.cpp
/// v8x_hermes_get_identity_hash): lazily attach a hidden, non-enumerable,
/// Symbol-keyed property holding a monotonically increasing id, read back on
/// later calls. The id lives on the object's own heap storage, not on the
/// (non-canonical, per-Local) slot index, so two different slots for the same
/// object yield the same hash.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetIdentityHash(this: *const Object) -> c_int {
  let rtw = current_rtw();
  if rtw.is_null() {
    return 0;
  }
  let slot = slot_of(this);
  if slot < 0 {
    return 0;
  }
  let h = unsafe { v8x_hermes_get_identity_hash(rtw, slot) };
  if h <= 0 {
    // v8's contract is "never 0"; the shim returns -1 on error (e.g. a
    // non-object Value). Surface a distinguishable-but-nonzero sentinel
    // rather than silently returning the reserved 0.
    return -1;
  }
  // v8's identity hash is a 31-bit-ish int; our monotonic counter fits easily
  // for any test-scale object count.
  h as c_int
}

// ---- Object / Array / Number / Integer / Boolean / Function (C6) ---------
//
// Widens the surface past hello-world (C3) + identity (C4): object/array
// construction and property access, numeric/boolean primitives, and calling
// a JS function value. Every op routes through the C++/JSI bridge the same
// way the rest of this file does: a v8 Local is a handle-table slot, and
// each entry point is a thin wrapper around one of the
// `v8x_hermes_*` shim functions (each already wrapped in the C2 catch-all on
// the C++ side). See docs/hermes-spike/experiments/C6-hermes-surface.md.

/// `v8::undefined(scope)`: needed as the receiver for `Function::Call` when
/// the caller wants an `undefined` `this` (no JSI Runtime call needed;
/// `jsi::Value::undefined()` is a static factory, just pushed into the
/// handle table).
#[unsafe(no_mangle)]
pub extern "C" fn v8__Undefined(isolate: *mut RealIsolate) -> *const Primitive {
  if isolate.is_null() {
    return ptr::null();
  }
  let rtw = iso_state(isolate).rtw;
  let slot = unsafe { v8x_hermes_undefined(rtw) };
  slot_ptr::<Primitive>(slot)
}

/// `v8::null(scope)`: see `v8__Undefined` above.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Null(isolate: *mut RealIsolate) -> *const Primitive {
  if isolate.is_null() {
    return ptr::null();
  }
  let rtw = iso_state(isolate).rtw;
  let slot = unsafe { v8x_hermes_null(rtw) };
  slot_ptr::<Primitive>(slot)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__New(isolate: *mut RealIsolate) -> *const Object {
  if isolate.is_null() {
    return ptr::null();
  }
  let rtw = iso_state(isolate).rtw;
  let slot = unsafe { v8x_hermes_object_new(rtw) };
  slot_ptr::<Object>(slot)
}

/// `Object::Get`: the v8 C-ABI key is a generic `Value` (not just a `Name`),
/// but JSI's `Object::getProperty` is string-/PropNameID-keyed only, so the
/// key is coerced to a JS string on the C++ side (matching ordinary v8
/// property-key coercion for non-Name keys).
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__Get(
  this: *const Object,
  context: *const Context,
  key: *const Value,
) -> *const Value {
  if this.is_null() || context.is_null() || key.is_null() {
    return ptr::null();
  }
  let rtw = iso_state(context as *mut RealIsolate).rtw;
  let slot = unsafe { v8x_hermes_object_get(rtw, slot_of(this), slot_of(key)) };
  slot_ptr::<Value>(slot)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__Set(
  this: *const Object,
  context: *const Context,
  key: *const Value,
  value: *const Value,
) -> MaybeBool {
  if this.is_null() || context.is_null() || key.is_null() || value.is_null() {
    return MaybeBool::Nothing;
  }
  let rtw = iso_state(context as *mut RealIsolate).rtw;
  let ok = unsafe {
    v8x_hermes_object_set(rtw, slot_of(this), slot_of(key), slot_of(value))
  };
  if ok != 0 {
    MaybeBool::JustTrue
  } else {
    MaybeBool::Nothing
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__Has(
  this: *const Object,
  context: *const Context,
  key: *const Value,
) -> MaybeBool {
  if this.is_null() || context.is_null() || key.is_null() {
    return MaybeBool::Nothing;
  }
  let rtw = iso_state(context as *mut RealIsolate).rtw;
  let r = unsafe { v8x_hermes_object_has(rtw, slot_of(this), slot_of(key)) };
  match r {
    1 => MaybeBool::JustTrue,
    0 => MaybeBool::JustFalse,
    _ => MaybeBool::Nothing,
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Array__New(
  isolate: *mut RealIsolate,
  length: c_int,
) -> *const Array {
  if isolate.is_null() {
    return ptr::null();
  }
  let rtw = iso_state(isolate).rtw;
  let len = if length < 0 { 0 } else { length as i64 };
  let slot = unsafe { v8x_hermes_array_new(rtw, len) };
  slot_ptr::<Array>(slot)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Array__Length(array: *const Array) -> u32 {
  let rtw = current_rtw();
  if rtw.is_null() || array.is_null() {
    return 0;
  }
  let n = unsafe { v8x_hermes_array_length(rtw, slot_of(array)) };
  if n < 0 { 0 } else { n as u32 }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetIndex(
  this: *const Object,
  context: *const Context,
  index: u32,
) -> *const Value {
  if this.is_null() || context.is_null() {
    return ptr::null();
  }
  let rtw = iso_state(context as *mut RealIsolate).rtw;
  let slot = unsafe { v8x_hermes_array_get_index(rtw, slot_of(this), index) };
  slot_ptr::<Value>(slot)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetIndex(
  this: *const Object,
  context: *const Context,
  index: u32,
  value: *const Value,
) -> MaybeBool {
  if this.is_null() || context.is_null() || value.is_null() {
    return MaybeBool::Nothing;
  }
  let rtw = iso_state(context as *mut RealIsolate).rtw;
  let ok = unsafe {
    v8x_hermes_array_set_index(rtw, slot_of(this), index, slot_of(value))
  };
  if ok != 0 {
    MaybeBool::JustTrue
  } else {
    MaybeBool::Nothing
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Number__New(
  isolate: *mut RealIsolate,
  value: f64,
) -> *const Number {
  if isolate.is_null() {
    return ptr::null();
  }
  let rtw = iso_state(isolate).rtw;
  let slot = unsafe { v8x_hermes_number_new(rtw, value) };
  slot_ptr::<Number>(slot)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Number__Value(this: *const Number) -> f64 {
  let rtw = current_rtw();
  if rtw.is_null() || this.is_null() {
    return 0.0;
  }
  let mut out: f64 = 0.0;
  unsafe { v8x_hermes_number_value(rtw, slot_of(this), &mut out) };
  out
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Integer__New(
  isolate: *mut RealIsolate,
  value: i32,
) -> *const Integer {
  v8__Number__New(isolate, value as f64) as *const Integer
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Integer__NewFromUnsigned(
  isolate: *mut RealIsolate,
  value: u32,
) -> *const Integer {
  v8__Number__New(isolate, value as f64) as *const Integer
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Integer__Value(this: *const Integer) -> i64 {
  v8__Number__Value(this as *const Number) as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Boolean__New(
  isolate: *mut RealIsolate,
  value: bool,
) -> *const Boolean {
  if isolate.is_null() {
    return ptr::null();
  }
  let rtw = iso_state(isolate).rtw;
  let slot = unsafe { v8x_hermes_boolean_new(rtw, if value { 1 } else { 0 }) };
  slot_ptr::<Boolean>(slot)
}

/// `Value::BooleanValue`: JS-truthiness coercion of ANY value (v8's
/// contract), not just an already-Boolean handle. `jsi::Value::getBool()`
/// asserts `isBool()`, so a truthiness coercion for non-boolean values would
/// need a small JS helper (`!!v`); the hello-world/C6 surface only calls this
/// on values already known to be booleans, so the shim's `isBool()` check
/// covers that case exactly and returns a distinguishable `false` (matching
/// v8's own default-false-on-non-bool call sites in the vendored surface)
/// otherwise.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__BooleanValue(
  this: *const Value,
  _isolate: *mut RealIsolate,
) -> bool {
  let rtw = current_rtw();
  if rtw.is_null() || this.is_null() {
    return false;
  }
  unsafe { v8x_hermes_boolean_value(rtw, slot_of(this)) == 1 }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsArray(this: *const Value) -> bool {
  let rtw = current_rtw();
  if rtw.is_null() || this.is_null() {
    return false;
  }
  unsafe { v8x_hermes_value_is_array(rtw, slot_of(this)) != 0 }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsFunction(this: *const Value) -> bool {
  let rtw = current_rtw();
  if rtw.is_null() || this.is_null() {
    return false;
  }
  unsafe { v8x_hermes_value_is_function(rtw, slot_of(this)) != 0 }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsNumber(this: *const Value) -> bool {
  let rtw = current_rtw();
  if rtw.is_null() || this.is_null() {
    return false;
  }
  unsafe { v8x_hermes_value_is_number(rtw, slot_of(this)) != 0 }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsBoolean(this: *const Value) -> bool {
  let rtw = current_rtw();
  if rtw.is_null() || this.is_null() {
    return false;
  }
  unsafe { v8x_hermes_value_is_boolean(rtw, slot_of(this)) != 0 }
}

/// `Value::IsString`: routed to the same shim entry `value_is_string` used
/// internally by `EscapeSlot__escape`; wired to the real symbol here now that
/// the wider surface needs it directly (previously `#[allow(dead_code)]`).
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsString(this: *const Value) -> bool {
  let rtw = current_rtw();
  if rtw.is_null() || this.is_null() {
    return false;
  }
  unsafe { v8x_hermes_value_is_string(rtw, slot_of(this)) != 0 }
}

/// `Value::IsUndefined`: routes to `jsi::Value::isUndefined`. Needed by tests
/// that assert a script result is `undefined` (e.g. a `--use_strict` body).
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUndefined(this: *const Value) -> bool {
  let rtw = current_rtw();
  if rtw.is_null() || this.is_null() {
    return false;
  }
  unsafe { v8x_hermes_value_is_undefined(rtw, slot_of(this)) != 0 }
}

/// `Value::IsExternal`: true when the handle holds a v8 `External` (modeled as
/// a JSI HostObject carrying an opaque pointer, see `v8__External__New`).
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsExternal(this: *const Value) -> bool {
  let rtw = current_rtw();
  if rtw.is_null() || this.is_null() {
    return false;
  }
  unsafe { v8x_hermes_value_is_external(rtw, slot_of(this)) != 0 }
}

/// `Value::Uint32Value`: ECMAScript ToUint32 of a numeric value, written into
/// the out-param `Maybe<u32>`. `has_value=false` for non-numbers or errors,
/// matching v8's `Maybe` contract on a failed coercion.
#[repr(C)]
struct MaybeU32 {
  has_value: bool,
  value: u32,
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__Uint32Value(
  this: *const Value,
  context: *const Context,
  out: *mut crate::support::Maybe<u32>,
) {
  let out = out as *mut MaybeU32;
  if out.is_null() {
    return;
  }
  let write = |has_value: bool, value: u32| unsafe {
    ptr::write(out, MaybeU32 { has_value, value });
  };
  if this.is_null() || context.is_null() {
    write(false, 0);
    return;
  }
  let rtw = iso_state(context as *mut RealIsolate).rtw;
  let mut v: u32 = 0;
  let ok = unsafe { v8x_hermes_uint32_value(rtw, slot_of(this), &mut v) };
  if ok != 0 {
    write(true, v);
  } else {
    write(false, 0);
  }
}

// ---- External (v8::External): opaque embedder void* ------------------------
//
// A v8 `External` wraps an opaque embedder `void*` in a JS heap value. Hermes
// (JSI) has no native external value, so the C++ shim models it as a JSI
// HostObject carrying the pointer (see `v8x_hermes_external_new`). Each
// External is a distinct JS object, so two different externals compare unequal
// by JSI object identity (which `v8__Data__EQ` routes through), and reading
// the pointer back is exact.

#[unsafe(no_mangle)]
pub extern "C" fn v8__External__New(
  isolate: *mut RealIsolate,
  value: *mut c_void,
) -> *const External {
  if isolate.is_null() {
    return ptr::null();
  }
  let rtw = iso_state(isolate).rtw;
  let slot = unsafe { v8x_hermes_external_new(rtw, value) };
  slot_ptr::<External>(slot)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__External__Value(this: *const External) -> *mut c_void {
  let rtw = current_rtw();
  if rtw.is_null() || this.is_null() {
    return ptr::null_mut();
  }
  let mut found: c_int = 0;
  unsafe { v8x_hermes_external_value(rtw, slot_of(this), &mut found) }
}

/// `Data::EQ`: equality of two `Data` handles by JSI identity. The vendored
/// `External`/`Context`/etc. `PartialEq` route here (`use identity`). Two
/// handles to the same JS object/value compare equal; different objects do
/// not. Uses the same `jsi::Value::strictEquals` as `v8__Value__StrictEquals`.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__EQ(
  this: *const Data,
  other: *const Data,
) -> bool {
  let rtw = current_rtw();
  if rtw.is_null() {
    return this == other;
  }
  let a = slot_of(this);
  let b = slot_of(other);
  if a < 0 || b < 0 {
    return this == other;
  }
  unsafe { v8x_hermes_strict_equals(rtw, a, b) == 1 }
}

// ---- ArrayBuffer + TypedArray (C8) ----------------------------------------
//
// Hermes/JSI has a real jsi::ArrayBuffer but no C++ factory for a fresh one, so
// ArrayBuffer allocation and every typed-array constructor route through the JS
// `ArrayBuffer`/`Uint8Array`/etc constructors on the runtime's global (see the
// C++ bridge). A `Local<ArrayBuffer>`/`Local<Uint8Array>` is the usual tagged
// handle-table slot pointer. See
// docs/hermes-spike/experiments/C8-hermes-testapi.md.

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__New__with_byte_length(
  isolate: *mut RealIsolate,
  byte_length: usize,
) -> *const ArrayBuffer {
  if isolate.is_null() {
    return ptr::null();
  }
  let rtw = iso_state(isolate).rtw;
  let slot = unsafe { v8x_hermes_array_buffer_new(rtw, byte_length) };
  slot_ptr::<ArrayBuffer>(slot)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__ByteLength(this: *const ArrayBuffer) -> usize {
  let rtw = current_rtw();
  if rtw.is_null() || this.is_null() {
    return 0;
  }
  let n = unsafe { v8x_hermes_array_buffer_byte_length(rtw, slot_of(this)) };
  if n == usize::MAX { 0 } else { n }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__Data(this: *const ArrayBuffer) -> *mut c_void {
  let rtw = current_rtw();
  if rtw.is_null() || this.is_null() {
    return ptr::null_mut();
  }
  unsafe { v8x_hermes_array_buffer_data(rtw, slot_of(this)) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TypedArray__Length(this: *const Value) -> usize {
  let rtw = current_rtw();
  if rtw.is_null() || this.is_null() {
    return 0;
  }
  let n = unsafe { v8x_hermes_typed_array_length(rtw, slot_of(this)) };
  if n == usize::MAX { 0 } else { n }
}

/// Shared body for the twelve `v8__<Name>Array__New` constructors: call the JS
/// constructor `ctor_name` (a `&CStr`) with `(buffer, byte_offset, length)`.
#[inline]
fn typed_array_new(
  ctor_name: &std::ffi::CStr,
  buf_ptr: *const ArrayBuffer,
  byte_offset: usize,
  length: usize,
) -> i64 {
  let rtw = current_rtw();
  if rtw.is_null() || buf_ptr.is_null() {
    return NULL_SLOT;
  }
  unsafe {
    v8x_hermes_typed_array_new(
      rtw,
      ctor_name.as_ptr(),
      slot_of(buf_ptr),
      byte_offset,
      length,
    )
  }
}

/// A `&'static CStr` literal (avoids allocating a CString per call).
macro_rules! c_str {
  ($s:literal) => {{
    // SAFETY: the literal has no interior NUL and ends with one.
    unsafe {
      std::ffi::CStr::from_bytes_with_nul_unchecked(concat!($s, "\0").as_bytes())
    }
  }};
}

/// Define `v8__<Name>Array__New` for each JS typed-array constructor. Each maps
/// a v8 `<Name>Array::new(buf, offset, len)` to `new <Name>Array(buf, ...)` on
/// the Hermes global. The return type is the concrete rusty_v8 view struct, but
/// every value is the same tagged handle-table slot pointer.
macro_rules! hermes_typed_array_new {
  ($fn_name:ident, $view:ident, $ctor:literal) => {
    #[unsafe(no_mangle)]
    pub extern "C" fn $fn_name(
      buf_ptr: *const ArrayBuffer,
      byte_offset: usize,
      length: usize,
    ) -> *const crate::$view {
      let slot = typed_array_new(
        c_str!($ctor),
        buf_ptr,
        byte_offset,
        length,
      );
      slot_ptr::<crate::$view>(slot)
    }
  };
}

hermes_typed_array_new!(v8__Uint8Array__New, Uint8Array, "Uint8Array");
hermes_typed_array_new!(
  v8__Uint8ClampedArray__New,
  Uint8ClampedArray,
  "Uint8ClampedArray"
);
hermes_typed_array_new!(v8__Int8Array__New, Int8Array, "Int8Array");
hermes_typed_array_new!(v8__Uint16Array__New, Uint16Array, "Uint16Array");
hermes_typed_array_new!(v8__Int16Array__New, Int16Array, "Int16Array");
hermes_typed_array_new!(v8__Uint32Array__New, Uint32Array, "Uint32Array");
hermes_typed_array_new!(v8__Int32Array__New, Int32Array, "Int32Array");
hermes_typed_array_new!(v8__Float32Array__New, Float32Array, "Float32Array");
hermes_typed_array_new!(v8__Float64Array__New, Float64Array, "Float64Array");
hermes_typed_array_new!(
  v8__BigUint64Array__New,
  BigUint64Array,
  "BigUint64Array"
);
hermes_typed_array_new!(v8__BigInt64Array__New, BigInt64Array, "BigInt64Array");

// Float16Array is behind V8's --js-float16array flag and not a JS global in
// Hermes, but the vendored `typed_array!(Float16Array)` still declares the
// symbol, so it must link. Route it through the same path (it returns the null
// slot when the `Float16Array` global is absent, rather than aborting).
hermes_typed_array_new!(v8__Float16Array__New, Float16Array, "Float16Array");

/// `Function::Call`: JSI's `Function::call`/`callWithThis`. `recv` may be
/// null (v8 passes a null receiver for `undefined`), in which case the shim
/// uses the undefined-`this` call path. Returns the null handle on any error
/// (not a function, a bad slot, or the call threw a `jsi::JSError`/other C++
/// exception, both caught at the C2 boundary).
#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__Call(
  this: *const Function,
  context: *const Context,
  recv: *const Value,
  argc: c_int,
  argv: *const *const Value,
) -> *const Value {
  if this.is_null() || context.is_null() {
    return ptr::null();
  }
  let rtw = iso_state(context as *mut RealIsolate).rtw;
  let n = if argc < 0 { 0 } else { argc as usize };
  let mut arg_slots: Vec<i64> = Vec::with_capacity(n);
  for i in 0..n {
    let v = unsafe { *argv.add(i) };
    if v.is_null() {
      return ptr::null();
    }
    arg_slots.push(slot_of(v));
  }
  let recv_slot = if recv.is_null() { NULL_SLOT } else { slot_of(recv) };
  let mut ok: c_int = 0;
  let out = unsafe {
    v8x_hermes_function_call(
      rtw,
      slot_of(this as *const Value),
      recv_slot,
      arg_slots.as_ptr(),
      n,
      &mut ok,
    )
  };
  if ok == 0 || out < 0 {
    return ptr::null();
  }
  slot_ptr::<Value>(out)
}

// ---- Platform / V8 lifecycle ----------------------------------------------
//
// Hermes manages its own runtime and needs no V8 platform. But the vendored
// `V8::initialize_platform`/`initialize` state machine still calls these
// symbols, and `new_default_platform(0,false).make_shared()` routes a
// `UniquePtr<Platform>` through a shared-pointer. These are minimal
// no-op/pointer-carrying versions mirroring src/quickjs/init.rs, enough for a
// test to bring the isolate up. The `Platform` object is an inert marker box.

/// Inert platform marker (Hermes has no V8 platform). Its box address is the
/// `*mut Platform` the shared-pointer carries.
struct HermesPlatform;

fn new_platform() -> *mut Platform {
  Box::into_raw(Box::new(HermesPlatform)) as *mut Platform
}

unsafe fn drop_platform(platform: *mut Platform) {
  if !platform.is_null() {
    unsafe { drop(Box::from_raw(platform as *mut HermesPlatform)) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__NewDefaultPlatform(
  _thread_pool_size: c_int,
  _idle_task_support: bool,
) -> *mut Platform {
  new_platform()
}

/// `new_unprotected_default_platform`: Hermes has no V8 platform, so the
/// "unprotected" distinction (a V8 code-space memory-protection setting) is
/// meaningless here; it returns the same inert marker as the default platform.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__NewUnprotectedDefaultPlatform(
  _thread_pool_size: c_int,
  _idle_task_support: bool,
) -> *mut Platform {
  new_platform()
}

/// `new_single_threaded_default_platform`: same inert marker (Hermes runs
/// single-threaded through JSI regardless).
#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__NewSingleThreadedDefaultPlatform(
  _idle_task_support: bool,
) -> *mut Platform {
  new_platform()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__InitializePlatform(_platform: *mut Platform) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__Initialize() {}

/// Process-global "run every script in strict mode" flag, set by
/// `--use_strict`. V8 applies `--use_strict` by making every top-level script
/// strict; Hermes has no such flag, so `Script::Compile` prepends a
/// `"use strict";` directive when this is set. Each rusty_v8 test target is its
/// own process, so this global is per-target (only the api_flags target sets
/// it).
static USE_STRICT: std::sync::atomic::AtomicBool =
  std::sync::atomic::AtomicBool::new(false);

/// `V8::set_flags_from_string`: V8 command-line flags do not apply to Hermes,
/// which has its own runtime configuration. Most flags the tests pass
/// (`--expose_gc`, `--allow-natives-syntax`, etc.) either match Hermes's own
/// defaults or exercise machinery those tests only reach through other
/// symbols, so they are accepted and ignored. `--use_strict` is the one flag
/// with observable top-level semantics (it makes scripts strict), so it is
/// honored via the `USE_STRICT` flag above.
#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__SetFlagsFromString(
  flags: *const u8,
  length: usize,
) {
  if flags.is_null() {
    return;
  }
  let bytes = unsafe { std::slice::from_raw_parts(flags, length) };
  if let Ok(s) = std::str::from_utf8(bytes) {
    if s.split_whitespace().any(|f| f == "--use_strict") {
      USE_STRICT.store(true, Ordering::Relaxed);
    }
  }
}

/// `Isolate::perform_microtask_checkpoint`: drain the JS microtask queue.
/// Hermes runs microtasks (promise jobs) as part of `evaluateJavaScript` and
/// exposes no separate embedder drain entry point through JSI, so for the
/// current surface this is a no-op: the tests that call it (the `slots`
/// layer1/layer2 Deno-pattern tests) only use it to prove `Isolate` methods
/// are reachable via `Deref`, not to observe queued microtasks.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__PerformMicrotaskCheckpoint(
  _isolate: *mut RealIsolate,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__Dispose() -> bool {
  true
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__DisposePlatform() {}

/// `V8::get_version()`. Reports the V8 version string the vendored rusty_v8
/// surface was generated against (`crate::VERSION_STRING`), so the round-trip
/// exact-match test holds. Returning null here would SEGV in the vendored
/// `CStr::from_ptr(null)`, so this is also a process-crash guard.
#[unsafe(no_mangle)]
pub extern "C" fn v8__V8__GetVersion() -> *const c_char {
  use std::sync::OnceLock;
  static VERSION: OnceLock<std::ffi::CString> = OnceLock::new();
  VERSION
    .get_or_init(|| std::ffi::CString::new(crate::VERSION_STRING).unwrap())
    .as_ptr()
}

/// Called on isolate teardown; Hermes needs no platform notification.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__NotifyIsolateShutdown(
  _platform: *mut Platform,
  _isolate: *mut c_void,
) {
}

// The shared-pointer over a `Platform`. Same [ptr, refcount] repr the QuickJS
// backend uses, so the vendored `SharedPtrBase`/`SharedRef` bit-layout matches.
#[repr(C)]
struct PlatformSharedRepr {
  platform: *mut c_void,
  refcount: *mut usize,
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__Platform__CONVERT__std__unique_ptr(
  unique_ptr: UniquePtr<Platform>,
) -> SharedPtrBase<Platform> {
  let raw = unique_ptr.into_raw() as *mut c_void;
  let repr = if raw.is_null() {
    PlatformSharedRepr {
      platform: ptr::null_mut(),
      refcount: ptr::null_mut(),
    }
  } else {
    PlatformSharedRepr {
      platform: raw,
      refcount: Box::into_raw(Box::new(1usize)),
    }
  };
  unsafe { std::mem::transmute::<PlatformSharedRepr, SharedPtrBase<Platform>>(repr) }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__Platform__get(
  ptr: *const SharedPtrBase<Platform>,
) -> *mut Platform {
  if ptr.is_null() {
    return ptr::null_mut();
  }
  let repr = ptr as *const PlatformSharedRepr;
  unsafe { (*repr).platform as *mut Platform }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__Platform__COPY(
  ptr: *const SharedPtrBase<Platform>,
) -> SharedPtrBase<Platform> {
  if ptr.is_null() {
    return SharedPtrBase::default();
  }
  let repr = ptr as *const PlatformSharedRepr;
  let (platform, refcount) = unsafe { ((*repr).platform, (*repr).refcount) };
  if !refcount.is_null() {
    unsafe { *refcount += 1 };
  }
  let copy = PlatformSharedRepr { platform, refcount };
  unsafe { std::mem::transmute::<PlatformSharedRepr, SharedPtrBase<Platform>>(copy) }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__Platform__reset(
  ptr: *mut SharedPtrBase<Platform>,
) {
  if ptr.is_null() {
    return;
  }
  let repr = ptr as *mut PlatformSharedRepr;
  unsafe {
    let refcount = (*repr).refcount;
    if !refcount.is_null() {
      *refcount -= 1;
      if *refcount == 0 {
        drop(Box::from_raw(refcount));
        drop_platform((*repr).platform as *mut Platform);
      }
    }
    (*repr).platform = ptr::null_mut();
    (*repr).refcount = ptr::null_mut();
  }
}

// ---- CreateParams + ArrayBuffer allocator ---------------------------------
//
// The vendored `Isolate::new(CreateParams::default())` path needs the
// CreateParams size/zero-init and a default array-buffer allocator (which it
// wraps in a shared_ptr). Hermes owns its own heap and the hello-world path
// never touches an ArrayBuffer, so the allocator is an inert marker box; the
// shared-pointer helpers mirror src/quickjs/allocator.rs's [obj, ctrl] word
// layout so the vendored `SharedPtrBase<Allocator>` bit-layout matches.

/// Inert allocator marker (Hermes manages memory itself).
struct HermesAllocator;

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__CreateParams__SIZEOF() -> usize {
  std::mem::size_of::<crate::isolate_create_params::raw::CreateParams>()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__CreateParams__CONSTRUCT(
  buf: *mut std::mem::MaybeUninit<
    crate::isolate_create_params::raw::CreateParams,
  >,
) {
  if !buf.is_null() {
    unsafe {
      ptr::write_bytes(
        buf as *mut u8,
        0,
        std::mem::size_of::<crate::isolate_create_params::raw::CreateParams>(),
      );
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__Allocator__NewDefaultAllocator()
-> *mut Allocator {
  Box::into_raw(Box::new(HermesAllocator)) as *mut Allocator
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__Allocator__DELETE(this: *mut Allocator) {
  if !this.is_null() {
    unsafe { drop(Box::from_raw(this as *mut HermesAllocator)) };
  }
}

// Shared-ptr repr is [obj: *mut Allocator, ctrl: *mut AtomicUsize].
fn alloc_read_words(ptr: *const SharedPtrBase<Allocator>) -> (usize, usize) {
  if ptr.is_null() {
    return (0, 0);
  }
  let w = ptr as *const usize;
  unsafe { (*w, *w.add(1)) }
}

unsafe fn alloc_write_words(
  ptr: *mut SharedPtrBase<Allocator>,
  obj: usize,
  ctrl: usize,
) {
  let w = ptr as *mut usize;
  unsafe {
    *w = obj;
    *w.add(1) = ctrl;
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__ArrayBuffer__Allocator__CONVERT__std__unique_ptr(
  unique_ptr: UniquePtr<Allocator>,
) -> SharedPtrBase<Allocator> {
  let raw = unique_ptr.into_raw();
  let mut out: SharedPtrBase<Allocator> = Default::default();
  if raw.is_null() {
    return out;
  }
  let ctrl = Box::into_raw(Box::new(AtomicUsize::new(1)));
  unsafe { alloc_write_words(&mut out, raw as usize, ctrl as usize) };
  out
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__ArrayBuffer__Allocator__COPY(
  ptr: *const SharedPtrBase<Allocator>,
) -> SharedPtrBase<Allocator> {
  let (obj, ctrl) = alloc_read_words(ptr);
  if ctrl != 0 {
    unsafe { (*(ctrl as *const AtomicUsize)).fetch_add(1, Ordering::Relaxed) };
  }
  let mut out: SharedPtrBase<Allocator> = Default::default();
  unsafe { alloc_write_words(&mut out, obj, ctrl) };
  out
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__ArrayBuffer__Allocator__get(
  ptr: *const SharedPtrBase<Allocator>,
) -> *mut Allocator {
  alloc_read_words(ptr).0 as *mut Allocator
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__ArrayBuffer__Allocator__reset(
  ptr: *mut SharedPtrBase<Allocator>,
) {
  if ptr.is_null() {
    return;
  }
  let (obj, ctrl) = alloc_read_words(ptr);
  if ctrl != 0 {
    let prev =
      unsafe { (*(ctrl as *const AtomicUsize)).fetch_sub(1, Ordering::AcqRel) };
    if prev == 1 {
      if obj != 0 {
        v8__ArrayBuffer__Allocator__DELETE(obj as *mut Allocator);
      }
      unsafe { drop(Box::from_raw(ctrl as *mut AtomicUsize)) };
    }
  }
  unsafe { alloc_write_words(ptr, 0, 0) };
}
