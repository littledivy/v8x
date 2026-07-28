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

use crate::support::{MaybeBool, SharedPtrBase, SharedRef};
use crate::{
  Allocator, Array, ArrayBuffer, BackingStore, BackingStoreDeleterCallback,
  Boolean, Context, Data, External, Function, FunctionCallback,
  FunctionCallbackInfo, FunctionTemplate, Integer, Message, MicrotaskQueue,
  Name, Number, Object, ObjectTemplate, OneByteConst, Platform, Primitive,
  Promise, PromiseResolver, PromiseState, RealIsolate, Script,
  String as V8String, Template, UniquePtr, Value,
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
  fn v8x_hermes_set_slot(rtw: *mut c_void, dst: i64, src: i64) -> c_int;
  fn v8x_hermes_slot_dup(rtw: *mut c_void, src: i64) -> i64;
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
  // D3: durable JS-value pins (shared with modules.rs). Used to keep the
  // per-context extras binding object alive across HandleScope pops (C2).
  fn v8x_hermes_pin(rtw: *mut c_void, slot: i64) -> i64;
  fn v8x_hermes_pin_get(rtw: *mut c_void, pin_id: i64) -> i64;
  fn v8x_hermes_pin_addref(rtw: *mut c_void, pin_id: i64) -> i64;
  fn v8x_hermes_unpin(rtw: *mut c_void, pin_id: i64);
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
  fn v8x_hermes_value_is_promise(rtw: *mut c_void, slot: i64) -> c_int;
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
  fn v8x_hermes_value_is_null(rtw: *mut c_void, slot: i64) -> c_int;
  fn v8x_hermes_uint32_value(
    rtw: *mut c_void,
    slot: i64,
    out: *mut u32,
  ) -> c_int;

  // C8: ArrayBuffer + TypedArray. See
  // docs/hermes-spike/experiments/C8-hermes-testapi.md.
  fn v8x_hermes_array_buffer_new(rtw: *mut c_void, byte_length: usize) -> i64;
  fn v8x_hermes_array_buffer_new_external(
    rtw: *mut c_void,
    data: *mut c_void,
    byte_length: usize,
    deleter: Option<
      unsafe extern "C" fn(*mut c_void, usize, *mut c_void),
    >,
    deleter_data: *mut c_void,
  ) -> i64;
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

  // C9: TryCatch / exception surfacing. See
  // docs/hermes-spike/experiments/C9-hermes-trycatch.md.
  fn v8x_hermes_trycatch_push(rtw: *mut c_void) -> i64;
  fn v8x_hermes_trycatch_pop(rtw: *mut c_void, index: i64);
  fn v8x_hermes_trycatch_has_caught(rtw: *mut c_void, index: i64) -> c_int;
  fn v8x_hermes_trycatch_exception(rtw: *mut c_void, index: i64) -> i64;
  fn v8x_hermes_trycatch_message(rtw: *mut c_void, index: i64) -> i64;
  fn v8x_hermes_trycatch_stack_trace(rtw: *mut c_void, index: i64) -> i64;
  fn v8x_hermes_trycatch_reset(rtw: *mut c_void, index: i64);
  fn v8x_hermes_trycatch_rethrow(rtw: *mut c_void, index: i64) -> i64;
  fn v8x_hermes_throw_exception(rtw: *mut c_void, value_slot: i64) -> c_int;
  fn v8x_hermes_exception_new(
    rtw: *mut c_void,
    ctor_name: *const c_char,
    message_slot: i64,
  ) -> i64;

  // C10: native function callbacks. See
  // docs/hermes-spike/experiments/C10-hermes-callbacks.md.
  fn v8x_hermes_function_new(
    rtw: *mut c_void,
    callback_bits: usize,
    data_slot: i64,
    length: i32,
    name: *const c_char,
    instance_internal_field_count: i64,
    template_id: i64,
    signature_templ_id: i64,
  ) -> i64;
  fn v8x_hermes_set_pending_callback_exception(rtw: *mut c_void, slot: i64);
  // C12 Signature stamp/check (v8x_hermes_stamp_template_id/
  // v8x_hermes_check_signature) are called only from hermes_shim.cpp itself
  // (inside v8x_hermes_function_new's hostFn), so they have no Rust-side
  // extern "C" declaration here - only their C++ definitions
  // (docs/hermes-spike/experiments/C12-hermes-interceptors.md).

  // C11: ObjectTemplate internal fields + accessors. See
  // docs/hermes-spike/experiments/C11-hermes-templates.md.
  fn v8x_hermes_object_new_with_internal_fields(
    rtw: *mut c_void,
    count: i64,
  ) -> i64;
  // Called from the C++ constructor host-function path, not from Rust.
  #[allow(dead_code)]
  fn v8x_hermes_object_ensure_internal_fields(
    rtw: *mut c_void,
    slot: i64,
    count: i64,
  ) -> c_int;
  fn v8x_hermes_object_internal_field_count(
    rtw: *mut c_void,
    slot: i64,
  ) -> i64;
  fn v8x_hermes_object_get_internal_field(
    rtw: *mut c_void,
    slot: i64,
    index: i64,
  ) -> i64;
  fn v8x_hermes_object_set_internal_field(
    rtw: *mut c_void,
    slot: i64,
    index: i64,
    value_slot: i64,
  ) -> c_int;
  fn v8x_hermes_object_define_property(
    rtw: *mut c_void,
    obj_slot: i64,
    key_slot: i64,
    value_slot: i64,
    attr: c_int,
  ) -> c_int;
  fn v8x_hermes_object_define_accessor(
    rtw: *mut c_void,
    obj_slot: i64,
    key_slot: i64,
    getter_bits: usize,
    setter_bits: usize,
    data_slot: i64,
    attr: c_int,
  ) -> c_int;
  fn v8x_hermes_function_set_name(
    rtw: *mut c_void,
    fn_slot: i64,
    name_slot: i64,
  ) -> c_int;
  fn v8x_hermes_set_prototype_from_ctor(
    rtw: *mut c_void,
    obj_slot: i64,
    ctor_slot: i64,
  ) -> c_int;
  fn v8x_hermes_object_set_prototype(
    rtw: *mut c_void,
    obj_slot: i64,
    proto_slot: i64,
  ) -> c_int;
  fn v8x_hermes_object_get_prototype(rtw: *mut c_void, obj_slot: i64) -> i64;
  fn v8x_hermes_object_define_accessor_fns(
    rtw: *mut c_void,
    obj_slot: i64,
    key_slot: i64,
    getter_fn_slot: i64,
    setter_fn_slot: i64,
    attr: c_int,
  ) -> c_int;

  // D1: Promises + microtask queue. See
  // docs/hermes-spike/experiments/D1-hermes-promises.md.
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
  fn v8x_hermes_promise_resolver_reject(
    rtw: *mut c_void,
    resolver_slot: i64,
    value_slot: i64,
  ) -> c_int;
  fn v8x_hermes_promise_state(rtw: *mut c_void, promise_slot: i64) -> c_int;
  fn v8x_hermes_promise_result(rtw: *mut c_void, promise_slot: i64) -> i64;
  fn v8x_hermes_promise_then(
    rtw: *mut c_void,
    promise_slot: i64,
    handler_slot: i64,
  ) -> i64;
  fn v8x_hermes_promise_catch(
    rtw: *mut c_void,
    promise_slot: i64,
    handler_slot: i64,
  ) -> i64;
  fn v8x_hermes_promise_then2(
    rtw: *mut c_void,
    promise_slot: i64,
    on_fulfilled_slot: i64,
    on_rejected_slot: i64,
  ) -> i64;
  fn v8x_hermes_promise_has_handler(rtw: *mut c_void, promise_slot: i64) -> c_int;
  fn v8x_hermes_promise_mark_handled(rtw: *mut c_void, promise_slot: i64);
  fn v8x_hermes_enqueue_microtask(rtw: *mut c_void, fn_slot: i64) -> c_int;
  fn v8x_hermes_drain_microtasks(rtw: *mut c_void) -> c_int;
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
  /// C10: a pending exception (handle-table slot) set by
  /// `Isolate::ThrowException` while a native FunctionCallback is running.
  /// The callback-dispatch trampoline reads and clears it, then re-throws it
  /// as a `jsi::JSError` on the C++ side so it propagates through JSI (and is
  /// caught by any enclosing TryCatch via the normal C9 path). -1 = none.
  pub pending_exception: i64,
  /// D2: every ES-module / ModuleRequest / FixedArray record this isolate has
  /// created, as erased boxed pointers with their drop glue. Modules are
  /// Rust-owned records whose raw pointer IS the v8 Local handle (they never go
  /// through the JSI handle table), so they must be freed here at Dispose. See
  /// docs/hermes-spike/experiments/D2-hermes-modules.md.
  pub module_records: Vec<Box<dyn FnOnce()>>,
  /// D3: the per-context "extras binding object" (V8's
  /// `Context::GetExtrasBindingObject`). deno_core's bootstrap reads built-ins
  /// (e.g. the console) off this object. V8 returns the SAME object every call,
  /// so it is created lazily on first use, pinned into a runtime-owned durable
  /// slot (C2 lifetime), and every call returns a fresh Local resolving to that
  /// one pinned object (C4 identity). -1 = not created yet.
  pub extras_binding_pin: i64,
}

thread_local! {
  /// The current entered isolate for this thread (set by Enter, cleared by
  /// Exit). There is at most one live Hermes isolate per thread.
  static CURRENT_ISO: Cell<*mut RealIsolate> = const { Cell::new(ptr::null_mut()) };
  /// Isolate re-entrancy stack (C9 fix): real V8's Enter/Exit is a NESTING
  /// counter, not a flat set/clear - `Isolate::Enter` can be called again
  /// while already entered (e.g. `Exception::type_error`'s internal
  /// `scope.enter()`/`scope.exit()` bracketing around a constructor call),
  /// and `Exit` must restore whatever was current BEFORE that nested Enter,
  /// not unconditionally null it out (which would otherwise leave
  /// `CURRENT_ISO` null for the rest of the enclosing scope, breaking every
  /// later `current_iso()`-dependent call including the isolate's own
  /// disposal-order assert). Push on Enter, pop-and-restore on Exit.
  static ISO_STACK: std::cell::RefCell<Vec<*mut RealIsolate>> =
    const { std::cell::RefCell::new(Vec::new()) };
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
pub(super) fn slot_ptr<T>(slot: i64) -> *const T {
  if slot < 0 {
    return ptr::null();
  }
  (((slot as usize) << 1) | 1) as *const T
}

/// Recover the slot index from a tagged `Local` pointer, or `NULL_SLOT`.
#[inline]
pub(super) fn slot_of<T>(ptr: *const T) -> i64 {
  let bits = ptr as usize;
  if bits == 0 || (bits & 1) == 0 {
    return NULL_SLOT;
  }
  (bits >> 1) as i64
}

/// The `RuntimeWrapper*` for the current thread's isolate, or null.
#[inline]
pub(super) fn current_rtw() -> *mut c_void {
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
    pending_exception: NULL_SLOT,
    module_records: Vec::new(),
    extras_binding_pin: -1,
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
  // D2: run every module record's drop glue (unpins its JS values) BEFORE the
  // runtime is freed, since unpin touches the still-live runtime.
  let mut st = unsafe { Box::from_raw(this as *mut IsoState) };
  for drop_fn in st.module_records.drain(..) {
    drop_fn();
  }
  // Drop the C++ runtime wrapper (clears the handle table Values while the
  // runtime is alive), then the Rust box.
  unsafe { v8x_hermes_runtime_free(st.rtw) };
  if current_iso() == this {
    CURRENT_ISO.with(|c| c.set(ptr::null_mut()));
  }
  // Drop any stale re-entrancy stack entries for a disposed isolate (should
  // normally already be empty - every Enter is paired with an Exit - but this
  // guards against a leaked frame outliving Dispose).
  ISO_STACK.with(|s| s.borrow_mut().retain(|&iso| iso != this));
}

/// `Isolate::Enter`: real V8 nesting semantics (see `ISO_STACK` doc comment)
/// - pushes whatever was current before this Enter, then makes `this`
/// current. A matching `Exit` restores exactly what was pushed here.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__Enter(this: *mut RealIsolate) {
  let previous = current_iso();
  ISO_STACK.with(|s| s.borrow_mut().push(previous));
  CURRENT_ISO.with(|c| c.set(this));
}

/// `Isolate::Exit`: pop the re-entrancy stack and restore whatever was
/// current before the matching `Enter`, rather than unconditionally nulling
/// `CURRENT_ISO` out (which would incorrectly clobber an outer, still-live
/// entered isolate - see `ISO_STACK` doc comment for the concrete failure
/// this fixes: `Exception::type_error`'s internal enter/exit bracketing).
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__Exit(_this: *mut RealIsolate) {
  let previous = ISO_STACK.with(|s| s.borrow_mut().pop()).unwrap_or(ptr::null_mut());
  CURRENT_ISO.with(|c| c.set(previous));
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
  isolate: *mut RealIsolate,
  data: *const Data,
) -> *const Data {
  global_new(isolate, data)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Global__NewWeak(
  isolate: *mut RealIsolate,
  data: *const Data,
  _parameter: *const c_void,
  _callback: unsafe extern "C" fn(*const c_void),
) -> *const Data {
  // A weak Global still needs its referent kept alive for the boot path (the
  // backend has no real GC weak-ref semantics; treat weak like strong). This
  // matches the conservative behavior elsewhere in the shim.
  global_new(isolate, data)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Global__Reset(data: *const Data) {
  // Free the durable pin backing a value Global (no-op for a non-value handle).
  if let Some(pin_id) = global_pin_id(data) {
    let rtw = current_rtw();
    if !rtw.is_null() {
      unsafe { v8x_hermes_unpin(rtw, pin_id) };
    }
  }
}

// ---- Global durable pins (C2 lifetime) -------------------------------------
//
// A v8 `Global` must outlive the HandleScope its value was created in. The
// handle table is scope-managed (a watermark truncates every slot created since
// the scope opened), so a Global that merely carried the source slot pointer
// would dangle after the creating scope popped: a later `Local::New` would read
// a truncated/reused slot (the exact deno_core boot bug where a stored
// `ext_import_meta_proto` object read back as a non-object).
//
// So a value Global durably PINS its JSI value (the D2 pin infra) and encodes
// the pin id in the returned handle. A non-value handle (Context == isolate
// pointer, Module == Box pointer; both >= 8-byte aligned, low 3 bits 0) is
// already stable and is returned unchanged.
//
// Encoding: a global-pin handle is `(pin_id << 2) | 0b10`. Bit 1 set marks a
// pin; ordinary value slots are `(i << 1) | 1` (bit 0 set) and aligned
// Context/Module pointers have bits 0..2 clear, so neither collides.

const GLOBAL_PIN_TAG: usize = 0b10;

#[inline]
fn global_pin_ptr(pin_id: i64) -> *const Data {
  (((pin_id as usize) << 2) | GLOBAL_PIN_TAG) as *const Data
}

/// If `ptr` is a global-pin handle, return its pin id; otherwise None.
#[inline]
fn global_pin_id(ptr: *const Data) -> Option<i64> {
  let bits = ptr as usize;
  if bits & 0b11 == GLOBAL_PIN_TAG {
    Some((bits >> 2) as i64)
  } else {
    None
  }
}

fn global_new(isolate: *mut RealIsolate, data: *const Data) -> *const Data {
  if data.is_null() {
    return ptr::null();
  }
  // Already a global pin (re-wrapping a Global, e.g. `Global::clone`): share the
  // same pin but bump its refcount, so the holder survives until the LAST alias
  // drops (each alias's `Global::drop` calls `v8__Global__Reset` -> `unpin`).
  if let Some(pin_id) = global_pin_id(data) {
    let rtw = if isolate.is_null() {
      current_rtw()
    } else {
      iso_state(isolate).rtw
    };
    if !rtw.is_null() {
      unsafe { v8x_hermes_pin_addref(rtw, pin_id) };
    }
    return data;
  }
  let src = slot_of(data);
  if src < 0 {
    // Non-value handle (Context / Module record): stable, identity.
    return data;
  }
  let rtw = if isolate.is_null() {
    current_rtw()
  } else {
    iso_state(isolate).rtw
  };
  if rtw.is_null() {
    return data;
  }
  let pin_id = unsafe { v8x_hermes_pin(rtw, src) };
  if pin_id < 0 {
    // Pin failed: fall back to identity (best-effort, matches prior behavior).
    return data;
  }
  global_pin_ptr(pin_id)
}

/// Re-materialize a handle into a live `Local` in the current scope. deno_core
/// calls this to turn a `Global<Context>` (and other Globals) back into a
/// `Local` during boot. Three handle shapes:
///   * null -> null.
///   * a non-value handle (even-aligned: a Context == isolate pointer, or a
///     Box-backed Module/record) -> returned as-is; these are stable, not
///     scope-managed, so no re-interning is needed.
///   * a value handle (odd-aligned tagged slot in the JSI handle table) -> its
///     JSI value is duplicated into a fresh slot in the current scope so the
///     new Local outlives the source handle's scope.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Local__New(
  isolate: *mut RealIsolate,
  other: *const Data,
) -> *const Data {
  if other.is_null() {
    return ptr::null();
  }
  let rtw = if isolate.is_null() {
    current_rtw()
  } else {
    iso_state(isolate).rtw
  };
  // A Global backed by a durable pin (value Global): materialize the pinned
  // value into a fresh slot in the current scope, so the Local is a live handle
  // that outlives nothing it must not (it lives until the current scope pops).
  if let Some(pin_id) = global_pin_id(other) {
    if rtw.is_null() {
      return other;
    }
    let slot = unsafe { v8x_hermes_pin_get(rtw, pin_id) };
    if slot < 0 {
      return ptr::null();
    }
    return slot_ptr::<Data>(slot);
  }
  let src = slot_of(other);
  if src < 0 {
    // Non-value handle (Context / Module record): identity.
    return other;
  }
  if rtw.is_null() {
    return other;
  }
  let dup = unsafe { v8x_hermes_slot_dup(rtw, src) };
  if dup < 0 {
    return other;
  }
  slot_ptr::<Data>(dup)
}

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

// EscapableHandleScope: reserve a slot in the PARENT, then move an escaping
// handle's value into it on `escape()`. `reserve` runs BEFORE the child
// scope's HandleScope records its watermark (see the vendored
// EscapableHandleScope construction order: `raw::EscapeSlot::new` precedes
// `HandleScope::init`), so a slot pushed here lives BELOW the child watermark
// and survives the child scope's truncate-on-exit. `escape` overwrites that
// reserved slot with a copy of the escaping value and returns a handle to it.
//
// The earlier C3 implementation reserved nothing (returned a sentinel) and, on
// escape, re-interned the value via value_to_utf8 into a slot ABOVE the child
// watermark - which was both string-only/lossy AND reclaimed by the child
// truncate, so any escape of a non-string value produced an empty string (this
// broke object_template's escaped block-completion string, the first test to
// actually depend on escape's return value).
#[unsafe(no_mangle)]
pub extern "C" fn v8__EscapeSlot__reserve(isolate: *mut RealIsolate) -> usize {
  if isolate.is_null() {
    return usize::MAX;
  }
  let rtw = iso_state(isolate).rtw;
  // Push an undefined placeholder in the parent; its index is the reserved
  // slot escape() will overwrite.
  let slot = unsafe { v8x_hermes_undefined(rtw) };
  if slot < 0 {
    return usize::MAX;
  }
  slot as usize
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__EscapeSlot__escape(
  isolate: *mut RealIsolate,
  index: usize,
  value: *const Data,
) -> *const Data {
  if isolate.is_null() || value.is_null() {
    return value;
  }
  let src = slot_of(value);
  if src < 0 || index == usize::MAX {
    return value;
  }
  let rtw = iso_state(isolate).rtw;
  let ok = unsafe { v8x_hermes_set_slot(rtw, index as i64, src) };
  if ok == 0 {
    return value;
  }
  slot_ptr::<Data>(index as i64)
}

// ---- Context ---------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__New(
  isolate: *mut RealIsolate,
  templ: *const c_void,
  _global_object: *const c_void,
  _microtask_queue: *mut c_void,
) -> *const Context {
  // One context per Hermes runtime; its handle is the isolate pointer.
  let ctx = isolate as *const Context;
  // C11: if a global ObjectTemplate was supplied, apply its Template::Set
  // properties (and accessors) onto the context's global object at
  // context-creation time (matches context_from_object_template: `f()` must
  // resolve to the FunctionTemplate stored under "f" on the global).
  if !isolate.is_null() && template_kind(templ) == Some(TEMPLATE_KIND_OBJ) {
    apply_template_to_global(templ as *const ObjectTemplate, ctx);
  }
  // V8 exposes a built-in `globalThis.console`. deno_core's 01_core.js reads it
  // (`const v8Console = globalThis.console; wrapConsole(coreConsole, v8Console)`,
  // which does `ObjectKeys(v8Console)`), so an undefined `console` throws
  // "Cannot convert undefined value to object". Hermes has no built-in console,
  // so synthesize a minimal one on the global (same no-op console the extras
  // binding object carries; deno_core forwards real console output through its
  // own op-based console, these are the fallback sinks).
  if !isolate.is_null() {
    install_global_console(ctx);
  }
  ctx
}

/// The JS source for a minimal console: an object of no-op methods, with the
/// method names as ENUMERABLE own properties so deno_core's `wrapConsole`
/// (`ObjectKeys(console)`) can enumerate them. Shared by the global console and
/// the extras-binding console.
const CONSOLE_LITERAL_SRC: &str = "(function(){var c={};var m=['log','info',\
  'debug','error','warn','dir','dirxml','table','trace','group',\
  'groupCollapsed','groupEnd','clear','count','countReset','assert','profile',\
  'profileEnd','time','timeLog','timeEnd','timeStamp'];\
  for(var i=0;i<m.length;i++){c[m[i]]=function(){};}return c;})()";

/// Intern a UTF-8 str into a fresh JSI string slot (local helper).
#[inline]
fn intern_str(rtw: *mut c_void, s: &str) -> i64 {
  unsafe {
    v8x_hermes_string_new_utf8(rtw, s.as_ptr() as *const c_char, s.len())
  }
}

/// Install a synthetic `console` on the context's global object (V8 built-in
/// parity for deno_core bootstrap). Idempotent: only sets it if absent.
fn install_global_console(context: *const Context) {
  let rtw = iso_state(context as *mut RealIsolate).rtw;
  let global_slot = unsafe { v8x_hermes_global(rtw) };
  if global_slot < 0 {
    return;
  }
  let key_slot = intern_str(rtw, "console");
  // Do not clobber a real console if one already exists.
  if unsafe { v8x_hermes_object_has(rtw, global_slot, key_slot) } == 1 {
    return;
  }
  let src_slot = intern_str(rtw, CONSOLE_LITERAL_SRC);
  let mut ok: c_int = 0;
  let console_slot = unsafe { v8x_hermes_run(rtw, src_slot, &mut ok) };
  if ok != 0 && console_slot >= 0 {
    unsafe {
      v8x_hermes_object_set(rtw, global_slot, key_slot, console_slot);
    }
  }
}

/// Apply an ObjectTemplate's stored `Set` properties and accessors onto the
/// context's global object (used by `Context::New`'s global_template).
fn apply_template_to_global(
  templ: *const ObjectTemplate,
  context: *const Context,
) {
  let t = unsafe { &*(templ as *const ObjTemplate) };
  let rtw = iso_state(context as *mut RealIsolate).rtw;
  let global_slot = unsafe { v8x_hermes_global(rtw) };
  if global_slot < 0 {
    return;
  }
  for prop in &t.properties {
    let value_slot = match template_kind(prop.value) {
      Some(TEMPLATE_KIND_FN) => slot_of(v8__FunctionTemplate__GetFunction(
        prop.value as *const FunctionTemplate,
        context,
      )),
      Some(TEMPLATE_KIND_OBJ) => slot_of(v8__ObjectTemplate__NewInstance(
        prop.value as *const ObjectTemplate,
        context,
      )),
      _ => slot_of(prop.value as *const Data),
    };
    if value_slot < 0 {
      continue;
    }
    unsafe {
      v8x_hermes_object_define_property(
        rtw,
        global_slot,
        prop.key_slot,
        value_slot,
        prop.attr,
      );
    }
  }
  for acc in &t.accessors {
    unsafe {
      v8x_hermes_object_define_accessor(
        rtw,
        global_slot,
        acc.key_slot,
        acc.getter_bits,
        acc.setter_bits,
        acc.data_slot,
        acc.attr,
      );
    }
  }
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

/// `Context::GetExtrasBindingObject`. V8 keeps one plain object per context on
/// which the embedder installs helper bindings; deno_core's bootstrap reads
/// built-ins (e.g. the console) off it. Hermes has no such object, so we create
/// a plain JS object lazily on first call, pin it into a runtime-owned durable
/// slot so it survives HandleScope pops (C2), and hand back a fresh Local
/// resolving to that same pinned object every call (C4 identity: two calls
/// return handles to one object). Never null: the vendored getter `cast_local`s
/// and would treat null as an empty MaybeLocal, but deno_core deref's the
/// result, so a live object is required.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetExtrasBindingObject(
  this: *const Context,
) -> *const Object {
  if this.is_null() {
    return ptr::null();
  }
  let st = iso_state(this as *mut RealIsolate);
  let rtw = st.rtw;
  if st.extras_binding_pin < 0 {
    let slot = unsafe { v8x_hermes_object_new(rtw) };
    if slot < 0 {
      return ptr::null();
    }
    // V8's extras binding object exposes a built-in `console`. deno_core reads
    // it (bindings.rs::initialize_deno_core_namespace) and binds it to
    // Deno.core.console, and it must be an Object or boot panics ("unable to
    // convert"). Hermes has no V8 console, so synthesize a minimal one whose
    // methods are no-ops (deno_core forwards real console output through its
    // own op-based console; these are the fallback sinks). Built by evaluating
    // an object literal so the methods are real callable functions.
    let console_src = "(function(){var c={};var m=['log','info','debug',\
      'error','warn','dir','dirxml','table','trace','group','groupCollapsed',\
      'groupEnd','clear','count','countReset','assert','profile','profileEnd',\
      'time','timeLog','timeEnd','timeStamp'];for(var i=0;i<m.length;i++){\
      c[m[i]]=function(){};}return c;})()";
    let src_bytes = console_src.as_bytes();
    let src_slot = unsafe {
      v8x_hermes_string_new_utf8(
        rtw,
        src_bytes.as_ptr() as *const c_char,
        src_bytes.len(),
      )
    };
    if src_slot >= 0 {
      let mut ok: c_int = 0;
      let console_slot = unsafe { v8x_hermes_run(rtw, src_slot, &mut ok) };
      if ok != 0 && console_slot >= 0 {
        let key_bytes = b"console";
        let key_slot = unsafe {
          v8x_hermes_string_new_utf8(
            rtw,
            key_bytes.as_ptr() as *const c_char,
            key_bytes.len(),
          )
        };
        if key_slot >= 0 {
          unsafe {
            v8x_hermes_object_set(rtw, slot, key_slot, console_slot);
          }
        }
      }
    }
    let pin = unsafe { v8x_hermes_pin(rtw, slot) };
    if pin < 0 {
      return ptr::null();
    }
    st.extras_binding_pin = pin;
  }
  let slot = unsafe { v8x_hermes_pin_get(rtw, st.extras_binding_pin) };
  slot_ptr::<Object>(slot)
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

/// `String::Empty(isolate)`: the interned empty JS string. V8 treats this as
/// infallible; deno_core's error-formatting path calls it (via `String::empty`)
/// for absent fields, so a null stub panics on the unwrap.
#[unsafe(no_mangle)]
pub extern "C" fn v8__String__Empty(
  isolate: *mut RealIsolate,
) -> *const V8String {
  intern_string_utf8(isolate, b"")
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

/// `String::create_external_onebyte_const`: a static ASCII string resource
/// baked at build time (`OneByteConst { vtable, cached_data, length }`).
/// deno_core interns most of its bootstrap strings this way (see
/// deno_core::FastString::StaticConst). Hermes has no external-string resource
/// concept, so we copy the ASCII bytes into a normal JSI string. The bytes are
/// guaranteed ASCII by the resource contract, so `as_str()` is valid UTF-8.
#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewExternalOneByteConst(
  isolate: *mut RealIsolate,
  onebyte_const: *const OneByteConst,
) -> *const V8String {
  if onebyte_const.is_null() {
    return ptr::null();
  }
  let s: &str = unsafe { (*onebyte_const).as_str() };
  intern_string_utf8(isolate, s.as_bytes())
}

/// `String::create_external_onebyte_static`: like above but the resource is a
/// raw `(buffer, length)` of ASCII/Latin-1 bytes rather than a `OneByteConst`.
#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewExternalOneByteStatic(
  isolate: *mut RealIsolate,
  buffer: *const c_char,
  length: c_int,
) -> *const V8String {
  if buffer.is_null() || length < 0 {
    return ptr::null();
  }
  let latin1 =
    unsafe { std::slice::from_raw_parts(buffer as *const u8, length as usize) };
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
pub(super) fn read_string_slot(rtw: *mut c_void, slot: i64) -> Option<String> {
  read_string(rtw, slot)
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
  let isolate = context as *mut RealIsolate;
  let rtw = iso_state(isolate).rtw;
  let use_strict = USE_STRICT.load(Ordering::Relaxed);

  // Read the source once if we might need to rewrite it (strict prefix and/or
  // the E1 async-generator lowering). Otherwise carry the slot unchanged.
  if let Some(src) = read_string(rtw, slot_of(source)) {
    // E1: lower every `async function*` / `async *method` declaration into the
    // ES2017 downlevel Hermes accepts (regular `function*` + runtime helpers,
    // native `for await`), since Hermes' compiler rejects the async-generator
    // declaration syntax. A no-op (borrows `src`) when none is present. See
    // src/hermes/lower.rs.
    let lowered = super::lower::lower_async_generators(&src);
    let needs_reintern = use_strict || matches!(lowered, std::borrow::Cow::Owned(_));
    if needs_reintern {
      let body: &str = &lowered;
      let final_src = if use_strict {
        format!("\"use strict\";\n{body}")
      } else {
        body.to_string()
      };
      let new_src = intern_string_utf8(isolate, final_src.as_bytes());
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

/// `Object::with_prototype_and_properties` (V8's `Object.create`-like builder).
/// deno_core uses it in `new_inner` to build objects with a fixed prototype and
/// an initial property set. Modeled on the existing primitives: create a fresh
/// object, set each (name, value) as an ordinary enumerable/configurable/
/// writable own property, then set its prototype. A null `prototype_or_null`
/// gives a null-prototype object (like `Object.create(null)`).
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__New__with_prototype_and_properties(
  isolate: *mut RealIsolate,
  prototype_or_null: *const Value,
  names: *const *const Name,
  values: *const *const Value,
  length: usize,
) -> *const Object {
  if isolate.is_null() {
    return ptr::null();
  }
  let rtw = iso_state(isolate).rtw;
  let obj = unsafe { v8x_hermes_object_new(rtw) };
  if obj < 0 {
    return ptr::null();
  }
  if length > 0 && !names.is_null() && !values.is_null() {
    let name_slice = unsafe { std::slice::from_raw_parts(names, length) };
    let value_slice = unsafe { std::slice::from_raw_parts(values, length) };
    for i in 0..length {
      let key = name_slice[i];
      let val = value_slice[i];
      if key.is_null() || val.is_null() {
        continue;
      }
      unsafe {
        v8x_hermes_object_set(
          rtw,
          obj,
          slot_of(key as *const Value),
          slot_of(val),
        );
      }
    }
  }
  // Set the prototype (null => null prototype). A null handle is the null slot,
  // which the C++ helper treats as "null prototype".
  unsafe {
    v8x_hermes_object_set_prototype(rtw, obj, slot_of(prototype_or_null));
  }
  slot_ptr::<Object>(obj)
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

/// `Object::GetPrototype`: `Object.getPrototypeOf(this)`. Returns a `Value`
/// (which may hold JS null) or a null handle on error. deno_core's
/// `is_instance_of_error` walks the chain via this to brand thrown exceptions.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetPrototype(
  this: *const Object,
) -> *const Value {
  if this.is_null() {
    return ptr::null();
  }
  let rtw = current_rtw();
  if rtw.is_null() {
    return ptr::null();
  }
  let slot = unsafe { v8x_hermes_object_get_prototype(rtw, slot_of(this)) };
  if slot < 0 {
    return ptr::null();
  }
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

/// `Value::IsTrue`: the value is the boolean `true` oddball (not truthiness).
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsTrue(this: *const Value) -> bool {
  let rtw = current_rtw();
  if rtw.is_null() || this.is_null() {
    return false;
  }
  unsafe { v8x_hermes_boolean_value(rtw, slot_of(this)) == 1 }
}

/// `Value::IsFalse`: the value is the boolean `false` oddball.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsFalse(this: *const Value) -> bool {
  let rtw = current_rtw();
  if rtw.is_null() || this.is_null() {
    return false;
  }
  unsafe { v8x_hermes_boolean_value(rtw, slot_of(this)) == 0 }
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

/// `Value::IsPromise` (`value instanceof Promise`). deno_core reads a source
/// module's `Evaluate` result as a Promise (the modeled `Evaluate` returns the
/// D1 resolved promise); the vendored `Local::<Promise>::try_from` calls this to
/// type-check. Was a null stub, which read as a garbage bool and made
/// `mod_evaluate_sync` report `BadType`.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsPromise(this: *const Value) -> bool {
  let rtw = current_rtw();
  if rtw.is_null() || this.is_null() {
    return false;
  }
  unsafe { v8x_hermes_value_is_promise(rtw, slot_of(this)) != 0 }
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

/// Read a numeric value as `f64`, or `None` if the handle is not a number.
/// Hermes stores all JS numbers as doubles, so this is exact.
#[inline]
fn number_value_opt(this: *const Value) -> Option<f64> {
  let rtw = current_rtw();
  if rtw.is_null() || this.is_null() {
    return None;
  }
  if unsafe { v8x_hermes_value_is_number(rtw, slot_of(this)) } == 0 {
    return None;
  }
  let mut out: f64 = 0.0;
  let ok = unsafe { v8x_hermes_number_value(rtw, slot_of(this), &mut out) };
  if ok != 0 { Some(out) } else { None }
}

/// `Value::IsInt32`: a number whose value is an integer representable as i32.
/// (V8 tags small integers specially; Hermes has no such tag, so we test the
/// value shape, matching V8's observable predicate for embedder callers.)
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsInt32(this: *const Value) -> bool {
  match number_value_opt(this) {
    Some(v) => v.fract() == 0.0 && v >= i32::MIN as f64 && v <= i32::MAX as f64,
    None => false,
  }
}

/// `Value::IsUint32`: a number whose value is an integer in `0..=u32::MAX`.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUint32(this: *const Value) -> bool {
  match number_value_opt(this) {
    Some(v) => v.fract() == 0.0 && v >= 0.0 && v <= u32::MAX as f64,
    None => false,
  }
}

/// `Value::IsNull`: routes to `jsi::Value::isNull`.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsNull(this: *const Value) -> bool {
  let rtw = current_rtw();
  if rtw.is_null() || this.is_null() {
    return false;
  }
  unsafe { v8x_hermes_value_is_null(rtw, slot_of(this)) != 0 }
}

/// `Value::NumberValue`: ECMAScript ToNumber, written into `Maybe<f64>`.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__NumberValue(
  this: *const Value,
  context: *const Context,
  out: *mut crate::support::Maybe<f64>,
) {
  #[repr(C)]
  struct MaybeF64 {
    has_value: bool,
    value: f64,
  }
  let out = out as *mut MaybeF64;
  if out.is_null() {
    return;
  }
  if this.is_null() || context.is_null() {
    unsafe {
      ptr::write(
        out,
        MaybeF64 {
          has_value: false,
          value: 0.0,
        },
      )
    };
    return;
  }
  match number_value_opt(this) {
    Some(v) => unsafe {
      ptr::write(
        out,
        MaybeF64 {
          has_value: true,
          value: v,
        },
      )
    },
    None => unsafe {
      ptr::write(
        out,
        MaybeF64 {
          has_value: false,
          value: 0.0,
        },
      )
    },
  }
}

/// `Value::Int32Value`: ECMAScript ToInt32, written into `Maybe<i32>`.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__Int32Value(
  this: *const Value,
  context: *const Context,
  out: *mut crate::support::Maybe<i32>,
) {
  #[repr(C)]
  struct MaybeI32 {
    has_value: bool,
    value: i32,
  }
  let out = out as *mut MaybeI32;
  if out.is_null() {
    return;
  }
  if this.is_null() || context.is_null() {
    unsafe {
      ptr::write(
        out,
        MaybeI32 {
          has_value: false,
          value: 0,
        },
      )
    };
    return;
  }
  match number_value_opt(this) {
    // ECMAScript ToInt32: truncate toward zero, wrap modulo 2^32.
    Some(v) => {
      let val = if v.is_finite() {
        (v.trunc() as i64 as u32) as i32
      } else {
        0
      };
      unsafe {
        ptr::write(
          out,
          MaybeI32 {
            has_value: true,
            value: val,
          },
        )
      }
    }
    None => unsafe {
      ptr::write(
        out,
        MaybeI32 {
          has_value: false,
          value: 0,
        },
      )
    },
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

// ---- BackingStore (external-memory ArrayBuffer, C2 lifetime) ----------------
//
// A v8 BackingStore is the raw memory of an ArrayBuffer, owned via
// std::unique_ptr / std::shared_ptr so V8-internal objects may alias it. deno
// uses external-memory BackingStores to share Rust memory with JS without
// copying: `store_js_callbacks` wraps `ContextState::tick_info` (Rust-owned
// bytes) as a BackingStore, then builds a JS `Uint8Array` over it so JS reads
// and writes hit the same memory.
//
// Hermes/JSI has no v8 BackingStore type, so (as with C11 templates and D2
// modules) we model it as a Rust-owned record: a `BsInner` box holding the
// external pointer, length, and the v8 deleter. The `*mut BackingStore` handle
// v8 passes around is a `*mut BsInner` in disguise (never dereferenced as a real
// BackingStore). A `SharedRef<BackingStore>`/`SharedPtrBase<BackingStore>` is an
// intrusively refcounted pointer to the same `BsInner`.
//
// C2 lifetime: the deleter, not us, owns the external bytes. We never free them
// directly. When `NewBackingStore__with_data` supplies a deleter, that deleter
// is called exactly once, when the last owner of the memory is dropped: either
// the JS ArrayBuffer built over it is GC'd (the ExternalMutableBuffer destructor
// runs) OR, if no ArrayBuffer was ever built, when the last BackingStore
// reference is dropped. `owns_bytes` tracks which path is responsible so the
// deleter fires exactly once.

struct BsInner {
  refcount: AtomicUsize,
  data: *mut c_void,
  byte_length: usize,
  is_shared: bool,
  deleter: BackingStoreDeleterCallback,
  deleter_data: *mut c_void,
  // True while this record still owns the right to run the deleter. Set false
  // once ownership of the bytes is handed to a JS ArrayBuffer (the
  // ExternalMutableBuffer becomes responsible for the deleter). `owns_alloc`
  // marks bytes we malloc'd ourselves (empty allocate path), freed on drop.
  owns_bytes: bool,
  owns_alloc: bool,
}

unsafe extern "C" fn noop_deleter(
  _data: *mut c_void,
  _len: usize,
  _deleter_data: *mut c_void,
) {
}

unsafe extern "C" {
  fn calloc(count: usize, size: usize) -> *mut c_void;
  fn free(ptr: *mut c_void);
}

impl BsInner {
  fn boxed(
    data: *mut c_void,
    byte_length: usize,
    is_shared: bool,
    deleter: BackingStoreDeleterCallback,
    deleter_data: *mut c_void,
    owns_bytes: bool,
    owns_alloc: bool,
  ) -> *mut BsInner {
    Box::into_raw(Box::new(BsInner {
      refcount: AtomicUsize::new(1),
      data,
      byte_length,
      is_shared,
      deleter,
      deleter_data,
      owns_bytes,
      owns_alloc,
    }))
  }

  fn new_allocated(byte_length: usize, is_shared: bool) -> *mut BsInner {
    let data = if byte_length == 0 {
      ptr::null_mut()
    } else {
      unsafe { calloc(byte_length, 1) }
    };
    BsInner::boxed(data, byte_length, is_shared, noop_deleter, ptr::null_mut(), true, true)
  }

  // Drop the record. Runs the deleter (or frees owned memory) only if this
  // record still holds byte ownership; if a JS ArrayBuffer took over, the
  // ExternalMutableBuffer destructor runs the deleter instead.
  unsafe fn destroy(ptr: *mut BsInner) {
    if ptr.is_null() {
      return;
    }
    let b = unsafe { Box::from_raw(ptr) };
    if b.owns_bytes && !b.data.is_null() {
      if b.owns_alloc {
        unsafe { free(b.data) };
      } else {
        unsafe { (b.deleter)(b.data, b.byte_length, b.deleter_data) };
      }
    }
  }
}

#[inline]
fn bs_inner<'a>(p: *const BackingStore) -> Option<&'a BsInner> {
  unsafe { (p as *const BsInner).as_ref() }
}

#[inline]
fn sp_get(p: *const SharedPtrBase<BackingStore>) -> *mut BsInner {
  if p.is_null() {
    return ptr::null_mut();
  }
  unsafe { *(p as *const usize) as *mut BsInner }
}

#[inline]
fn sp_set(p: *mut SharedPtrBase<BackingStore>, inner: *mut BsInner) {
  unsafe {
    let words = p as *mut usize;
    *words = inner as usize;
    *words.add(1) = 0;
  }
}

#[inline]
fn make_shared_ref(inner: *mut BsInner) -> SharedRef<BackingStore> {
  let base: SharedPtrBase<BackingStore> = Default::default();
  let mut sref = unsafe {
    std::mem::transmute_copy::<
      SharedPtrBase<BackingStore>,
      SharedRef<BackingStore>,
    >(&base)
  };
  std::mem::forget(base);
  sp_set(
    &mut sref as *mut SharedRef<BackingStore>
      as *mut SharedPtrBase<BackingStore>,
    inner,
  );
  sref
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__NewBackingStore__with_byte_length(
  isolate: *mut RealIsolate,
  byte_length: usize,
) -> *mut BackingStore {
  let _ = isolate;
  BsInner::new_allocated(byte_length, false) as *mut BackingStore
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__NewBackingStore__with_data(
  data: *mut c_void,
  byte_length: usize,
  deleter: BackingStoreDeleterCallback,
  deleter_data: *mut c_void,
) -> *mut BackingStore {
  BsInner::boxed(data, byte_length, false, deleter, deleter_data, true, false)
    as *mut BackingStore
}

// Build a JS ArrayBuffer that ALIASES the backing store's external memory, so
// Rust writes (e.g. deno's tick_info) are visible to the JS view and vice
// versa. Ownership of the bytes and the deleter transfers to the JS
// ArrayBuffer's ExternalMutableBuffer: this record stops owning the deleter
// (`owns_bytes = false`) so the bytes are freed exactly once, when the JS
// ArrayBuffer is GC'd.
#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__New__with_backing_store(
  isolate: *mut RealIsolate,
  backing_store: *const SharedRef<BackingStore>,
) -> *const ArrayBuffer {
  if isolate.is_null() || backing_store.is_null() {
    return ptr::null();
  }
  let rtw = iso_state(isolate).rtw;
  if rtw.is_null() {
    return ptr::null();
  }
  let inner = sp_get(backing_store as *const SharedPtrBase<BackingStore>);
  if inner.is_null() {
    return ptr::null();
  }
  let (data, len, deleter, deleter_data, owns_bytes, owns_alloc) = unsafe {
    (
      (*inner).data,
      (*inner).byte_length,
      (*inner).deleter,
      (*inner).deleter_data,
      (*inner).owns_bytes,
      (*inner).owns_alloc,
    )
  };

  // The JS ArrayBuffer aliases the external pointer. Ownership of the bytes
  // passes to the ArrayBuffer's ExternalMutableBuffer, which runs the v8 deleter
  // when collected. For our own malloc'd bytes (empty allocate path) or a record
  // that no longer owns the bytes, pass a no-op deleter so nothing is
  // double-freed; the JS ArrayBuffer just aliases and this record keeps
  // responsibility (or already relinquished it).
  let (js_deleter, js_deleter_data): (
    Option<unsafe extern "C" fn(*mut c_void, usize, *mut c_void)>,
    *mut c_void,
  ) = if owns_bytes && !owns_alloc {
    unsafe { (*inner).owns_bytes = false };
    (Some(deleter), deleter_data)
  } else {
    (Some(noop_deleter), ptr::null_mut())
  };

  let slot = unsafe {
    v8x_hermes_array_buffer_new_external(
      rtw,
      data,
      len,
      js_deleter,
      js_deleter_data,
    )
  };
  if slot == NULL_SLOT {
    // Creation failed: we did not hand ownership off after all.
    if owns_bytes && !owns_alloc {
      unsafe { (*inner).owns_bytes = true };
    }
    return ptr::null();
  }
  slot_ptr::<ArrayBuffer>(slot)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BackingStore__Data(
  this: *const BackingStore,
) -> *mut c_void {
  bs_inner(this).map_or(ptr::null_mut(), |b| {
    if b.byte_length == 0 {
      ptr::null_mut()
    } else {
      b.data
    }
  })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BackingStore__ByteLength(
  this: *const BackingStore,
) -> usize {
  bs_inner(this).map_or(0, |b| b.byte_length)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BackingStore__IsShared(
  this: *const BackingStore,
) -> bool {
  bs_inner(this).map_or(false, |b| b.is_shared)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BackingStore__IsResizableByUserJavaScript(
  this: *const BackingStore,
) -> bool {
  let _ = this;
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BackingStore__DELETE(this: *mut BackingStore) {
  let inner = this as *mut BsInner;
  if inner.is_null() {
    return;
  }
  if unsafe { (*inner).refcount.fetch_sub(1, Ordering::SeqCst) } == 1 {
    unsafe { BsInner::destroy(inner) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__BackingStore__COPY(
  ptr: *const SharedPtrBase<BackingStore>,
) -> SharedPtrBase<BackingStore> {
  let inner = sp_get(ptr);
  if !inner.is_null() {
    unsafe { (*inner).refcount.fetch_add(1, Ordering::SeqCst) };
  }
  let mut out: SharedPtrBase<BackingStore> = Default::default();
  sp_set(&mut out, inner);
  out
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__BackingStore__CONVERT__std__unique_ptr(
  unique_ptr: UniquePtr<BackingStore>,
) -> SharedPtrBase<BackingStore> {
  let raw = unique_ptr.into_raw() as *mut BsInner;
  let mut out: SharedPtrBase<BackingStore> = Default::default();
  sp_set(&mut out, raw);
  out
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__BackingStore__get(
  ptr: *const SharedPtrBase<BackingStore>,
) -> *mut BackingStore {
  sp_get(ptr) as *mut BackingStore
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__BackingStore__reset(
  ptr: *mut SharedPtrBase<BackingStore>,
) {
  let inner = sp_get(ptr);
  if !inner.is_null()
    && unsafe { (*inner).refcount.fetch_sub(1, Ordering::SeqCst) } == 1
  {
    unsafe { BsInner::destroy(inner) };
  }
  if !ptr.is_null() {
    sp_set(ptr, ptr::null_mut());
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__BackingStore__use_count(
  ptr: *const SharedPtrBase<BackingStore>,
) -> crate::support::long {
  let inner = sp_get(ptr);
  if inner.is_null() {
    0
  } else {
    unsafe { (*inner).refcount.load(Ordering::SeqCst) as crate::support::long }
  }
}

// Return a BackingStore that aliases an existing JS ArrayBuffer's bytes. The
// record is non-owning (no deleter): the JS ArrayBuffer, not this record, owns
// the memory, so the data pointer is only valid while that ArrayBuffer lives.
#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__GetBackingStore(
  this: *const ArrayBuffer,
) -> SharedRef<BackingStore> {
  let rtw = current_rtw();
  let (data, len) = if rtw.is_null() || this.is_null() {
    (ptr::null_mut(), 0)
  } else {
    let slot = slot_of(this);
    let data = unsafe { v8x_hermes_array_buffer_data(rtw, slot) };
    let len = unsafe { v8x_hermes_array_buffer_byte_length(rtw, slot) };
    (data, if len == usize::MAX { 0 } else { len })
  };
  let inner = BsInner::boxed(
    data,
    len,
    false,
    noop_deleter,
    ptr::null_mut(),
    false,
    false,
  );
  make_shared_ref(inner)
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

// ---- D1: Promises + microtask queue ----------------------------------------
//
// Hermes has native JS Promises but JSI exposes no Promise API and no
// [[PromiseState]] accessor, so these route through a cached JS helper on the
// C++ side (hermes_shim.cpp's promise infra). A v8 `PromiseResolver` is a
// handle to the `[promise, resolve, reject]` array that helper returns; a
// `Promise` handle is the array's element 0. State/result are recorded into a
// closure-captured WeakMap by a `.then` the helper attaches, so settlement is
// only observable AFTER a microtask drain, matching v8 semantics. See
// docs/hermes-spike/experiments/D1-hermes-promises.md.

/// `Promise::Resolver::New`: create a pending promise plus its resolve/reject.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__Resolver__New(
  context: *const Context,
) -> *const PromiseResolver {
  if context.is_null() {
    return ptr::null();
  }
  let rtw = iso_state(context as *mut RealIsolate).rtw;
  let out = unsafe { v8x_hermes_promise_resolver_new(rtw) };
  slot_ptr::<PromiseResolver>(out)
}

/// `Promise::Resolver::GetPromise`: the resolver's associated promise.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__Resolver__GetPromise(
  this: *const PromiseResolver,
) -> *const Promise {
  let rtw = current_rtw();
  if rtw.is_null() || this.is_null() {
    return ptr::null();
  }
  let out = unsafe { v8x_hermes_promise_resolver_get_promise(rtw, slot_of(this)) };
  slot_ptr::<Promise>(out)
}

/// `Promise::Resolver::Resolve`: settle the promise fulfilled with `value`.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__Resolver__Resolve(
  this: *const PromiseResolver,
  context: *const Context,
  value: *const Value,
) -> MaybeBool {
  if this.is_null() || context.is_null() {
    return MaybeBool::Nothing;
  }
  let rtw = iso_state(context as *mut RealIsolate).rtw;
  let ok = unsafe {
    v8x_hermes_promise_resolver_resolve(rtw, slot_of(this), slot_of(value))
  };
  if ok != 0 {
    MaybeBool::JustTrue
  } else {
    MaybeBool::Nothing
  }
}

/// `Promise::Resolver::Reject`: settle the promise rejected with `value`.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__Resolver__Reject(
  this: *const PromiseResolver,
  context: *const Context,
  value: *const Value,
) -> MaybeBool {
  if this.is_null() || context.is_null() {
    return MaybeBool::Nothing;
  }
  let rtw = iso_state(context as *mut RealIsolate).rtw;
  let ok = unsafe {
    v8x_hermes_promise_resolver_reject(rtw, slot_of(this), slot_of(value))
  };
  if ok != 0 {
    MaybeBool::JustTrue
  } else {
    MaybeBool::Nothing
  }
}

/// `Promise::State`: pending / fulfilled / rejected (observable after a drain).
#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__State(this: *const Promise) -> PromiseState {
  let rtw = current_rtw();
  if rtw.is_null() || this.is_null() {
    return PromiseState::Pending;
  }
  match unsafe { v8x_hermes_promise_state(rtw, slot_of(this)) } {
    1 => PromiseState::Fulfilled,
    2 => PromiseState::Rejected,
    _ => PromiseState::Pending,
  }
}

/// `Promise::Result`: the settled `[[PromiseResult]]` value.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__Result(this: *const Promise) -> *const Value {
  let rtw = current_rtw();
  if rtw.is_null() || this.is_null() {
    return ptr::null();
  }
  let out = unsafe { v8x_hermes_promise_result(rtw, slot_of(this)) };
  slot_ptr::<Value>(out)
}

/// `Promise::HasHandler`: whether a reaction is attached.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__HasHandler(this: *const Promise) -> bool {
  let rtw = current_rtw();
  if rtw.is_null() || this.is_null() {
    return false;
  }
  unsafe { v8x_hermes_promise_has_handler(rtw, slot_of(this)) != 0 }
}

/// `Promise::MarkAsHandled`: suppress unhandled-rejection reporting by marking
/// the promise handled (the same flag `HasHandler` reads).
#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__MarkAsHandled(this: *const Promise) {
  let rtw = current_rtw();
  if rtw.is_null() || this.is_null() {
    return;
  }
  unsafe { v8x_hermes_promise_mark_handled(rtw, slot_of(this)) };
}

/// `Promise::Then`: `promise.then(handler)`, returning the derived promise.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__Then(
  this: *const Promise,
  context: *const Context,
  handler: *const Function,
) -> *const Promise {
  if this.is_null() || context.is_null() {
    return ptr::null();
  }
  let rtw = iso_state(context as *mut RealIsolate).rtw;
  let out = unsafe {
    v8x_hermes_promise_then(rtw, slot_of(this), slot_of(handler))
  };
  slot_ptr::<Promise>(out)
}

/// `Promise::Catch`: `promise.catch(handler)`, returning the derived promise.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__Catch(
  this: *const Promise,
  context: *const Context,
  handler: *const Function,
) -> *const Promise {
  if this.is_null() || context.is_null() {
    return ptr::null();
  }
  let rtw = iso_state(context as *mut RealIsolate).rtw;
  let out = unsafe {
    v8x_hermes_promise_catch(rtw, slot_of(this), slot_of(handler))
  };
  slot_ptr::<Promise>(out)
}

/// `Promise::Then2`: `promise.then(onFulfilled, onRejected)`.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Promise__Then2(
  this: *const Promise,
  context: *const Context,
  on_fulfilled: *const Function,
  on_rejected: *const Function,
) -> *const Promise {
  if this.is_null() || context.is_null() {
    return ptr::null();
  }
  let rtw = iso_state(context as *mut RealIsolate).rtw;
  let out = unsafe {
    v8x_hermes_promise_then2(
      rtw,
      slot_of(this),
      slot_of(on_fulfilled),
      slot_of(on_rejected),
    )
  };
  slot_ptr::<Promise>(out)
}

/// `Isolate::EnqueueMicrotask`: schedule `function` to run at the next
/// checkpoint.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__EnqueueMicrotask(
  isolate: *mut RealIsolate,
  function: *const Function,
) {
  if isolate.is_null() || function.is_null() {
    return;
  }
  let rtw = iso_state(isolate).rtw;
  unsafe {
    v8x_hermes_enqueue_microtask(rtw, slot_of(function));
  }
}

// ---- MicrotaskQueue object API ---------------------------------------------
//
// deno_core can drive microtasks through an explicit `MicrotaskQueue` object
// (Context::New(queue) / Context::get_microtask_queue) as well as through the
// isolate. Hermes has one shared job queue per runtime (the setImmediate FIFO
// the promise infra installs), so a `MicrotaskQueue` handle is just a small
// heap marker: enqueue/checkpoint on it route to that same shared queue. This
// gives a real, non-null, round-trippable pointer (the identity check in
// `microtask_queue_new`) and working enqueue+drain, without a second queue.

/// Backing object for a `MicrotaskQueue` handle. Boxed; its address is the
/// `*mut MicrotaskQueue` the vendored surface passes around.
struct MtqState {
  /// Whether a drain is currently running (IsRunningMicrotasks).
  running: Cell<bool>,
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__MicrotaskQueue__New(
  _isolate: *mut RealIsolate,
  _policy: crate::MicrotasksPolicy,
) -> *mut MicrotaskQueue {
  let boxed = Box::new(MtqState {
    running: Cell::new(false),
  });
  Box::into_raw(boxed) as *mut MicrotaskQueue
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__MicrotaskQueue__DESTRUCT(queue: *mut MicrotaskQueue) {
  if queue.is_null() {
    return;
  }
  // SAFETY: queue was produced by `New` (a Box<MtqState>::into_raw).
  drop(unsafe { Box::from_raw(queue as *mut MtqState) });
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__MicrotaskQueue__EnqueueMicrotask(
  isolate: *mut RealIsolate,
  _queue: *const MicrotaskQueue,
  microtask: *const Function,
) {
  if isolate.is_null() || microtask.is_null() {
    return;
  }
  let rtw = iso_state(isolate).rtw;
  unsafe {
    v8x_hermes_enqueue_microtask(rtw, slot_of(microtask));
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__MicrotaskQueue__PerformCheckpoint(
  isolate: *mut RealIsolate,
  queue: *const MicrotaskQueue,
) {
  if isolate.is_null() {
    return;
  }
  let mtq = if queue.is_null() {
    None
  } else {
    // SAFETY: queue was produced by `New`.
    Some(unsafe { &*(queue as *const MtqState) })
  };
  if let Some(m) = mtq {
    m.running.set(true);
  }
  let rtw = iso_state(isolate).rtw;
  unsafe {
    v8x_hermes_drain_microtasks(rtw);
  }
  if let Some(m) = mtq {
    m.running.set(false);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__MicrotaskQueue__IsRunningMicrotasks(
  queue: *const MicrotaskQueue,
) -> bool {
  if queue.is_null() {
    return false;
  }
  // SAFETY: queue was produced by `New`.
  unsafe { &*(queue as *const MtqState) }.running.get()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__MicrotaskQueue__GetMicrotasksScopeDepth(
  _queue: *const MicrotaskQueue,
) -> c_int {
  0
}

// ---- C10: native function callbacks ----------------------------------------
//
// A v8 FunctionCallback is a C fn ptr the vendored surface invokes with a
// `*const FunctionCallbackInfo`, reading its arguments/this/data/return-value
// through the `v8__FunctionCallbackInfo__*` / `v8__ReturnValue__*` accessors.
// Hermes drives host functions through a C++ `std::function`; the C++ bridge
// (hermes_shim.cpp) marshals each JSI call into handle-table slots and calls
// `v8x_hermes_dispatch_callback` below, which builds a `CbInfo`, invokes the
// FunctionCallback, and hands back the slot the callback stored via
// ReturnValue.
//
// The FunctionCallbackInfo the accessors read is a Rust-owned `CbInfo` (the
// same model the QuickJS backend uses). A v8 Local here is a tagged
// handle-table index (`slot_ptr`), so the ReturnValue "slot" the setters write
// into is a `usize` holding a tagged Local pointer (or the tagged undefined
// pointer initially).

/// The layout the vendored `ReturnValue` reads: a single `usize` that is a raw
/// pointer to the return-value storage. Matches `function.rs::RawReturnValue`.
#[repr(C)]
struct RawReturnValue(usize);

/// The layout the vendored `FunctionCallbackInfo::get_parts` reads. Matches
/// `function.rs::RawFunctionCallbackInfoParts`.
#[repr(C)]
struct RawFunctionCallbackInfoParts {
  isolate: *mut RealIsolate,
  return_value: usize,
  data: *const Value,
  length: crate::support::int,
}

/// The Rust-owned backing object a `*const FunctionCallbackInfo` points at
/// during a native callback. Each field is a tagged Local pointer (as `usize`)
/// into the isolate's handle table, except `return_slot` which is a boxed
/// storage the ReturnValue setters mutate.
struct CbInfo {
  isolate: *mut RealIsolate,
  this: usize,
  data: usize,
  new_target: usize,
  is_construct: bool,
  args: Vec<usize>,
  /// Boxed so its address is stable while the callback holds a ReturnValue.
  /// Holds a tagged Local pointer (initialised to tagged-undefined).
  return_slot: Box<usize>,
}

#[inline]
fn cbinfo<'a>(this: *const FunctionCallbackInfo) -> &'a mut CbInfo {
  unsafe { &mut *(this as *mut CbInfo) }
}

/// The C++ host-function trampoline calls this when a native-backed JS function
/// is invoked. It constructs the `FunctionCallbackInfo`, runs the v8
/// FunctionCallback, and returns the handle-table slot of the callback's
/// return value (or `NULL_SLOT` for undefined). `*threw` is set to 1 if the
/// callback left a pending exception the host function must re-throw.
#[unsafe(no_mangle)]
pub extern "C" fn v8x_hermes_dispatch_callback(
  _rtw: *mut c_void,
  callback_bits: usize,
  this_slot: i64,
  data_slot: i64,
  arg_slots: *const i64,
  argc: usize,
  is_construct: c_int,
  new_target_slot: i64,
  threw: *mut c_int,
) -> i64 {
  if !threw.is_null() {
    unsafe { *threw = 0 };
  }
  if callback_bits == 0 {
    return NULL_SLOT;
  }
  let iso = current_iso();

  // A tagged Local pointer for "undefined" to seed the return slot. Reuse the
  // isolate's undefined singleton so `rv.get()` before any set reads undefined.
  let undef_slot = if iso.is_null() {
    NULL_SLOT
  } else {
    unsafe { v8x_hermes_undefined(iso_state(iso).rtw) }
  };

  let mut args: Vec<usize> = Vec::with_capacity(argc);
  if !arg_slots.is_null() {
    for i in 0..argc {
      let s = unsafe { *arg_slots.add(i) };
      args.push(slot_ptr::<Value>(s) as usize);
    }
  }

  let mut info = Box::new(CbInfo {
    isolate: iso,
    this: slot_ptr::<Value>(this_slot) as usize,
    data: slot_ptr::<Value>(data_slot) as usize,
    new_target: slot_ptr::<Value>(new_target_slot) as usize,
    is_construct: is_construct != 0,
    args,
    return_slot: Box::new(slot_ptr::<Value>(undef_slot) as usize),
  });

  let callback: FunctionCallback =
    unsafe { std::mem::transmute::<usize, FunctionCallback>(callback_bits) };
  let info_ptr = &mut *info as *mut CbInfo as *const FunctionCallbackInfo;
  unsafe { (callback)(info_ptr) };

  // Read the return slot back as a handle-table index.
  let ret_tagged = *info.return_slot as *const Value;
  let ret_slot = slot_of(ret_tagged);

  // If the callback threw (Isolate::ThrowException stored a pending exception
  // on the isolate, see below), surface it to the host function.
  if !iso.is_null() {
    let st = iso_state(iso);
    if st.pending_exception >= 0 {
      let exc = st.pending_exception;
      st.pending_exception = NULL_SLOT;
      unsafe {
        v8x_hermes_set_pending_callback_exception(st.rtw, exc);
      }
      if !threw.is_null() {
        unsafe { *threw = 1 };
      }
    }
  }

  ret_slot
}

/// `Function::New` (via `FunctionBuilder::build`): create a JS function backed
/// by a native FunctionCallback. `data_or_null` is the callback's `data`
/// (optional). ConstructorBehavior/SideEffectType are accepted but not modeled
/// (Hermes host functions are always callable; constructor behaviour is not
/// distinguished at the JSI layer). Returns the null handle on error.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__New(
  context: *const Context,
  callback: FunctionCallback,
  data_or_null: *const Value,
  length: i32,
  _constructor_behavior: c_int,
  _side_effect_type: c_int,
) -> *const Function {
  if context.is_null() {
    return ptr::null();
  }
  let rtw = iso_state(context as *mut RealIsolate).rtw;
  let data_slot = slot_of(data_or_null);
  let callback_bits = callback as usize;
  // -1: a plain Function::new is a non-constructable callable (not a
  // template), so template_id=0 (never stamped) and signature_templ_id=-1
  // (no signature, any receiver is accepted).
  let out = unsafe {
    v8x_hermes_function_new(
      rtw,
      callback_bits,
      data_slot,
      length,
      ptr::null(),
      -1,
      0,
      -1,
    )
  };
  if out < 0 {
    return ptr::null();
  }
  slot_ptr::<Function>(out)
}

/// `Function::SetName`. JSI has no name setter, so the name is (re)defined as
/// the function's `name` own property via `Object.defineProperty` (writable
/// false, configurable true, matching V8's `Function.name`). Reuses the same
/// C++ helper the FunctionTemplate class-name path uses. deno_core calls this
/// on every op function during bootstrap.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__SetName(
  this: *const Function,
  name: *const V8String,
) {
  if this.is_null() || name.is_null() {
    return;
  }
  let rtw = current_rtw();
  if rtw.is_null() {
    return;
  }
  unsafe {
    v8x_hermes_function_set_name(rtw, slot_of(this), slot_of(name));
  }
}

/// `Function::GetName`. Reads the function's `name` own property (a JS string).
/// Returns null if there is no current runtime; the vendored surface treats a
/// null as an empty Local, which its callers `.unwrap()`, so a live function
/// always has a `name` (the engine defaults it to "").
#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__GetName(
  this: *const Function,
) -> *const V8String {
  if this.is_null() {
    return ptr::null();
  }
  let rtw = current_rtw();
  if rtw.is_null() {
    return ptr::null();
  }
  let key = intern_string_utf8(current_iso(), b"name");
  if key.is_null() {
    return ptr::null();
  }
  let slot =
    unsafe { v8x_hermes_object_get(rtw, slot_of(this), slot_of(key)) };
  slot_ptr::<V8String>(slot)
}

// ---- FunctionCallbackInfo accessors ----------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__GetIsolate(
  this: *const FunctionCallbackInfo,
) -> *mut RealIsolate {
  if this.is_null() {
    return current_iso();
  }
  cbinfo(this).isolate
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__GetParts(
  this: *const FunctionCallbackInfo,
) -> RawFunctionCallbackInfoParts {
  if this.is_null() {
    return RawFunctionCallbackInfoParts {
      isolate: current_iso(),
      return_value: 0,
      data: ptr::null(),
      length: 0,
    };
  }
  let info = cbinfo(this);
  RawFunctionCallbackInfoParts {
    isolate: info.isolate,
    return_value: (&mut *info.return_slot as *mut usize) as usize,
    data: info.data as *const Value,
    length: info.args.len() as crate::support::int,
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__Data(
  this: *const FunctionCallbackInfo,
) -> *const Value {
  if this.is_null() {
    return ptr::null();
  }
  cbinfo(this).data as *const Value
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__This(
  this: *const FunctionCallbackInfo,
) -> *const Object {
  if this.is_null() {
    return ptr::null();
  }
  cbinfo(this).this as *const Object
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__NewTarget(
  this: *const FunctionCallbackInfo,
) -> *const Value {
  if this.is_null() {
    return ptr::null();
  }
  let info = cbinfo(this);
  if info.is_construct {
    info.new_target as *const Value
  } else {
    // Not a construct call: undefined.
    let iso = info.isolate;
    if iso.is_null() {
      return ptr::null();
    }
    let s = unsafe { v8x_hermes_undefined(iso_state(iso).rtw) };
    slot_ptr::<Value>(s)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__IsConstructCall(
  this: *const FunctionCallbackInfo,
) -> bool {
  if this.is_null() {
    return false;
  }
  cbinfo(this).is_construct
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__Get(
  this: *const FunctionCallbackInfo,
  index: crate::support::int,
) -> *const Value {
  if this.is_null() {
    return ptr::null();
  }
  let info = cbinfo(this);
  if index < 0 {
    return undefined_tagged(info.isolate);
  }
  match info.args.get(index as usize) {
    Some(&v) => v as *const Value,
    None => undefined_tagged(info.isolate),
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__Length(
  this: *const FunctionCallbackInfo,
) -> crate::support::int {
  if this.is_null() {
    return 0;
  }
  cbinfo(this).args.len() as crate::support::int
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__GetReturnValue(
  this: *const FunctionCallbackInfo,
) -> usize {
  if this.is_null() {
    return 0;
  }
  let info = cbinfo(this);
  (&mut *info.return_slot as *mut usize) as usize
}

/// A tagged Local pointer for undefined on `iso`, or null if none.
#[inline]
fn undefined_tagged(iso: *mut RealIsolate) -> *const Value {
  if iso.is_null() {
    return ptr::null();
  }
  let s = unsafe { v8x_hermes_undefined(iso_state(iso).rtw) };
  slot_ptr::<Value>(s)
}

// ---- ReturnValue setters ---------------------------------------------------
//
// `RawReturnValue.0` is a raw pointer to the `usize` return slot in `CbInfo`.
// Each setter writes a tagged Local pointer into that slot. Primitive setters
// intern a fresh handle for the value.

#[inline]
unsafe fn rv_slot(this: *mut RawReturnValue) -> *mut usize {
  if this.is_null() {
    return ptr::null_mut();
  }
  unsafe { (*this).0 as *mut usize }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Set(
  this: *mut RawReturnValue,
  value: *const Value,
) {
  let slot = unsafe { rv_slot(this) };
  if !slot.is_null() {
    unsafe { *slot = value as usize };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Set__Bool(
  this: *mut RawReturnValue,
  value: bool,
) {
  let slot = unsafe { rv_slot(this) };
  if slot.is_null() {
    return;
  }
  let iso = current_iso();
  if iso.is_null() {
    return;
  }
  let s = unsafe { v8x_hermes_boolean_new(iso_state(iso).rtw, value as c_int) };
  unsafe { *slot = slot_ptr::<Value>(s) as usize };
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Set__Int32(
  this: *mut RawReturnValue,
  value: i32,
) {
  set_number(this, value as f64);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Set__Uint32(
  this: *mut RawReturnValue,
  value: u32,
) {
  set_number(this, value as f64);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Set__Double(
  this: *mut RawReturnValue,
  value: f64,
) {
  set_number(this, value);
}

#[inline]
fn set_number(this: *mut RawReturnValue, value: f64) {
  let slot = unsafe { rv_slot(this) };
  if slot.is_null() {
    return;
  }
  let iso = current_iso();
  if iso.is_null() {
    return;
  }
  let s = unsafe { v8x_hermes_number_new(iso_state(iso).rtw, value) };
  unsafe { *slot = slot_ptr::<Value>(s) as usize };
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__SetNull(this: *mut RawReturnValue) {
  let slot = unsafe { rv_slot(this) };
  if slot.is_null() {
    return;
  }
  let iso = current_iso();
  if iso.is_null() {
    return;
  }
  let s = unsafe { v8x_hermes_null(iso_state(iso).rtw) };
  unsafe { *slot = slot_ptr::<Value>(s) as usize };
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__SetUndefined(
  this: *mut RawReturnValue,
) {
  let slot = unsafe { rv_slot(this) };
  if slot.is_null() {
    return;
  }
  let iso = current_iso();
  if iso.is_null() {
    return;
  }
  let s = unsafe { v8x_hermes_undefined(iso_state(iso).rtw) };
  unsafe { *slot = slot_ptr::<Value>(s) as usize };
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__SetEmptyString(
  this: *mut RawReturnValue,
) {
  let slot = unsafe { rv_slot(this) };
  if slot.is_null() {
    return;
  }
  let iso = current_iso();
  if iso.is_null() {
    return;
  }
  let s = intern_string_utf8(iso, b"");
  unsafe { *slot = s as usize };
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Get(
  this: *const RawReturnValue,
) -> *const Value {
  if this.is_null() {
    return ptr::null();
  }
  let slot = unsafe { (*this).0 as *const usize };
  if slot.is_null() {
    return ptr::null();
  }
  unsafe { *slot as *const Value }
}

// ---- Templates: FunctionTemplate + ObjectTemplate (C10 + C11) --------------
//
// Hermes has no template concept, so a v8 Template is modeled as a Rust-owned
// record leaked as a stable pointer (a template is not a JS value, so the
// tagged-pointer/handle-table scheme does not apply). Both concrete template
// kinds begin with a shared `#[repr(C)] TemplateHeader { kind }` so
// `v8__Template__Set` (whose `this` is the abstract base `Template`) can
// dispatch on the concrete kind.
//
// Template pointers are raw `Box::into_raw` allocations, so they are always
// even (Box aligns >= 2). A tagged Local pointer, by contrast, always has its
// low bit set (`slot_ptr`'s `(i<<1)|1`). `data_is_template_ptr` uses this to
// tell an untyped `*const Data` value apart: a template argument to
// `Template::Set` vs a handle-table Local. See
// docs/hermes-spike/experiments/C11-hermes-templates.md.

const TEMPLATE_KIND_FN: u8 = 0;
const TEMPLATE_KIND_OBJ: u8 = 1;

/// A raw (untagged) template pointer, distinct from a tagged Local (low bit
/// set). Non-null + even => a `Box::into_raw` template; low bit set => a
/// handle-table Local.
#[inline]
fn data_is_template_ptr(p: *const c_void) -> bool {
  !p.is_null() && (p as usize) & 1 == 0
}

/// Read the shared header kind of a template pointer, or `None` if `p` is not
/// a template pointer.
#[inline]
fn template_kind(p: *const c_void) -> Option<u8> {
  if !data_is_template_ptr(p) {
    return None;
  }
  Some(unsafe { (*(p as *const TemplateHeader)).kind })
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TemplateHeader {
  kind: u8,
}

/// One `Template::Set` property: a key slot, a stored value (either a
/// handle-table Local, or a nested template pointer), and its attributes.
struct TemplateProp {
  key_slot: i64,
  /// The raw `*const Data` value pointer as passed to `Template::Set`. Either
  /// a tagged Local or a template pointer (distinguished by
  /// `data_is_template_ptr`). Re-materialized at instantiation time.
  value: *const c_void,
  attr: c_int,
}

/// One native-data-property accessor registered on an ObjectTemplate.
struct TemplAccessor {
  key_slot: i64,
  getter_bits: usize,
  setter_bits: usize,
  data_slot: i64,
  attr: c_int,
}

/// One FunctionTemplate-pair accessor property (`SetAccessorProperty`): the
/// getter/setter are FunctionTemplates, instantiated to real functions at
/// NewInstance time and installed as a JS `{get, set}` accessor.
struct TemplAccessorProp {
  key_slot: i64,
  getter_templ: *const FnTemplate,
  setter_templ: *const FnTemplate,
  attr: c_int,
}

#[repr(C)]
struct FnTemplate {
  header: TemplateHeader,
  callback: FunctionCallback,
  data_slot: i64,
  length: i32,
  isolate: *mut RealIsolate,
  /// Lazily-created instance/prototype ObjectTemplates (v8 semantics: created
  /// on first `instance_template()`/`prototype_template()` and reused).
  instance_template: *mut ObjTemplate,
  prototype_template: *mut ObjTemplate,
  /// The class-name String slot set by `SetClassName`, or -1.
  class_name_slot: i64,
  /// A stable per-template id (C12 `Signature`): every instance constructed
  /// through this template's constructor function is stamped with this id (a
  /// hidden Symbol-keyed property), so a `Signature`'s receiver check can
  /// walk the prototype chain looking for it. Never 0 (a real id is always
  /// >= 1; 0 doubles as "no id assigned").
  template_id: i64,
  /// `FunctionTemplate::builder().signature(...)`: the `template_id` of the
  /// FunctionTemplate the `Signature` was built from (`Signature::New`'s
  /// `templ` argument), or -1 for no signature (the default, any receiver is
  /// accepted).
  signature_templ_id: i64,
}

/// Process-global monotonic counter backing `FnTemplate::template_id` (C12
/// `Signature`). Templates are Rust-owned leaked pointers, not tied to any one
/// runtime, so a single global counter (rather than a per-runtime one like the
/// C4 identity hash) keeps every template's id distinct. Starts at 1 so 0 can
/// mean "unset" / "no signature".
static NEXT_TEMPLATE_ID: AtomicUsize = AtomicUsize::new(1);

#[repr(C)]
struct ObjTemplate {
  header: TemplateHeader,
  isolate: *mut RealIsolate,
  properties: Vec<TemplateProp>,
  internal_field_count: i32,
  accessors: Vec<TemplAccessor>,
  accessor_props: Vec<TemplAccessorProp>,
  immutable_proto: bool,
  /// If this ObjectTemplate was created from a FunctionTemplate
  /// (`ObjectTemplate::new_from_template`), the source template pointer, so
  /// `new_instance`'s constructor-name reflects the FunctionTemplate's class.
  from_function_template: *const FnTemplate,
}

impl ObjTemplate {
  fn new(isolate: *mut RealIsolate) -> *mut ObjTemplate {
    Box::into_raw(Box::new(ObjTemplate {
      header: TemplateHeader {
        kind: TEMPLATE_KIND_OBJ,
      },
      isolate,
      properties: Vec::new(),
      internal_field_count: 0,
      accessors: Vec::new(),
      accessor_props: Vec::new(),
      immutable_proto: false,
      from_function_template: ptr::null(),
    }))
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__New(
  isolate: *mut RealIsolate,
  callback: FunctionCallback,
  data_or_null: *const Value,
  signature_or_null: *const c_void,
  length: i32,
  _constructor_behavior: c_int,
  _side_effect_type: c_int,
  _c_functions: *const c_void,
  _c_functions_len: usize,
) -> *const FunctionTemplate {
  if isolate.is_null() {
    return ptr::null();
  }
  // A `Signature` is just the source FnTemplate pointer (see
  // v8__Signature__New below); read its template_id, or -1 for "no
  // signature" (any receiver is accepted, the v8 default).
  let signature_templ_id = if signature_or_null.is_null() {
    -1
  } else {
    unsafe { (*(signature_or_null as *const FnTemplate)).template_id }
  };
  let templ = Box::new(FnTemplate {
    header: TemplateHeader {
      kind: TEMPLATE_KIND_FN,
    },
    callback,
    data_slot: slot_of(data_or_null),
    length,
    isolate,
    instance_template: ptr::null_mut(),
    prototype_template: ptr::null_mut(),
    class_name_slot: NULL_SLOT,
    template_id: NEXT_TEMPLATE_ID.fetch_add(1, Ordering::Relaxed) as i64,
    signature_templ_id,
  });
  Box::into_raw(templ) as *const FunctionTemplate
}

/// `Signature::New`: a Signature is identified by the source FunctionTemplate
/// it was built from. Rather than a separate allocation, this simply returns
/// the FnTemplate pointer itself reinterpreted as `*const Signature` (both are
/// opaque, non-handle-table pointers in this backend, and `FunctionTemplate::
/// New`'s `signature_or_null` reads the `template_id` straight back off it, so
/// no unwrapping is ever needed at any other call site).
#[unsafe(no_mangle)]
pub extern "C" fn v8__Signature__New(
  _isolate: *mut RealIsolate,
  templ: *const FunctionTemplate,
) -> *const c_void {
  templ as *const c_void
}

/// The instance-template internal-field count for this FunctionTemplate (0 if
/// no instance_template was created or none was requested).
#[inline]
fn fn_instance_ifc(templ: &FnTemplate) -> i64 {
  if templ.instance_template.is_null() {
    0
  } else {
    unsafe { (*templ.instance_template).internal_field_count as i64 }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__GetFunction(
  this: *const FunctionTemplate,
  context: *const Context,
) -> *const Function {
  if this.is_null() || context.is_null() {
    return ptr::null();
  }
  let templ = unsafe { &*(this as *const FnTemplate) };
  let rtw = iso_state(context as *mut RealIsolate).rtw;
  let out = unsafe {
    v8x_hermes_function_new(
      rtw,
      templ.callback as usize,
      templ.data_slot,
      templ.length,
      ptr::null(),
      fn_instance_ifc(templ),
      templ.template_id,
      templ.signature_templ_id,
    )
  };
  if out < 0 {
    return ptr::null();
  }
  // Apply the class name (SetClassName): set the function's `.name`, so
  // `g.constructor.name` reflects it.
  if templ.class_name_slot >= 0 {
    unsafe {
      v8x_hermes_function_set_name(rtw, out, templ.class_name_slot);
    }
  }
  slot_ptr::<Function>(out)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__SetClassName(
  this: *const FunctionTemplate,
  name: *const V8String,
) {
  if this.is_null() {
    return;
  }
  let templ = unsafe { &mut *(this as *mut FnTemplate) };
  templ.class_name_slot = slot_of(name);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__InstanceTemplate(
  this: *const FunctionTemplate,
) -> *const ObjectTemplate {
  if this.is_null() {
    return ptr::null();
  }
  let templ = unsafe { &mut *(this as *mut FnTemplate) };
  if templ.instance_template.is_null() {
    templ.instance_template = ObjTemplate::new(templ.isolate);
  }
  templ.instance_template as *const ObjectTemplate
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__PrototypeTemplate(
  this: *const FunctionTemplate,
) -> *const ObjectTemplate {
  if this.is_null() {
    return ptr::null();
  }
  let templ = unsafe { &mut *(this as *mut FnTemplate) };
  if templ.prototype_template.is_null() {
    templ.prototype_template = ObjTemplate::new(templ.isolate);
  }
  templ.prototype_template as *const ObjectTemplate
}

// ---- Template::Set (base of FunctionTemplate + ObjectTemplate) -------------

/// `Template::Set` / `set_with_attr`. `this` is the abstract `Template` base;
/// dispatch on the shared header kind. The stored `value` may itself be a
/// nested template (a FunctionTemplate/ObjectTemplate Local) or a plain Data
/// handle; it is kept as the raw pointer and re-materialized at instantiation.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Template__Set(
  this: *const Template,
  key: *const Name,
  value: *const Data,
  attr: c_int,
) {
  if this.is_null() {
    return;
  }
  let prop = TemplateProp {
    key_slot: slot_of(key),
    value: value as *const c_void,
    attr,
  };
  match template_kind(this as *const c_void) {
    Some(TEMPLATE_KIND_OBJ) => {
      let t = unsafe { &mut *(this as *mut ObjTemplate) };
      t.properties.push(prop);
    }
    Some(TEMPLATE_KIND_FN) => {
      // A FunctionTemplate::Set adds a static property to the constructor. Not
      // exercised by the C11 target cluster; store it on a lazily-created
      // prototype-less holder is out of scope, so record on the instance
      // template as the closest reachable behavior. (No target test asserts
      // FunctionTemplate::Set, so this never runs in the cluster.)
      let t = unsafe { &mut *(this as *mut FnTemplate) };
      if t.instance_template.is_null() {
        t.instance_template = ObjTemplate::new(t.isolate);
      }
      unsafe { (*t.instance_template).properties.push(prop) };
    }
    _ => {}
  }
}

// ---- ObjectTemplate --------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__New(
  isolate: *mut RealIsolate,
  templ: *const FunctionTemplate,
) -> *const ObjectTemplate {
  if isolate.is_null() {
    return ptr::null();
  }
  let obj = ObjTemplate::new(isolate);
  if !templ.is_null() {
    unsafe {
      (*obj).from_function_template = templ as *const FnTemplate;
    }
  }
  obj as *const ObjectTemplate
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__SetInternalFieldCount(
  this: *const ObjectTemplate,
  value: crate::support::int,
) {
  if this.is_null() {
    return;
  }
  let t = unsafe { &mut *(this as *mut ObjTemplate) };
  t.internal_field_count = value as i32;
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__InternalFieldCount(
  this: *const ObjectTemplate,
) -> crate::support::int {
  if this.is_null() {
    return 0;
  }
  let t = unsafe { &*(this as *const ObjTemplate) };
  t.internal_field_count as crate::support::int
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__SetImmutableProto(
  this: *const ObjectTemplate,
) {
  if this.is_null() {
    return;
  }
  let t = unsafe { &mut *(this as *mut ObjTemplate) };
  t.immutable_proto = true;
}

/// The accessor-callback pointer types. Linking is name-only, so these are
/// declared as raw pointers (an `unsafe extern "C" fn(...)` is ABI-identical
/// to a data pointer here); `getter`/`setter` bits are transmuted to the real
/// fn pointer inside the dispatch trampolines.
#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__SetNativeDataProperty(
  this: *const ObjectTemplate,
  key: *const Name,
  getter: *const c_void,
  setter: *const c_void,
  data_or_null: *const Value,
  attr: c_int,
) {
  if this.is_null() {
    return;
  }
  let t = unsafe { &mut *(this as *mut ObjTemplate) };
  t.accessors.push(TemplAccessor {
    key_slot: slot_of(key),
    getter_bits: getter as usize,
    setter_bits: setter as usize,
    data_slot: slot_of(data_or_null),
    attr,
  });
}

/// `ObjectTemplate::SetAccessorProperty`: an accessor whose getter/setter are
/// FunctionTemplates (instantiated at NewInstance). Either may be null.
#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__SetAccessorProperty(
  this: *const ObjectTemplate,
  key: *const Name,
  getter: *const FunctionTemplate,
  setter: *const FunctionTemplate,
  attr: c_int,
) {
  if this.is_null() {
    return;
  }
  let t = unsafe { &mut *(this as *mut ObjTemplate) };
  t.accessor_props.push(TemplAccessorProp {
    key_slot: slot_of(key),
    getter_templ: getter as *const FnTemplate,
    setter_templ: setter as *const FnTemplate,
    attr,
  });
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__NewInstance(
  this: *const ObjectTemplate,
  context: *const Context,
) -> *const Object {
  if this.is_null() || context.is_null() {
    return ptr::null();
  }
  let t = unsafe { &*(this as *const ObjTemplate) };
  let rtw = iso_state(context as *mut RealIsolate).rtw;

  // Materialize a real JS object with the declared internal-field slots.
  let obj_slot =
    unsafe { v8x_hermes_object_new_with_internal_fields(rtw, t.internal_field_count as i64) };
  if obj_slot < 0 {
    return ptr::null();
  }

  // Apply each stored Template::Set property. A property whose value is itself
  // a FunctionTemplate is instantiated to a real function at this point
  // (mirroring v8's template instantiation).
  for prop in &t.properties {
    let value_slot = match template_kind(prop.value) {
      Some(TEMPLATE_KIND_FN) => {
        let f = v8__FunctionTemplate__GetFunction(
          prop.value as *const FunctionTemplate,
          context,
        );
        slot_of(f)
      }
      Some(TEMPLATE_KIND_OBJ) => {
        let o = v8__ObjectTemplate__NewInstance(
          prop.value as *const ObjectTemplate,
          context,
        );
        slot_of(o)
      }
      _ => slot_of(prop.value as *const Data),
    };
    if value_slot < 0 {
      continue;
    }
    unsafe {
      v8x_hermes_object_define_property(
        rtw,
        obj_slot,
        prop.key_slot,
        value_slot,
        prop.attr,
      );
    }
  }

  // Apply each native-data-property accessor.
  for acc in &t.accessors {
    unsafe {
      v8x_hermes_object_define_accessor(
        rtw,
        obj_slot,
        acc.key_slot,
        acc.getter_bits,
        acc.setter_bits,
        acc.data_slot,
        acc.attr,
      );
    }
  }

  // Apply each FunctionTemplate-pair accessor property.
  for ap in &t.accessor_props {
    let getter_fn = if ap.getter_templ.is_null() {
      NULL_SLOT
    } else {
      slot_of(v8__FunctionTemplate__GetFunction(
        ap.getter_templ as *const FunctionTemplate,
        context,
      ))
    };
    let setter_fn = if ap.setter_templ.is_null() {
      NULL_SLOT
    } else {
      slot_of(v8__FunctionTemplate__GetFunction(
        ap.setter_templ as *const FunctionTemplate,
        context,
      ))
    };
    unsafe {
      v8x_hermes_object_define_accessor_fns(
        rtw,
        obj_slot,
        ap.key_slot,
        getter_fn,
        setter_fn,
        ap.attr,
      );
    }
  }

  // If this ObjectTemplate was created from a FunctionTemplate
  // (`new_from_template`), link the instance to that constructor so
  // `instance.constructor(.name)` resolves to it (object_template_from_
  // function_template).
  if !t.from_function_template.is_null() {
    let ctor = v8__FunctionTemplate__GetFunction(
      t.from_function_template as *const FunctionTemplate,
      context,
    );
    let ctor_slot = slot_of(ctor);
    if ctor_slot >= 0 {
      unsafe {
        v8x_hermes_set_prototype_from_ctor(rtw, obj_slot, ctor_slot);
      }
    }
  }

  slot_ptr::<Object>(obj_slot)
}

/// `Object::DefineOwnProperty`: define a property with explicit attribute bits
/// via `Object.defineProperty` (a data property with value + writable/
/// enumerable/configurable from `attr`). Needed by `object_template` (it
/// installs `g` on the global with `DONT_ENUM` and reads back the descriptor).
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__DefineOwnProperty(
  this: *const Object,
  context: *const Context,
  key: *const Name,
  value: *const Value,
  attr: c_int,
) -> MaybeBool {
  if this.is_null() || context.is_null() || key.is_null() || value.is_null() {
    return MaybeBool::Nothing;
  }
  let rtw = iso_state(context as *mut RealIsolate).rtw;
  let ok = unsafe {
    v8x_hermes_object_define_property(
      rtw,
      slot_of(this),
      slot_of(key),
      slot_of(value),
      attr,
    )
  };
  if ok != 0 {
    MaybeBool::JustTrue
  } else {
    MaybeBool::Nothing
  }
}

// ---- Object internal fields (C11) ------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__InternalFieldCount(
  this: *const Object,
) -> crate::support::int {
  let rtw = current_rtw();
  if rtw.is_null() || this.is_null() {
    return 0;
  }
  let n = unsafe { v8x_hermes_object_internal_field_count(rtw, slot_of(this)) };
  if n < 0 {
    0
  } else {
    n as crate::support::int
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetInternalField(
  this: *const Object,
  index: crate::support::int,
) -> *const Data {
  let rtw = current_rtw();
  if rtw.is_null() || this.is_null() || index < 0 {
    return ptr::null();
  }
  let slot = unsafe {
    v8x_hermes_object_get_internal_field(rtw, slot_of(this), index as i64)
  };
  slot_ptr::<Data>(slot)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetInternalField(
  this: *const Object,
  index: crate::support::int,
  data: *const Data,
) {
  let rtw = current_rtw();
  if rtw.is_null() || this.is_null() || index < 0 {
    return;
  }
  unsafe {
    v8x_hermes_object_set_internal_field(
      rtw,
      slot_of(this),
      index as i64,
      slot_of(data),
    );
  }
}

/// `Object::GetAlignedPointerFromInternalField`: the internal field stores an
/// `External` (a JSI HostObject carrying the pointer); unwrap it back. The
/// `tag` is not modeled (Hermes has no per-field tag), matching the tests that
/// exercise this only through matching Set/Get pairs.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__GetAlignedPointerFromInternalField(
  this: *const Object,
  index: crate::support::int,
  _tag: u16,
) -> *const c_void {
  let rtw = current_rtw();
  if rtw.is_null() || this.is_null() || index < 0 {
    return ptr::null();
  }
  let slot = unsafe {
    v8x_hermes_object_get_internal_field(rtw, slot_of(this), index as i64)
  };
  if slot < 0 {
    return ptr::null();
  }
  let mut found: c_int = 0;
  unsafe { v8x_hermes_external_value(rtw, slot, &mut found) as *const c_void }
}

/// `Object::SetAlignedPointerInInternalField`: wrap the pointer in an
/// `External` and store it in the internal-field slot (see Get above).
#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetAlignedPointerInInternalField(
  this: *const Object,
  index: crate::support::int,
  value: *const c_void,
  _tag: u16,
) {
  let rtw = current_rtw();
  if rtw.is_null() || this.is_null() || index < 0 {
    return;
  }
  let ext_slot = unsafe { v8x_hermes_external_new(rtw, value as *mut c_void) };
  if ext_slot < 0 {
    return;
  }
  unsafe {
    v8x_hermes_object_set_internal_field(
      rtw,
      slot_of(this),
      index as i64,
      ext_slot,
    );
  }
}

// ---- Object::SetAccessor (accessor registered directly on an object) -------

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__SetAccessor(
  this: *const Object,
  context: *const Context,
  key: *const Name,
  getter: *const c_void,
  setter: *const c_void,
  data_or_null: *const Value,
  attr: c_int,
) -> MaybeBool {
  if this.is_null() || context.is_null() || key.is_null() {
    return MaybeBool::Nothing;
  }
  let rtw = iso_state(context as *mut RealIsolate).rtw;
  let ok = unsafe {
    v8x_hermes_object_define_accessor(
      rtw,
      slot_of(this),
      slot_of(key),
      getter as usize,
      setter as usize,
      slot_of(data_or_null),
      attr,
    )
  };
  if ok != 0 {
    MaybeBool::JustTrue
  } else {
    MaybeBool::Nothing
  }
}

// ---- Data template predicates (C11) ----------------------------------------

/// `Data::IsFunctionTemplate`: true when the handle is a FunctionTemplate
/// pointer (raw, even) with the FN kind, not a tagged Local or ObjectTemplate.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsFunctionTemplate(this: *const Data) -> bool {
  template_kind(this as *const c_void) == Some(TEMPLATE_KIND_FN)
}

/// `Data::IsObjectTemplate`: true when the handle is an ObjectTemplate pointer.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsObjectTemplate(this: *const Data) -> bool {
  template_kind(this as *const c_void) == Some(TEMPLATE_KIND_OBJ)
}

/// `Data::IsValue`: true when the handle is a real JS value (a tagged
/// handle-table Local, low bit set), not a template pointer. Internal-field
/// data read back through `GetInternalField` are real values, so
/// `data.is_value()` holds for them (the `object_template` test asserts this).
#[unsafe(no_mangle)]
pub extern "C" fn v8__Data__IsValue(this: *const Data) -> bool {
  let bits = this as usize;
  bits != 0 && (bits & 1) == 1
}

// ---- PropertyCallbackInfo accessors (C11) ----------------------------------
//
// The Rust-owned backing object a `*const PropertyCallbackInfo<T>` points at
// during a native accessor callback. Parallel to `CbInfo` (C10) but for
// accessors: it carries holder/data/key and a boxed return slot the getter's
// ReturnValue setters mutate.

struct PropCbInfo {
  isolate: *mut RealIsolate,
  holder: usize,
  data: usize,
  return_slot: Box<usize>,
}

#[inline]
fn propcbinfo<'a>(this: *const c_void) -> &'a mut PropCbInfo {
  unsafe { &mut *(this as *mut PropCbInfo) }
}

/// The C++ accessor-getter host function calls this. Builds a
/// `PropertyCallbackInfo<Value>`, runs the getter (an
/// `AccessorNameGetterCallback = fn(SealedLocal<Name>, *const
/// PropertyCallbackInfo<Value>)`), and returns the handle-table slot of the
/// getter's ReturnValue (or NULL_SLOT for undefined). `*threw` is set if the
/// callback left a pending exception.
#[unsafe(no_mangle)]
pub extern "C" fn v8x_hermes_dispatch_accessor_getter(
  _rtw: *mut c_void,
  getter_bits: usize,
  key_slot: i64,
  holder_slot: i64,
  data_slot: i64,
  threw: *mut c_int,
) -> i64 {
  if !threw.is_null() {
    unsafe { *threw = 0 };
  }
  if getter_bits == 0 {
    return NULL_SLOT;
  }
  let iso = current_iso();
  let undef_slot = if iso.is_null() {
    NULL_SLOT
  } else {
    unsafe { v8x_hermes_undefined(iso_state(iso).rtw) }
  };

  let mut info = Box::new(PropCbInfo {
    isolate: iso,
    holder: slot_ptr::<Value>(holder_slot) as usize,
    data: slot_ptr::<Value>(data_slot) as usize,
    return_slot: Box::new(slot_ptr::<Value>(undef_slot) as usize),
  });

  // The accessor getter's ABI: fn(SealedLocal<Name>, *const
  // PropertyCallbackInfo<Value>). A SealedLocal<Name> is a NonNull<Name>,
  // ABI-identical to `*const Name`.
  type GetterFn = unsafe extern "C" fn(*const Name, *const c_void);
  let getter: GetterFn = unsafe { std::mem::transmute::<usize, GetterFn>(getter_bits) };
  let key_ptr = slot_ptr::<Name>(key_slot);
  let info_ptr = &mut *info as *mut PropCbInfo as *const c_void;
  unsafe { (getter)(key_ptr, info_ptr) };

  let ret_tagged = *info.return_slot as *const Value;
  let ret_slot = slot_of(ret_tagged);

  surface_pending_exception(iso, threw);
  ret_slot
}

/// The C++ accessor-setter host function calls this. Builds a
/// `PropertyCallbackInfo<()>`, runs the setter (an `AccessorNameSetterCallback
/// = fn(SealedLocal<Name>, SealedLocal<Value>, *const
/// PropertyCallbackInfo<()>)`). The ReturnValue is ignored for accessors.
#[unsafe(no_mangle)]
pub extern "C" fn v8x_hermes_dispatch_accessor_setter(
  _rtw: *mut c_void,
  setter_bits: usize,
  key_slot: i64,
  value_slot: i64,
  holder_slot: i64,
  data_slot: i64,
  threw: *mut c_int,
) {
  if !threw.is_null() {
    unsafe { *threw = 0 };
  }
  if setter_bits == 0 {
    return;
  }
  let iso = current_iso();
  let undef_slot = if iso.is_null() {
    NULL_SLOT
  } else {
    unsafe { v8x_hermes_undefined(iso_state(iso).rtw) }
  };

  let mut info = Box::new(PropCbInfo {
    isolate: iso,
    holder: slot_ptr::<Value>(holder_slot) as usize,
    data: slot_ptr::<Value>(data_slot) as usize,
    return_slot: Box::new(slot_ptr::<Value>(undef_slot) as usize),
  });

  type SetterFn =
    unsafe extern "C" fn(*const Name, *const Value, *const c_void);
  let setter: SetterFn = unsafe { std::mem::transmute::<usize, SetterFn>(setter_bits) };
  let key_ptr = slot_ptr::<Name>(key_slot);
  let value_ptr = slot_ptr::<Value>(value_slot);
  let info_ptr = &mut *info as *mut PropCbInfo as *const c_void;
  unsafe { (setter)(key_ptr, value_ptr, info_ptr) };

  surface_pending_exception(iso, threw);
}

/// Shared: after a native callback returns, surface any pending exception the
/// isolate captured (Isolate::ThrowException during the callback) to the C++
/// host function, so it re-throws it as a jsi::JSError. Same plumbing C10 uses.
#[inline]
fn surface_pending_exception(iso: *mut RealIsolate, threw: *mut c_int) {
  if iso.is_null() {
    return;
  }
  let st = iso_state(iso);
  if st.pending_exception >= 0 {
    let exc = st.pending_exception;
    st.pending_exception = NULL_SLOT;
    unsafe {
      v8x_hermes_set_pending_callback_exception(st.rtw, exc);
    }
    if !threw.is_null() {
      unsafe { *threw = 1 };
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyCallbackInfo__GetIsolate(
  this: *const c_void,
) -> *mut RealIsolate {
  if this.is_null() {
    return current_iso();
  }
  propcbinfo(this).isolate
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyCallbackInfo__Data(
  this: *const c_void,
) -> *const Value {
  if this.is_null() {
    return ptr::null();
  }
  propcbinfo(this).data as *const Value
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyCallbackInfo__Holder(
  this: *const c_void,
) -> *const Object {
  if this.is_null() {
    return ptr::null();
  }
  propcbinfo(this).holder as *const Object
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyCallbackInfo__GetReturnValue(
  this: *const c_void,
) -> usize {
  if this.is_null() {
    return 0;
  }
  let info = propcbinfo(this);
  (&mut *info.return_slot as *mut usize) as usize
}

/// `ShouldThrowOnError`: always false. Every vendored accessor test in this
/// cluster asserts `!args.should_throw_on_error()`, so false is exactly
/// correct (strict-mode routing is not modeled).
#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyCallbackInfo__ShouldThrowOnError(
  _this: *const c_void,
) -> bool {
  false
}

// ---- Value integer coercions (C11 needs) -----------------------------------

/// `Value::IntegerValue`: ECMAScript ToInteger of a number, written into
/// `Maybe<i64>`. Hermes stores numbers as doubles; truncate toward zero.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IntegerValue(
  this: *const Value,
  context: *const Context,
  out: *mut crate::support::Maybe<i64>,
) {
  #[repr(C)]
  struct MaybeI64 {
    has_value: bool,
    value: i64,
  }
  let out = out as *mut MaybeI64;
  if out.is_null() {
    return;
  }
  if this.is_null() || context.is_null() {
    unsafe { ptr::write(out, MaybeI64 { has_value: false, value: 0 }) };
    return;
  }
  match number_value_opt(this) {
    Some(v) if v.is_finite() => unsafe {
      ptr::write(
        out,
        MaybeI64 {
          has_value: true,
          value: v.trunc() as i64,
        },
      )
    },
    _ => unsafe {
      ptr::write(out, MaybeI64 { has_value: false, value: 0 })
    },
  }
}

/// `Value::ToInteger`: ECMAScript ToInteger, returning a Number handle holding
/// the truncated integer value.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__ToInteger(
  this: *const Value,
  context: *const Context,
) -> *const Integer {
  if this.is_null() || context.is_null() {
    return ptr::null();
  }
  let isolate = context as *mut RealIsolate;
  match number_value_opt(this) {
    Some(v) if v.is_finite() => {
      v8__Number__New(isolate, v.trunc()) as *const Integer
    }
    Some(_) => v8__Number__New(isolate, 0.0) as *const Integer,
    None => ptr::null(),
  }
}

// ---- TryCatch / exception surfacing (C9) -----------------------------------
//
// v8's TryCatch is a stack-discipline scope (like HandleScope): CONSTRUCT
// pushes a frame on the isolate's exception-capture stack, DESTRUCT pops it.
// While one is on top, `Script::Run`/`Function::Call` route a thrown
// `jsi::JSError` into it (see `RuntimeWrapper::capture_exception` in
// hermes_shim.cpp) instead of just returning a null Local. The frame stack
// itself lives in C++ (`RuntimeWrapper::tc_stack`); Rust only carries the
// `(isolate, frame_index)` pair.
//
// The vendored `raw::TryCatch` buffer is `[MaybeUninit<usize>; 6]` (48
// bytes); linking is name-only so only OUR read/write of this buffer needs
// to agree on layout. `[0]` = isolate ptr, `[1]` = the C++ tc_stack frame
// index this scope owns.

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__CONSTRUCT(
  buf: *mut usize,
  isolate: *mut RealIsolate,
) {
  let rtw = if isolate.is_null() {
    current_rtw()
  } else {
    iso_state(isolate).rtw
  };
  let index = if rtw.is_null() {
    -1
  } else {
    unsafe { v8x_hermes_trycatch_push(rtw) }
  };
  unsafe {
    *buf.offset(0) = isolate as usize;
    *buf.offset(1) = index as usize;
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__DESTRUCT(this: *mut usize) {
  unsafe {
    let isolate = *this.offset(0) as *mut RealIsolate;
    let index = *this.offset(1) as i64;
    if isolate.is_null() || index < 0 {
      return;
    }
    v8x_hermes_trycatch_pop(iso_state(isolate).rtw, index);
  }
}

/// Read `(rtw, frame_index)` out of a `raw::TryCatch` buffer, or `None` if
/// the scope failed to acquire a frame (`CONSTRUCT` saw a null isolate/rtw).
#[inline]
fn trycatch_frame(this: *const usize) -> Option<(*mut c_void, i64)> {
  if this.is_null() {
    return None;
  }
  unsafe {
    let isolate = *this.offset(0) as *mut RealIsolate;
    let index = *this.offset(1) as i64;
    if isolate.is_null() || index < 0 {
      return None;
    }
    Some((iso_state(isolate).rtw, index))
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__HasCaught(this: *const usize) -> bool {
  match trycatch_frame(this) {
    Some((rtw, index)) => unsafe { v8x_hermes_trycatch_has_caught(rtw, index) != 0 },
    None => false,
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__Exception(this: *const usize) -> *const Value {
  match trycatch_frame(this) {
    Some((rtw, index)) => {
      let slot = unsafe { v8x_hermes_trycatch_exception(rtw, index) };
      slot_ptr::<Value>(slot)
    }
    None => ptr::null(),
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__Message(this: *const usize) -> *const Message {
  match trycatch_frame(this) {
    Some((rtw, index)) => {
      let slot = unsafe { v8x_hermes_trycatch_message(rtw, index) };
      slot_ptr::<Message>(slot)
    }
    None => ptr::null(),
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__StackTrace(
  this: *const usize,
  _context: *const Context,
) -> *const Value {
  match trycatch_frame(this) {
    Some((rtw, index)) => {
      let slot = unsafe { v8x_hermes_trycatch_stack_trace(rtw, index) };
      slot_ptr::<Value>(slot)
    }
    None => ptr::null(),
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__Reset(this: *mut usize) {
  if let Some((rtw, index)) = trycatch_frame(this) {
    unsafe { v8x_hermes_trycatch_reset(rtw, index) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__ReThrow(this: *mut usize) -> *const Value {
  match trycatch_frame(this) {
    Some((rtw, index)) => {
      let slot = unsafe { v8x_hermes_trycatch_rethrow(rtw, index) };
      slot_ptr::<Value>(slot)
    }
    None => ptr::null(),
  }
}

/// Whether execution can continue (always true: we never model a fatal
/// termination exception, only regular JS throws).
#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__CanContinue(this: *const usize) -> bool {
  trycatch_frame(this).is_some()
}

/// Never models script termination (`Isolate::TerminateExecution` is not
/// implemented), so always false.
#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__HasTerminated(_this: *const usize) -> bool {
  false
}

// Verbose mode / capture-message toggles have no observable effect in this
// model (a Message is always synthesized on demand from the captured
// JSError, never routed to an isolate-level message listener), so these are
// safe no-ops mirroring the vendored setter/getter shape.
#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__IsVerbose(_this: *const usize) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__SetVerbose(_this: *mut usize, _value: bool) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TryCatch__SetCaptureMessage(
  _this: *mut usize,
  _value: bool,
) {
}

// ---- Isolate::ThrowException + Exception::* constructors (C9) -------------
//
// The embedder throwing a value: captured straight into the innermost live
// TryCatch frame (same sink `capture_exception` uses for a caught
// `jsi::JSError`), since this backend has no separate "isolate pending
// exception" object distinct from a TryCatch frame. Returns the vendored
// contract's `*const Value` (v8 hands back the same exception value it was
// given); v8 returns it to make chaining easy, callers rarely use the result.

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__ThrowException(
  isolate: *mut RealIsolate,
  exception: *const Value,
) -> *const Value {
  if isolate.is_null() || exception.is_null() {
    return ptr::null();
  }
  let st = iso_state(isolate);
  let rtw = st.rtw;
  let slot = slot_of(exception);
  // C9 path: capture into the innermost live TryCatch frame, if any.
  unsafe { v8x_hermes_throw_exception(rtw, slot) };
  // C10 path: also record it as the isolate's pending exception so a native
  // FunctionCallback that throws surfaces the error through JSI (the
  // dispatch trampoline reads this after the callback returns). Cleared by the
  // trampoline; harmless if no callback is running (nothing reads it).
  st.pending_exception = slot;
  exception
}

/// `Exception::Error/RangeError/ReferenceError/SyntaxError/TypeError`:
/// construct (but do not throw) a JS Error subtype via its JS global
/// constructor, since JSI exposes no C++ Error-subtype factory.
fn exception_new(
  ctor_name: &std::ffi::CStr,
  message: *const V8String,
) -> *const Value {
  let rtw = current_rtw();
  if rtw.is_null() || message.is_null() {
    return ptr::null();
  }
  let slot = unsafe {
    v8x_hermes_exception_new(rtw, ctor_name.as_ptr(), slot_of(message))
  };
  slot_ptr::<Value>(slot)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Exception__Error(message: *const V8String) -> *const Value {
  exception_new(c_str!("Error"), message)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Exception__RangeError(
  message: *const V8String,
) -> *const Value {
  exception_new(c_str!("RangeError"), message)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Exception__ReferenceError(
  message: *const V8String,
) -> *const Value {
  exception_new(c_str!("ReferenceError"), message)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Exception__SyntaxError(
  message: *const V8String,
) -> *const Value {
  exception_new(c_str!("SyntaxError"), message)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Exception__TypeError(
  message: *const V8String,
) -> *const Value {
  exception_new(c_str!("TypeError"), message)
}

/// `Exception::CreateMessage`: build a `Message` from an exception value.
///
/// Our `Message` is modeled as a String slot holding the message text (see
/// `Message::Get`, which just re-tags the slot). V8's message text is the
/// stringification of the exception (`"Uncaught <Error>: <msg>"`-ish). We coerce
/// the exception value to a string via the shim and intern that as the Message.
/// deno_core reads the structured `.message`/stack off the Error object itself;
/// the line/column accessors are best-effort (their stubs return null, which
/// `JsError::from_v8_message` handles as `None`). Returning a non-null Message
/// here is what stops `create_message(...).unwrap()` from panicking on an empty
/// handle when an ext script fails to compile.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Exception__CreateMessage(
  isolate: *mut RealIsolate,
  exception: *const Value,
) -> *const Message {
  if exception.is_null() {
    return ptr::null();
  }
  let rtw = if isolate.is_null() {
    current_rtw()
  } else {
    iso_state(isolate).rtw
  };
  if rtw.is_null() {
    return ptr::null();
  }
  let text = read_string(rtw, slot_of(exception))
    .unwrap_or_else(|| "uncaught exception".to_string());
  let slot = unsafe {
    v8x_hermes_string_new_utf8(
      rtw,
      text.as_ptr() as *const c_char,
      text.len(),
    )
  };
  if slot < 0 {
    return ptr::null();
  }
  slot_ptr::<Message>(slot)
}

/// `Message::Get`: the synthesized "Uncaught ..." text is already a String
/// slot (built by `v8x_hermes_trycatch_message`), so the `Message` handle IS
/// that String handle; this just re-tags it as `*const String` (a no-op:
/// same slot, different vendored phantom type).
#[unsafe(no_mangle)]
pub extern "C" fn v8__Message__Get(this: *const Message) -> *const V8String {
  if this.is_null() {
    return ptr::null();
  }
  this as *const V8String
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
/// `*mut Platform` the shared-pointer carries. `custom_context` carries the
/// double-boxed `PlatformImpl` deno_core passes to `NewCustomPlatform` (a raw
/// pointer we never call back into, since Hermes drives its own JSI job queue
/// and has no V8 task platform); it is dropped in `drop_platform` so the boxed
/// impl does not leak.
struct HermesPlatform {
  custom_context: *mut ::std::ffi::c_void,
}

fn new_platform() -> *mut Platform {
  new_custom_platform(::std::ptr::null_mut())
}

fn new_custom_platform(custom_context: *mut ::std::ffi::c_void) -> *mut Platform {
  Box::into_raw(Box::new(HermesPlatform { custom_context })) as *mut Platform
}

unsafe extern "C" {
  // Defined by the vendored rusty_v8 `platform` module (#[no_mangle]); frees the
  // double-boxed `PlatformImpl` context deno_core passes to NewCustomPlatform.
  fn v8__Platform__CustomPlatform__BASE__DROP(context: *mut ::std::ffi::c_void);
}

unsafe fn drop_platform(platform: *mut Platform) {
  if !platform.is_null() {
    let hp = unsafe { Box::from_raw(platform as *mut HermesPlatform) };
    // Release the double-boxed `PlatformImpl` deno_core handed to
    // NewCustomPlatform. The vendored binding frees it through this drop
    // shim; Hermes has no C++ platform object to do so, so we call it here.
    if !hp.custom_context.is_null() {
      unsafe { v8__Platform__CustomPlatform__BASE__DROP(hp.custom_context) };
    }
    drop(hp);
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

/// `new_custom_platform`: deno_core (`v8_init`) always builds a *custom*
/// platform, handing us a boxed `PlatformImpl` for foreground task ownership.
/// Hermes has no V8 task platform (it drives its own JSI job/microtask queue),
/// so the impl is never called back into; we return the same inert marker as
/// the default platform, carrying the context so it is freed at teardown.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Platform__NewCustomPlatform(
  _thread_pool_size: c_int,
  _idle_task_support: bool,
  _unprotected: bool,
  context: *mut ::std::ffi::c_void,
) -> *mut Platform {
  new_custom_platform(context)
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

/// `Isolate::perform_microtask_checkpoint`: drain the JS microtask/job queue,
/// running all pending promise reactions and enqueued microtasks. Routes to
/// Hermes's `jsi::Runtime::drainMicrotasks` (D1). A throwing microtask is
/// swallowed at the C++ boundary, never unwinding across the C ABI.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__PerformMicrotaskCheckpoint(
  isolate: *mut RealIsolate,
) {
  if isolate.is_null() {
    return;
  }
  let rtw = iso_state(isolate).rtw;
  unsafe {
    v8x_hermes_drain_microtasks(rtw);
  }
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
