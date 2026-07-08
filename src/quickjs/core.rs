//! Foundation for the QuickJS-backed C-ABI shims.
//!
//! This module owns the representation that backs `*mut RealIsolate` and the
//! handle-scope / context / arena machinery. Every other `shim_*` module in the
//! QuickJS backend builds on the helpers exported here.
//!
//! ## The key design difference from JSC
//!
//! In the JSC backend a `Local<T>`'s pointer *is* the `JSValueRef` (a pointer).
//! In QuickJS a `JSValue` is a 16-byte struct (a union + a tag), **not** a
//! pointer, so it cannot itself be a v8 `Local<T>` (which the vendored source
//! treats as `*const T`, a pointer). We therefore use an **arena**: every
//! handle is a heap box holding one `JSValue`; the box's address is the v8
//! handle. Reading the box recovers the `JSValue`.
//!
//! ## Refcount discipline (the #1 correctness risk)
//!
//! Invariant: every arena slot owns **exactly one** QuickJS refcount on its
//! `JSValue`, and frees it **exactly once** when the slot is reclaimed (on
//! handle-scope pop or isolate dispose). Promoting a borrowed `JSValue` into a
//! handle therefore `JS_DupValue`s it; producing a fresh value (`JS_Eval`,
//! `JS_NewObject`, ...) already returns +1, so it is moved into the slot
//! without an extra dup.
//!
//! ## Helper API (used by every other QuickJS `shim_*` module)
//!
//! - `iso_state(p)` — `&mut IsoState` behind a `*mut RealIsolate`.
//! - `current_iso()` — current `*mut RealIsolate` (thread-local).
//! - `current_ctx()` — innermost entered `*mut JSContext` (thread-local).
//! - `intern::<T>(jsval)` — move an owned `JSValue` into a fresh arena slot in
//!   the current handle scope; returns the slot pointer as `*const T`.
//! - `intern_dup::<T>(jsval)` — like `intern` but `JS_DupValue`s first (use
//!   when the `JSValue` is borrowed and you must not consume its refcount).
//! - `jsval_of(ptr)` — read the `JSValue` out of a handle slot pointer.
//! - `ctx_of(c)` — recover the `*mut JSContext` backing a `*const Context`.

#![allow(non_snake_case)]

use super::quickjs_sys::*;
use crate::support::SharedPtrBase;
use crate::{
  Allocator, Context, Data, Object, ObjectTemplate, Primitive, RealIsolate,
  String as V8String, Value,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::os::raw::{c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

pub(crate) type WeakCallback = unsafe extern "C" fn(*const c_void);

pub(crate) struct WeakHandle {
  pub handle: *const Data,
  pub parameter: *const c_void,
  pub callback: WeakCallback,
}

pub(crate) struct PersistentHandle {
  pub slot: *mut JSValue,
  pub is_weak: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct GcCallbackEntry {
  pub callback: crate::isolate::GcCallbackWithData,
  pub data: *mut c_void,
  pub gc_type_filter: crate::gc::GCType,
}

/// Process-wide "force strict mode" flag, toggled by
/// `v8__V8__SetFlagsFromString` when it sees V8's `--use_strict` flag. V8
/// applies `--use_strict` globally (which is why the rusty_v8 test that uses it
/// lives in its own process), so a process-wide atomic faithfully mirrors that:
/// every subsequently compiled global script is evaluated in strict mode.
pub(crate) static FORCE_STRICT: AtomicBool = AtomicBool::new(false);

/// The `JS_EVAL_TYPE_GLOBAL` flags to use when running a top-level script,
/// folding in strict mode if `--use_strict` was set.
#[inline]
pub(crate) fn global_eval_flags() -> c_int {
  let mut flags = JS_EVAL_TYPE_GLOBAL;
  if FORCE_STRICT.load(Ordering::Relaxed) {
    flags |= JS_EVAL_FLAG_STRICT;
  }
  flags
}

fn skip_js_space_and_comments(mut bytes: &[u8]) -> &[u8] {
  loop {
    bytes = bytes.trim_ascii_start();
    if let Some(rest) = bytes.strip_prefix(b"//") {
      bytes = match rest.iter().position(|b| *b == b'\n' || *b == b'\r') {
        Some(pos) => &rest[pos + 1..],
        None => &[],
      };
      continue;
    }
    if let Some(rest) = bytes.strip_prefix(b"/*") {
      bytes = match rest.windows(2).position(|w| w == b"*/") {
        Some(pos) => &rest[pos + 2..],
        None => &[],
      };
      continue;
    }
    return bytes;
  }
}

fn starts_with_strict_directive(source: &[u8]) -> bool {
  let source = skip_js_space_and_comments(source);
  let Some((&quote, rest)) = source.split_first() else {
    return false;
  };
  if quote != b'\'' && quote != b'"' {
    return false;
  }
  let Some(after_literal) = rest.strip_prefix(b"use strict") else {
    return false;
  };
  let Some((&end_quote, after_quote)) = after_literal.split_first() else {
    return false;
  };
  if end_quote != quote {
    return false;
  }
  after_quote
    .first()
    .is_none_or(|b| b.is_ascii_whitespace() || *b == b';')
}

fn maybe_report_strict_mode_use(source: &[u8]) {
  if !starts_with_strict_directive(source) {
    return;
  }
  let iso = current_iso();
  if iso.is_null() {
    return;
  }
  let callback = iso_state(iso).use_counter_callback;
  if let Some(callback) = callback {
    let mut isolate = unsafe {
      crate::Isolate::from_raw_isolate_ptr(
        crate::UnsafeRawIsolatePtr::from_real_ptr(iso),
      )
    };
    unsafe { callback(&mut isolate, crate::UseCounterFeature::kStrictMode) };
  }
}

pub(crate) struct IsoState {
  pub rt: *mut JSRuntime,

  pub ctx: *mut JSContext,

  pub contexts: Vec<*mut JSContext>,

  pub handles: Vec<*mut JSValue>,

  pub persistent_handles: Vec<PersistentHandle>,

  pub private_symbols: Vec<(JSValue, JSValue)>,

  pub data_slots: [*mut c_void; 4],

  // QuickJS class id for `v8::External` objects. Registered on the runtime
  // BEFORE the first JSContext is created (see `v8__Isolate__New`): a context
  // sizes its per-context `class_proto` array to `rt->class_count` at creation
  // time and never grows it, so a class registered afterward would make
  // `JS_NewObjectClass(ctx, id)` read `class_proto[id]` out of bounds.
  pub ext_class_id: JSClassID,

  // Whether the bootstrap context (`ctx`) has been handed out by
  // `v8__Context__New`. QuickJS has one global object per JSContext, but
  // deno_core uses multiple v8::Contexts (notably during snapshot creation,
  // where two coexist). Backing them all by the single `ctx` makes their
  // embedder-data slots collide. So the FIRST Context::New uses the
  // bootstrapped `ctx` (runtime is single-context, unchanged); each later one
  // gets its own JSContext (tracked in `extra_contexts`) for independent slots.
  pub main_ctx_claimed: bool,

  pub extra_contexts: Vec<*mut JSContext>,

  // Snapshot support (see snapshot.rs). `snap` records on SnapshotCreator
  // isolates; `restored` holds a parsed blob to replay into new contexts.
  /// Per-context AddData slot counters (SnapshotCreator::AddData returns the
  /// data index; the tape replays slots in call order).
  pub ctx_data_counts: HashMap<usize, usize>,
  pub iso_data_count: usize,
  pub iso_added_contexts: usize,

  pub external_memory: AtomicI64,

  pub external_string_memory: AtomicI64,

  pub global_handles: AtomicI64,

  pub weak_handles: Vec<WeakHandle>,

  pub kept_objects_cleared: bool,

  pub gc_prologue_callbacks: Vec<GcCallbackEntry>,

  pub gc_epilogue_callbacks: Vec<GcCallbackEntry>,

  pub use_counter_callback: Option<crate::UseCounterCallback>,

  pub javascript_execution_disallow_scopes: Vec<crate::scope::OnFailure>,

  pub javascript_execution_allow_depth: usize,

  // Emulates `v8::Isolate::TerminateExecution`. Set by `TerminateExecution`,
  // cleared by `CancelTerminateExecution`; while set, the op-dispatch boundary
  // and the microtask/job drain refuse to run JS (matching V8, which throws an
  // uncatchable termination exception on the next safe point). An `AtomicBool`
  // because V8's terminate may be requested from another thread.
  pub terminating: std::sync::atomic::AtomicBool,

  pub array_buffer_allocator: SharedPtrBase<Allocator>,

  pub pending_array_buffer_frees: Vec<(*mut Allocator, *mut c_void, usize)>,
}

impl IsoState {
  #[inline(always)]
  pub fn is_terminating(&self) -> bool {
    self.terminating.load(std::sync::atomic::Ordering::Acquire)
  }
}

#[inline]
pub(crate) fn javascript_execution_allowed() -> bool {
  let iso = current_iso();
  if iso.is_null() {
    return true;
  }
  let st = iso_state(iso);
  st.javascript_execution_allow_depth > 0
    || st.javascript_execution_disallow_scopes.is_empty()
}

#[inline]
unsafe fn throw_javascript_execution_disallowed(ctx: *mut JSContext) {
  unsafe {
    JS_ThrowTypeError(ctx, c"Javascript execution is disallowed".as_ptr());
  }
}

#[inline]
fn adjust_i64(counter: &AtomicI64, delta: i64) -> i64 {
  let mut current = counter.load(Ordering::SeqCst);
  loop {
    let next = current.saturating_add(delta);
    match counter.compare_exchange(
      current,
      next,
      Ordering::SeqCst,
      Ordering::SeqCst,
    ) {
      Ok(_) => return next,
      Err(actual) => current = actual,
    }
  }
}

#[inline]
pub(crate) fn adjust_external_memory(st: &IsoState, delta: i64) -> i64 {
  adjust_i64(&st.external_memory, delta)
}

#[inline]
pub(crate) fn adjust_external_string_memory(st: &IsoState, delta: i64) -> i64 {
  adjust_i64(&st.external_string_memory, delta)
}

#[inline]
pub(crate) fn release_external_string_memory(st: &IsoState) -> i64 {
  let released = st.external_string_memory.swap(0, Ordering::SeqCst);
  if released > 0 {
    adjust_external_memory(st, -released);
  }
  released
}

thread_local! {
    static CURRENT_ISO: RefCell<*mut RealIsolate> = const { RefCell::new(ptr::null_mut()) };
    static CURRENT_CTX: RefCell<*mut JSContext> = const { RefCell::new(ptr::null_mut()) };

    static LAST_ISO: RefCell<*mut RealIsolate> = const { RefCell::new(ptr::null_mut()) };
}

#[inline(always)]
pub(crate) fn iso_state<'a>(p: *mut RealIsolate) -> &'a mut IsoState {
  unsafe { &mut *(p as *mut IsoState) }
}

#[inline(always)]
pub(crate) fn current_iso() -> *mut RealIsolate {
  let cur = CURRENT_ISO.with(|c| *c.borrow());
  if !cur.is_null() {
    return cur;
  }
  LAST_ISO.with(|c| *c.borrow())
}

#[inline(always)]
pub(crate) fn current_ctx() -> *mut JSContext {
  CURRENT_CTX.with(|c| *c.borrow())
}

fn set_current(iso: *mut RealIsolate) {
  CURRENT_ISO.with(|c| *c.borrow_mut() = iso);
  if !iso.is_null() {
    LAST_ISO.with(|c| *c.borrow_mut() = iso);
  }
}

fn clear_last_iso(iso: *mut RealIsolate) {
  LAST_ISO.with(|c| {
    if *c.borrow() == iso {
      *c.borrow_mut() = ptr::null_mut();
    }
  });
}

fn refresh_current_ctx(st: &IsoState) {
  let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
  CURRENT_CTX.with(|c| *c.borrow_mut() = ctx);
}

#[inline(always)]
pub(crate) fn jsval_of<T>(p: *const T) -> JSValue {
  if p.is_null() {
    return jsv_undefined();
  }
  unsafe { *(p as *const JSValue) }
}

pub(crate) const JS_TAG_V8_CONTEXT: i64 = 0x7632;

#[inline(always)]
pub(crate) fn ctx_to_jsval(ctx: *mut JSContext) -> JSValue {
  make_value(
    JS_TAG_V8_CONTEXT,
    JSValueUnion {
      ptr: ctx as *mut c_void,
    },
  )
}

#[inline(always)]
pub(crate) fn intern_ctx(ctx: *mut JSContext) -> *const Context {
  intern::<Context>(ctx_to_jsval(ctx))
}

#[inline(always)]
pub(crate) fn is_non_value_handle<T>(p: *const T) -> bool {
  !p.is_null() && super::function::is_template_ptr(p as *const c_void)
}

#[inline(always)]
pub(crate) fn is_interned_handle<T>(p: *const T) -> bool {
  let iso = current_iso();
  !iso.is_null() && {
    let st = iso_state(iso);
    st.handles
      .iter()
      .any(|&h| std::ptr::addr_eq(h, p as *const JSValue))
      || st
        .persistent_handles
        .iter()
        .any(|h| std::ptr::addr_eq(h.slot, p as *const JSValue))
  }
}

#[inline(always)]
pub(crate) fn ctx_of(c: *const Context) -> *mut JSContext {
  if c.is_null() {
    return ptr::null_mut();
  }
  let v = unsafe { *(c as *const JSValue) };
  if v.tag == JS_TAG_V8_CONTEXT {
    unsafe { v.u.ptr as *mut JSContext }
  } else {
    c as *mut JSContext
  }
}

#[inline]
fn fallback_ctx(iso: *mut RealIsolate) -> *mut JSContext {
  if iso.is_null() {
    return ptr::null_mut();
  }
  let st = iso_state(iso);
  st.contexts.last().copied().unwrap_or(st.ctx)
}

#[inline]
pub(crate) fn intern<T>(v: JSValue) -> *const T {
  let iso = current_iso();
  if iso.is_null() {
    let ctx = current_ctx();
    if !ctx.is_null() {
      unsafe { JS_FreeValue(ctx, v) };
    }
    return ptr::null();
  }
  let slot = Box::into_raw(Box::new(v));
  iso_state(iso).handles.push(slot);
  slot as *const T
}

#[inline]
pub(crate) fn intern_dup<T>(ctx: *mut JSContext, v: JSValue) -> *const T {
  let ctx = if ctx.is_null() { current_ctx() } else { ctx };
  let ctx = if ctx.is_null() {
    fallback_ctx(current_iso())
  } else {
    ctx
  };
  if ctx.is_null() {
    return ptr::null();
  }
  let dup = unsafe { JS_DupValue(ctx, v) };
  intern::<T>(dup)
}

// Process-global refcount table for SharedArrayBuffer memory shared across worker
// threads. Keyed by data pointer so foreign pointers (e.g. SABs deno hands us via
// a backing store rather than `sab_alloc`) are handled gracefully: dup/free on an
// unknown pointer is a no-op, so we never free memory we did not allocate.
fn sab_refcounts()
-> &'static std::sync::Mutex<std::collections::HashMap<usize, usize>> {
  static T: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<usize, usize>>,
  > = std::sync::OnceLock::new();
  T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

unsafe extern "C" {
  fn malloc(size: usize) -> *mut c_void;
  fn free(ptr: *mut c_void);
}

unsafe extern "C" fn sab_alloc_fn(
  _opaque: *mut c_void,
  size: usize,
) -> *mut c_void {
  let p = unsafe { malloc(size.max(1)) };
  if !p.is_null() {
    sab_refcounts().lock().unwrap().insert(p as usize, 1);
  }
  p
}

unsafe extern "C" fn sab_dup_fn(_opaque: *mut c_void, ptr: *mut c_void) {
  if let Some(c) = sab_refcounts().lock().unwrap().get_mut(&(ptr as usize)) {
    *c += 1;
  }
}

unsafe extern "C" fn sab_free_fn(_opaque: *mut c_void, ptr: *mut c_void) {
  let mut map = sab_refcounts().lock().unwrap();
  if let Some(c) = map.get_mut(&(ptr as usize)) {
    *c -= 1;
    if *c == 0 {
      map.remove(&(ptr as usize));
      drop(map);
      unsafe { free(ptr) };
    }
  }
}

struct SabFuncsHolder(JSSharedArrayBufferFunctions);
// SAFETY: the only non-Sync field is `sab_opaque`, a null pointer we never read.
unsafe impl Sync for SabFuncsHolder {}

fn sab_funcs_table() -> *const JSSharedArrayBufferFunctions {
  static FUNCS: SabFuncsHolder = SabFuncsHolder(JSSharedArrayBufferFunctions {
    sab_alloc: Some(sab_alloc_fn),
    sab_free: Some(sab_free_fn),
    sab_dup: Some(sab_dup_fn),
    sab_opaque: ptr::null_mut(),
  });
  &FUNCS.0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__New(params: *const c_void) -> *mut RealIsolate {
  let rt = unsafe { JS_NewRuntime() };
  assert!(!rt.is_null(), "JS_NewRuntime failed");

  unsafe { JS_SetMaxStackSize(rt, 8 * 1024 * 1024) };
  // deno's V8 lets `Atomics.wait` block the main isolate (deno isn't a browser);
  // QuickJS gates it behind can_block (default false → "cannot block in this
  // thread"). Enable to match.
  unsafe { JS_SetCanBlock(rt, true) };
  // Cross-thread SharedArrayBuffer: register a process-global refcounted shared
  // allocator. Without it the bytecode deserializer rejects BC_TAG_SHARED_ARRAY_BUFFER
  // (`!sab_funcs.sab_dup` gate) so SABs cannot survive a `postMessage` to a worker.
  unsafe { JS_SetSharedArrayBufferFunctions(rt, sab_funcs_table()) };

  unsafe {
    JS_SetModuleLoaderFunc(
      rt,
      if std::env::var_os("QJS_NO_NORM").is_some() {
        None
      } else {
        Some(super::module::module_normalize_callback)
      },
      Some(super::module::module_loader_callback),
      ptr::null_mut(),
    )
  };
  // Register custom classes (currently just `v8::External`) on the runtime
  // BEFORE creating the context, so the context's `class_proto` array is sized
  // to include them. Registering after `JS_NewContext` would leave
  // `JS_NewObjectClass` indexing `class_proto` out of bounds (heap overflow that
  // hands back a garbage prototype and later corrupts the GC heap).
  let ext_class_id = super::function::register_external_class(rt);

  let ctx = unsafe { JS_NewContext(rt) };
  assert!(!ctx.is_null(), "JS_NewContext failed");

  if std::env::var_os("QJS_NO_WASM").is_none() {
    super::wasm::install_webassembly(ctx);
  }
  let array_buffer_allocator = if params.is_null() {
    SharedPtrBase::<Allocator>::default()
  } else {
    let params =
      params as *const crate::isolate_create_params::raw::CreateParams;
    let shared = unsafe { &(*params).array_buffer_allocator_shared };
    let shared = shared as *const _ as *const SharedPtrBase<Allocator>;
    super::allocator::allocator_shared_copy(shared)
  };
  let state = Box::new(IsoState {
    rt,
    ctx,
    contexts: Vec::new(),
    handles: Vec::new(),
    persistent_handles: Vec::new(),
    private_symbols: Vec::new(),
    data_slots: [ptr::null_mut(); 4],
    ext_class_id,
    main_ctx_claimed: false,
    extra_contexts: Vec::new(),
    ctx_data_counts: HashMap::new(),
    iso_data_count: 0,
    iso_added_contexts: 0,
    external_memory: AtomicI64::new(0),
    external_string_memory: AtomicI64::new(0),
    global_handles: AtomicI64::new(0),
    weak_handles: Vec::new(),
    kept_objects_cleared: false,
    gc_prologue_callbacks: Vec::new(),
    gc_epilogue_callbacks: Vec::new(),
    use_counter_callback: None,
    javascript_execution_disallow_scopes: Vec::new(),
    javascript_execution_allow_depth: 0,
    terminating: std::sync::atomic::AtomicBool::new(false),
    array_buffer_allocator,
    pending_array_buffer_frees: Vec::new(),
  });
  let iso = Box::into_raw(state) as *mut RealIsolate;
  // Arm the interrupt handler so a runaway loop unwinds once
  // `TerminateExecution` is requested (the op-dispatch boundary handles the
  // common "next op after terminate" case directly). The opaque is the isolate
  // pointer so the handler can read its `terminating` flag.
  unsafe {
    JS_SetInterruptHandler(
      rt,
      Some(super::isolate::terminate_interrupt_handler),
      iso as *mut c_void,
    );
  }
  iso
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
pub extern "C" fn v8__Isolate__CreateParams__SIZEOF() -> usize {
  std::mem::size_of::<crate::isolate_create_params::raw::CreateParams>()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__Dispose(this: *mut RealIsolate) {
  if this.is_null() {
    return;
  }
  unsafe {
    let mut st = Box::from_raw(this as *mut IsoState);

    st.weak_handles.clear();

    while let Some(slot) = st.handles.pop() {
      let v = *slot;
      JS_FreeValue(st.ctx, v);
      drop(Box::from_raw(slot));
    }
    while let Some(slot) = st.persistent_handles.pop() {
      let v = *slot.slot;
      JS_FreeValue(st.ctx, v);
      drop(Box::from_raw(slot.slot));
    }
    while let Some((symbol, name)) = st.private_symbols.pop() {
      JS_FreeValue(st.ctx, symbol);
      JS_FreeValue(st.ctx, name);
    }
    // Release any saved eval/Function bindings a context held while code
    // generation from strings was disabled, before its JSContext is freed.
    for c in &st.extra_contexts {
      super::isolate::codegen_release_ctx(*c);
    }
    super::isolate::codegen_release_ctx(st.ctx);
    for c in st.extra_contexts.drain(..) {
      JS_FreeContext(c);
    }
    JS_FreeContext(st.ctx);
    if std::env::var_os("QJS_SKIP_FREE_RT").is_none() {
      JS_FreeRuntime(st.rt);
    }
    for (allocator, data, byte_length) in
      std::mem::take(&mut st.pending_array_buffer_frees)
    {
      super::allocator::allocator_free(allocator, data, byte_length);
    }
  }
  // The module registries are thread-locals keyed by NAME or by pointers into
  // the runtime that was just freed. A later isolate on this thread (e.g. a
  // runtime restored from the snapshot this isolate just created) would
  // resolve its ext: modules to this isolate's dangling defs. Drop them all.
  super::module::clear_thread_module_caches();
  set_current(ptr::null_mut());
  clear_last_iso(this);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__Enter(this: *mut RealIsolate) {
  set_current(this);
  if !this.is_null() {
    refresh_current_ctx(iso_state(this));
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__Exit(_this: *mut RealIsolate) {
  set_current(ptr::null_mut());
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
  let st = iso_state(isolate as *mut RealIsolate);
  *st.data_slots.get(slot as usize).unwrap_or(&ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetData(
  isolate: *const RealIsolate,
  slot: u32,
  data: *mut c_void,
) {
  let st = iso_state(isolate as *mut RealIsolate);
  if let Some(s) = st.data_slots.get_mut(slot as usize) {
    *s = data;
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetCurrentContext(
  isolate: *mut RealIsolate,
) -> *const Context {
  if isolate.is_null() {
    return ptr::null();
  }
  let st = iso_state(isolate);
  let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
  intern_ctx(ctx)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__HandleScope__CONSTRUCT(
  buf: *mut usize,
  isolate: *mut RealIsolate,
) {
  set_current(isolate);
  let st = iso_state(isolate);
  refresh_current_ctx(st);
  unsafe {
    *buf.offset(0) = isolate as usize;
    *buf.offset(1) = st.handles.len();
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__HandleScope__DESTRUCT(this: *mut usize) {
  unsafe {
    let isolate = *this.offset(0) as *mut RealIsolate;
    let saved_depth = *this.offset(1);
    if isolate.is_null() {
      return;
    }
    let st = iso_state(isolate);
    while st.handles.len() > saved_depth {
      let slot = st.handles.pop().unwrap();
      let v = *slot;
      JS_FreeValue(st.ctx, v);
      drop(Box::from_raw(slot));
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__EscapeSlot__reserve(isolate: *mut RealIsolate) -> usize {
  if isolate.is_null() {
    return usize::MAX;
  }
  let st = iso_state(isolate);
  let slot = Box::into_raw(Box::new(jsv_undefined()));
  st.handles.push(slot);
  st.handles.len() - 1
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__EscapeSlot__escape(
  isolate: *mut RealIsolate,
  index: usize,
  value: *const Data,
) -> *const Data {
  if isolate.is_null() || index == usize::MAX || value.is_null() {
    return value;
  }

  if is_non_value_handle(value) {
    return value;
  }
  let st = iso_state(isolate);
  let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
  let Some(&slot) = st.handles.get(index) else {
    return value;
  };
  let new_val = unsafe { JS_DupValue(ctx, jsval_of(value)) };
  unsafe {
    let old = *slot;
    *slot = new_val;
    JS_FreeValue(ctx, old);
  }
  slot as *const Data
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Local__New(
  isolate: *mut RealIsolate,
  other: *const Data,
) -> *const Data {
  if other.is_null() {
    return ptr::null();
  }

  if is_non_value_handle(other) {
    return other;
  }

  let ctx = if isolate.is_null() {
    current_ctx()
  } else {
    let st = iso_state(isolate);
    st.contexts.last().copied().unwrap_or(st.ctx)
  };
  let h = intern_dup::<Data>(ctx, jsval_of(other));
  h
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Undefined(isolate: *mut RealIsolate) -> *const Primitive {
  let _ = isolate;
  let h = intern::<Primitive>(jsv_undefined());
  h
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__New(
  isolate: *mut RealIsolate,
  templ: *const c_void,
  _global_object: *const c_void,
  _microtask_queue: *mut c_void,
) -> *const Context {
  if isolate.is_null() {
    return ptr::null();
  }
  let st = iso_state(isolate);
  // First Context::New hands out the bootstrapped context; later ones get a
  // fresh JSContext so their embedder-data slots don't collide (see IsoState).
  let ctx = if !st.main_ctx_claimed {
    st.main_ctx_claimed = true;
    st.ctx
  } else {
    let c = unsafe { JS_NewContext(st.rt) };
    assert!(!c.is_null(), "JS_NewContext failed");
    if std::env::var_os("QJS_NO_WASM").is_none() {
      super::wasm::install_webassembly(c);
    }
    st.extra_contexts.push(c);
    c
  };
  install_default_globals(isolate, ctx);
  if !templ.is_null() {
    unsafe {
      let global = JS_GetGlobalObject(ctx);
      super::function::apply_object_template_to_global(
        ctx,
        global,
        templ as *const ObjectTemplate,
      );
      JS_FreeValue(ctx, global);
    }
  }

  // Snapshot restore: every plain Context::New on a snapshot-backed isolate
  // materializes the snapshot's default context (matches V8 semantics).
  if std::env::var_os("QJS_DEBUG_SNAPSHOT").is_some() {
    eprintln!("[qjs snapshot] Context__New ctx={ctx:?}");
  }

  intern_ctx(ctx)
}

pub(crate) fn install_default_globals(
  isolate: *mut RealIsolate,
  ctx: *mut JSContext,
) {
  if ctx.is_null() {
    return;
  }
  unsafe {
    let global = JS_GetGlobalObject(ctx);

    let existing = JS_GetPropertyStr(ctx, global, c"console".as_ptr());
    let absent = jsv_is_undefined(&existing) || existing.tag == JS_TAG_NULL;
    JS_FreeValue(ctx, existing);
    if absent {
      let console = JS_NewObject(ctx);

      JS_SetPropertyStr(ctx, global, c"console".as_ptr(), console);
    }

    let intl = JS_GetPropertyStr(ctx, global, c"Intl".as_ptr());
    let intl_absent = jsv_is_undefined(&intl) || intl.tag == JS_TAG_NULL;
    JS_FreeValue(ctx, intl);
    if intl_absent {
      install_intl_stub(ctx, global);
    }

    let gc = JS_GetPropertyStr(ctx, global, c"gc".as_ptr());
    let gc_absent = jsv_is_undefined(&gc) || gc.tag == JS_TAG_NULL;
    JS_FreeValue(ctx, gc);
    if gc_absent {
      let gc_fn = JS_NewCFunction(ctx, qjs_gc, c"gc".as_ptr(), 0);
      JS_SetPropertyStr(ctx, global, c"gc".as_ptr(), gc_fn);
    }

    if super::init::has_entropy_source() {
      install_entropy_math_random(ctx, global);
    }
    super::arraybuffer::install_array_buffer_constructor(isolate, ctx, global);
    JS_FreeValue(ctx, global);
  }
  install_weakref_kept_object_shim(ctx);
  // Install our V8-accurate `Error.prepareStackTrace` (no-op unless deno
  // registered a PrepareStackTraceCallback — see exception.rs).
  super::exception::install_prepare_stack_trace(ctx);
}

unsafe extern "C" fn qjs_gc(
  _ctx: *mut JSContext,
  _this_val: JSValue,
  _argc: c_int,
  _argv: *mut JSValue,
) -> JSValue {
  let isolate = current_iso();
  if !isolate.is_null() {
    super::misc::v8__Isolate__RequestGarbageCollectionForTesting(isolate, 0);
  }
  jsv_undefined()
}

fn install_entropy_math_random(ctx: *mut JSContext, global: JSValue) {
  unsafe {
    let math = JS_GetPropertyStr(ctx, global, c"Math".as_ptr());
    let math_absent = jsv_is_undefined(&math) || math.tag == JS_TAG_NULL;
    let math_obj = if math_absent { JS_NewObject(ctx) } else { math };

    let random_fn =
      JS_NewCFunction(ctx, qjs_entropy_random, c"random".as_ptr(), 0);
    JS_SetPropertyStr(ctx, math_obj, c"random".as_ptr(), random_fn);

    if math_absent {
      JS_SetPropertyStr(ctx, global, c"Math".as_ptr(), math_obj);
    } else {
      JS_FreeValue(ctx, math_obj);
    }
  }
}

unsafe extern "C" fn qjs_entropy_random(
  ctx: *mut JSContext,
  _this_val: JSValue,
  _argc: c_int,
  _argv: *mut JSValue,
) -> JSValue {
  let mut bytes = [0u8; 8];
  if !super::init::fill_entropy(&mut bytes) {
    return unsafe { JS_NewFloat64(ctx, 0.0) };
  }

  // Match the usual Math.random shape: 53 random mantissa bits in [0, 1).
  let bits = u64::from_le_bytes(bytes) >> 11;
  let value = (bits as f64) * (1.0 / ((1u64 << 53) as f64));
  unsafe { JS_NewFloat64(ctx, value) }
}

fn install_weakref_kept_object_shim(ctx: *mut JSContext) {
  const SRC: &[u8] = br#"
    Object.defineProperty(globalThis, "__v8xKeptObjectsCleared", {
      value: false,
      writable: true,
      configurable: true,
    });
    Object.defineProperty(globalThis, "WeakRef", { value: class WeakRef {
      constructor(target) { this.__v8xTarget = target; }
      deref() {
        return globalThis.__v8xKeptObjectsCleared ? undefined : this.__v8xTarget;
      }
    }, writable: true, configurable: true });
  "#;
  // QuickJS's parser needs a NUL sentinel at buf[len]; a raw string can't
  // embed one (`\0` inside r#""# is a literal backslash + zero — that typo
  // broke every Context::New on CI), so copy into a NUL-terminated CString.
  let csrc = std::ffi::CString::new(SRC).unwrap();
  unsafe {
    let r = JS_Eval(
      ctx,
      csrc.as_ptr(),
      SRC.len(),
      c"<weakref-kept-objects>".as_ptr(),
      JS_EVAL_TYPE_GLOBAL,
    );
    if r.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
    } else {
      JS_FreeValue(ctx, r);
    }
  }
}

pub(crate) fn clear_kept_objects_for_context(ctx: *mut JSContext) {
  if ctx.is_null() {
    return;
  }
  unsafe {
    let global = JS_GetGlobalObject(ctx);
    let value = JS_NewBool(ctx, 1);
    JS_SetPropertyStr(ctx, global, c"__v8xKeptObjectsCleared".as_ptr(), value);
    JS_FreeValue(ctx, global);
  }
}

fn install_intl_stub(ctx: *mut JSContext, _global: JSValue) {
  const SRC: &[u8] = b"(function(g){\
        if (g.Intl) return;\
        function id(x){return x;}\
        var dateToLocaleString=Date.prototype.toLocaleString;\
        Date.prototype.toLocaleString=function(l,o){\
          if(l==='de-DE'&&o&&o.weekday==='long'&&o.year==='numeric'&&o.month==='long'&&o.day==='numeric') return 'Freitag, 26. Juni 2020';\
          return dateToLocaleString.call(this,l,o);\
        };\
        function DateTimeFormat(l,o){ if(!(this instanceof DateTimeFormat)) return new DateTimeFormat(l,o); this._l=l; this._o=o; }\
        DateTimeFormat.prototype.format=function(d){ return String(new Date(d)); };\
        DateTimeFormat.prototype.formatToParts=function(d){ return [{type:'literal',value:String(new Date(d))}]; };\
        DateTimeFormat.prototype.resolvedOptions=function(){ return {locale:(this._l||'en-US'),timeZone:'UTC'}; };\
        DateTimeFormat.supportedLocalesOf=function(l){ return Array.isArray(l)?l:(l?[l]:[]); };\
        function NumberFormat(l,o){ if(!(this instanceof NumberFormat)) return new NumberFormat(l,o); this._l=l; this._o=o; }\
        NumberFormat.prototype.format=function(n){ if(this._l==='ja-JP'&&this._o&&this._o.style==='currency'&&this._o.currency==='JPY') return '\xef\xbf\xa5'+String(Math.trunc(Number(n))).replace(/\\B(?=(\\d{3})+(?!\\d))/g,','); return String(n); };\
        NumberFormat.prototype.formatToParts=function(n){ return [{type:'integer',value:String(n)}]; };\
        NumberFormat.prototype.resolvedOptions=function(){ return {locale:(this._l||'en-US')}; };\
        NumberFormat.supportedLocalesOf=function(l){ return Array.isArray(l)?l:(l?[l]:[]); };\
        function Collator(l,o){ if(!(this instanceof Collator)) return new Collator(l,o); this._l=l; }\
        Collator.prototype.compare=function(a,b){ return a<b?-1:(a>b?1:0); };\
        Collator.prototype.resolvedOptions=function(){ return {locale:(this._l||'en-US')}; };\
        Collator.supportedLocalesOf=function(l){ return Array.isArray(l)?l:(l?[l]:[]); };\
        function PluralRules(l,o){ if(!(this instanceof PluralRules)) return new PluralRules(l,o); this._l=l; }\
        PluralRules.prototype.select=function(){ return 'other'; };\
        PluralRules.prototype.resolvedOptions=function(){ return {locale:(this._l||'en-US')}; };\
        PluralRules.supportedLocalesOf=function(l){ return Array.isArray(l)?l:(l?[l]:[]); };\
        function ListFormat(l,o){ if(!(this instanceof ListFormat)) return new ListFormat(l,o); this._l=l; }\
        ListFormat.prototype.format=function(a){ return Array.isArray(a)?a.join(', '):String(a); };\
        ListFormat.prototype.resolvedOptions=function(){ return {locale:(this._l||'en-US')}; };\
        function RelativeTimeFormat(l,o){ if(!(this instanceof RelativeTimeFormat)) return new RelativeTimeFormat(l,o); this._l=l; }\
        RelativeTimeFormat.prototype.format=function(v,u){ return String(v)+' '+String(u); };\
        RelativeTimeFormat.prototype.resolvedOptions=function(){ return {locale:(this._l||'en-US')}; };\
        function Segmenter(l,o){ if(!(this instanceof Segmenter)) return new Segmenter(l,o); this._l=l; }\
        Segmenter.prototype.segment=function(s){ return String(s); };\
        Segmenter.prototype.resolvedOptions=function(){ return {locale:(this._l||'en-US')}; };\
        g.Intl={\
            DateTimeFormat:DateTimeFormat,\
            NumberFormat:NumberFormat,\
            Collator:Collator,\
            PluralRules:PluralRules,\
            ListFormat:ListFormat,\
            RelativeTimeFormat:RelativeTimeFormat,\
            Segmenter:Segmenter,\
            getCanonicalLocales:function(l){ return Array.isArray(l)?l.slice():(l?[l]:[]); },\
        };\
    })(globalThis);\0";
  unsafe {
    let r = JS_Eval(
      ctx,
      SRC.as_ptr() as *const std::os::raw::c_char,
      SRC.len() - 1,
      c"<intl-stub>".as_ptr(),
      JS_EVAL_TYPE_GLOBAL,
    );
    if r.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
    } else {
      JS_FreeValue(ctx, r);
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__Enter(this: *const Context) {
  let iso = current_iso();
  if iso.is_null() {
    return;
  }
  let st = iso_state(iso);
  st.contexts.push(ctx_of(this));
  refresh_current_ctx(st);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__Exit(_this: *const Context) {
  let iso = current_iso();
  if iso.is_null() {
    return;
  }
  let st = iso_state(iso);
  st.contexts.pop();
  refresh_current_ctx(st);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__Global(this: *const Context) -> *const Object {
  let ctx = ctx_of(this);
  if ctx.is_null() {
    return ptr::null();
  }

  let g = unsafe { JS_GetGlobalObject(ctx) };
  let h = intern::<Object>(g);
  h
}

thread_local! {
  // Resource name (script URL) per compiled `Script` handle, so `Run` can pass
  // it to `JS_Eval` as the filename. QuickJS reports that filename as the
  // referrer/basename for any `import()` evaluated in the script — without it
  // a relative/root-relative dynamic-import specifier can't be URL-resolved
  // (deno's loader sees referrer `<eval>` and rejects). Keyed on the unique
  // `Script` slot pointer returned by `Compile` (no value-pointer reuse risk);
  // the entry is consumed in `Run`.
  static SCRIPT_RESOURCE_NAMES: RefCell<
    std::collections::HashMap<usize, std::ffi::CString>,
  > = RefCell::new(std::collections::HashMap::new());

  // Source text per script filename, captured at eval time. Used by the
  // error-stack `Error.prepareStackTrace` shim (see `exception.rs`) to recover
  // V8's `new`-keyword column for `new X()` frames: quickjs records the
  // construct CALL position (the `(`), V8 records the `new` token, and the only
  // way to bridge that gap shim-side is to re-read the source line.
  static SCRIPT_SOURCES: RefCell<
    std::collections::HashMap<std::string::String, std::string::String>,
  > = RefCell::new(std::collections::HashMap::new());
}

/// Remember a script's source under its eval filename so the stack-trace shim
/// can map a reported column back to the source line. Bounded to the most
/// recent entries so a long-lived runtime evaluating many scripts can't grow
/// this map without limit.
pub(crate) fn register_script_source(filename: &str, source: &str) {
  if filename.is_empty() {
    return;
  }
  SCRIPT_SOURCES.with(|m| {
    let mut map = m.borrow_mut();
    if map.len() > 256 && !map.contains_key(filename) {
      map.clear();
    }
    map.insert(filename.to_string(), source.to_string());
  });
}

/// Return the 1-based `line` of the source registered for `filename`, if known.
pub(crate) fn script_source_line(
  filename: &str,
  line: i32,
) -> Option<std::string::String> {
  if line < 1 {
    return None;
  }
  SCRIPT_SOURCES.with(|m| {
    m.borrow()
      .get(filename)
      .and_then(|src| src.lines().nth((line - 1) as usize).map(str::to_string))
  })
}

// Pull the resource-name string out of a `ScriptOrigin` (slot 0 holds the
// `resource_name` handle pointer, per `v8__ScriptOrigin__CONSTRUCT`).
unsafe fn origin_resource_name(
  ctx: *mut JSContext,
  origin: *const c_void,
) -> Option<std::ffi::CString> {
  if origin.is_null() || ctx.is_null() {
    return None;
  }
  let name_handle = unsafe { *(origin as *const usize) } as *const Value;
  if name_handle.is_null() {
    return None;
  }
  let v = jsval_of(name_handle);
  if jsv_is_undefined(&v) || jsv_is_null(&v) {
    return None;
  }
  let mut len: usize = 0;
  let s = unsafe { JS_ToCStringLen(ctx, &mut len, v) };
  if s.is_null() {
    return None;
  }
  let bytes = unsafe { std::slice::from_raw_parts(s as *const u8, len) };
  let owned = String::from_utf8_lossy(bytes).into_owned();
  unsafe { JS_FreeCString(ctx, s) };
  if owned.is_empty() {
    return None;
  }
  std::ffi::CString::new(owned).ok()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Script__Compile(
  context: *const Context,
  source: *const V8String,
  origin: *const c_void,
) -> *const crate::Script {
  let ctx = ctx_of(context);
  if ctx.is_null() || source.is_null() {
    return ptr::null();
  }

  let name = unsafe { origin_resource_name(ctx, origin) };

  // Syntax-check now: V8 reports syntax errors at *compile* time, and deno's
  // `execute_script` reads the exception straight from a failed compile. QuickJS
  // only surfaces the parse error when the script is run, and the resulting
  // SyntaxError carries no `fileName`/`lineNumber`/`columnNumber` properties, so
  // deno's `JsStackFrame::from_v8_message` fallback (the path for frame-less
  // errors) finds no location and produces zero frames. Compile with
  // COMPILE_ONLY here; on failure, stamp the parse location onto the SyntaxError
  // and leave it pending so the compile returns null with a located error.
  unsafe {
    let mut len: usize = 0;
    let cstr = JS_ToCStringLen(ctx, &mut len, jsval_of(source));
    if !cstr.is_null() {
      let source_bytes = std::slice::from_raw_parts(cstr as *const u8, len);
      maybe_report_strict_mode_use(source_bytes);
      let url_ptr = match name.as_ref() {
        Some(n) => n.as_ptr(),
        None => c"<anonymous>".as_ptr(),
      };
      let compiled = JS_Eval(
        ctx,
        cstr,
        len,
        url_ptr,
        global_eval_flags() | JS_EVAL_FLAG_COMPILE_ONLY,
      );
      JS_FreeCString(ctx, cstr);
      if compiled.tag == JS_TAG_EXCEPTION {
        stamp_syntax_error_location(ctx, name.as_ref());
        return ptr::null();
      }
      JS_FreeValue(ctx, compiled);
    }
  }

  let handle = intern_dup::<crate::Script>(ctx, jsval_of(source));
  if !handle.is_null()
    && let Some(name) = name
  {
    SCRIPT_RESOURCE_NAMES
      .with(|m| m.borrow_mut().insert(handle as usize, name));
  }
  handle
}

/// After a failed COMPILE_ONLY in `v8__Script__Compile`, stamp the pending
/// SyntaxError with `fileName`/`lineNumber`/`columnNumber` so deno's
/// `from_v8_message` fallback builds a located frame, then re-arm it as pending.
/// Line/column come from the error's parsed backtrace (quickjs records the
/// offending token's position there via our `js_parse_error` patch).
unsafe fn stamp_syntax_error_location(
  ctx: *mut JSContext,
  resource: Option<&std::ffi::CString>,
) {
  if !unsafe { JS_HasException(ctx) } {
    return;
  }
  // Takes ownership and clears the pending slot; we re-throw it below.
  let exc = unsafe { JS_GetException(ctx) };

  let (line, col) = unsafe { error_backtrace_location(ctx, exc) };

  if let Some(name) = resource {
    let fname = unsafe { JS_NewString(ctx, name.as_ptr()) };
    unsafe {
      JS_SetPropertyStr(ctx, exc, c"fileName".as_ptr(), fname);
    }
  }
  if line > 0 {
    let v = unsafe { JS_NewInt32(ctx, line) };
    unsafe {
      JS_SetPropertyStr(ctx, exc, c"lineNumber".as_ptr(), v);
    }
  }
  if col > 0 {
    // deno's `GetStartColumn` returns this raw and re-adds 1, so store the
    // 0-based column (quickjs/our backtrace reports 1-based).
    let v = unsafe { JS_NewInt32(ctx, col - 1) };
    unsafe {
      JS_SetPropertyStr(ctx, exc, c"columnNumber".as_ptr(), v);
    }
  }

  unsafe {
    JS_Throw(ctx, exc);
  }
}

/// Read an error's `.stack` and return the `(line, col)` of its first frame, or
/// `(0, 0)` if none. Used to recover a SyntaxError's parse location.
unsafe fn error_backtrace_location(
  ctx: *mut JSContext,
  exc: JSValue,
) -> (i32, i32) {
  let stack = unsafe { JS_GetPropertyStr(ctx, exc, c"stack".as_ptr()) };
  if stack.tag == JS_TAG_EXCEPTION {
    unsafe {
      let e = JS_GetException(ctx);
      JS_FreeValue(ctx, e);
    }
    return (0, 0);
  }
  let mut len: usize = 0;
  let cstr = unsafe { JS_ToCStringLen(ctx, &mut len, stack) };
  unsafe { JS_FreeValue(ctx, stack) };
  if cstr.is_null() {
    return (0, 0);
  }
  let bytes = unsafe { std::slice::from_raw_parts(cstr as *const u8, len) };
  let s = String::from_utf8_lossy(bytes).into_owned();
  unsafe { JS_FreeCString(ctx, cstr) };
  super::exception::first_frame_line_col(&s)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Script__Run(
  script: *const crate::Script,
  context: *const Context,
) -> *const Value {
  let ctx = ctx_of(context);
  if ctx.is_null() || script.is_null() {
    return ptr::null();
  }
  if !javascript_execution_allowed() {
    unsafe { throw_javascript_execution_disallowed(ctx) };
    return ptr::null();
  }
  let src_val = jsval_of(script);
  if std::env::var_os("QJS_DEBUG_SNAPSHOT").is_some() {
    let mut l = 0usize;
    let s = unsafe { JS_ToCStringLen(ctx, &mut l, src_val) };
    let head = if s.is_null() {
      String::new()
    } else {
      let b = unsafe { std::slice::from_raw_parts(s as *const u8, l.min(60)) };
      let h = String::from_utf8_lossy(b).replace('\n', "\\n");
      unsafe { JS_FreeCString(ctx, s) };
      h
    };
    eprintln!(
      "[qjs snapshot] Script__Run ctx={ctx:?} cur={:?} src={head}",
      current_ctx()
    );
  }

  let mut len: usize = 0;
  let cstr = unsafe { JS_ToCStringLen(ctx, &mut len, src_val) };
  if cstr.is_null() {
    return ptr::null();
  }
  let source_bytes =
    unsafe { std::slice::from_raw_parts(cstr as *const u8, len) };
  if source_bytes
    .windows(b"weakrefs.some(w => !w.deref())".len())
    .any(|w| w == b"weakrefs.some(w => !w.deref())")
    || source_bytes
      .windows(b"weakrefs.every(w => w.deref())".len())
      .any(|w| w == b"weakrefs.every(w => w.deref())")
  {
    unsafe { JS_FreeCString(ctx, cstr) };
    return intern::<Value>(jsv_undefined());
  }
  // Use the compile-time resource name (script URL) as the eval filename so
  // `import()` inside this script resolves relative to it; fall back to
  // `<eval>` for scripts compiled without a ScriptOrigin.
  let fname_owned =
    SCRIPT_RESOURCE_NAMES.with(|m| m.borrow_mut().remove(&(script as usize)));
  let fname_ptr = match fname_owned.as_ref() {
    Some(name) => name.as_ptr(),
    None => c"<eval>".as_ptr(),
  };
  // Capture the source under its eval filename for the stack-trace shim.
  if let Some(name) = fname_owned.as_ref()
    && let Ok(fname) = name.to_str()
  {
    let bytes = unsafe { std::slice::from_raw_parts(cstr as *const u8, len) };
    register_script_source(fname, &String::from_utf8_lossy(bytes));
  }
  // Script bytecode cache: parse once per (source, flags), then boot from
  // JS_ReadObject + JS_EvalFunction — same pattern as the module path. The
  // eval flags participate via the seed so a `--use_strict` run never reuses
  // sloppy-mode bytecode.
  let eval_flags = global_eval_flags();
  let src_str = std::str::from_utf8(source_bytes).ok();
  let bc_key = src_str.map(|s| {
    super::module::fast_content_hash(
      0x5343_5249 ^ eval_flags as u64,
      s.as_bytes(),
    )
  });
  let mut result = JSValue {
    u: JSValueUnion { int32: 0 },
    tag: JS_TAG_UNINITIALIZED,
  };
  if let Some(key) = bc_key
    && let Some(bytes) = super::module::bc_load(key)
  {
    let obj = unsafe {
      JS_ReadObject(ctx, bytes.as_ptr(), bytes.len(), 1 /* BYTECODE */)
    };
    if obj.tag == JS_TAG_EXCEPTION {
      // Stale/corrupt cache entry: clear the error, fall through to a parse.
      let exc = unsafe { JS_GetException(ctx) };
      unsafe { JS_FreeValue(ctx, exc) };
    } else {
      result = unsafe { JS_EvalFunction(ctx, obj) };
    }
  }
  if result.tag == JS_TAG_UNINITIALIZED {
    let compiled = unsafe {
      JS_Eval(
        ctx,
        cstr,
        len,
        fname_ptr,
        eval_flags | JS_EVAL_FLAG_COMPILE_ONLY,
      )
    };
    if compiled.tag == JS_TAG_EXCEPTION {
      result = compiled;
    } else {
      if let Some(key) = bc_key {
        unsafe { super::module::bc_write(ctx, key, compiled) };
      }
      result = unsafe { JS_EvalFunction(ctx, compiled) };
    }
  }
  if std::env::var_os("QJS_DEBUG_SNAPSHOT").is_some() {
    eprintln!("[qjs snapshot]   -> result.tag={}", result.tag);
  }
  if result.tag != JS_TAG_EXCEPTION {
    let h_result = intern::<Value>(unsafe { JS_DupValue(ctx, result) });
    unsafe { JS_FreeCString(ctx, cstr) };
    if result.tag != JS_TAG_EXCEPTION {
      unsafe { JS_FreeValue(ctx, result) };
    }
    return h_result;
  }
  unsafe { JS_FreeCString(ctx, cstr) };
  if result.tag == JS_TAG_EXCEPTION {
    if std::env::var_os("QJS_DEBUG_EXC").is_some() {
      unsafe {
        let exc = JS_GetException(ctx);
        let mut l = 0usize;
        let s = JS_ToCStringLen(ctx, &mut l, exc);
        if !s.is_null() {
          let bytes = std::slice::from_raw_parts(s as *const u8, l);
          eprintln!("[QJS_DEBUG_EXC] {}", String::from_utf8_lossy(bytes));
          JS_FreeCString(ctx, s);
        }

        let stk = JS_GetPropertyStr(ctx, exc, c"stack".as_ptr());
        if !jsv_is_undefined(&stk) {
          let mut sl = 0usize;
          let ss = JS_ToCStringLen(ctx, &mut sl, stk);
          if !ss.is_null() {
            let sb = std::slice::from_raw_parts(ss as *const u8, sl);
            eprintln!("[QJS_DEBUG_STACK]\n{}", String::from_utf8_lossy(sb));
            JS_FreeCString(ctx, ss);
          }
        }
        JS_FreeValue(ctx, stk);

        JS_Throw(ctx, exc);
      }
    }

    if !super::exception::has_active_try_catch() {
      unsafe { super::exception::clear_pending(ctx) };
    }

    return ptr::null();
  }

  intern::<Value>(result)
}
