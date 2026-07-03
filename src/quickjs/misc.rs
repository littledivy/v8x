//! Family "misc" (QuickJS backend): SnapshotCreator / StartupData / CppHeap /
//! cppgc / WeakCallbackInfo / Proxy / JSON / Wasm* / Task / IdleTask / Global.
//!
//! QuickJS has no equivalent for V8 snapshots, cppgc (Oilpan), the WebAssembly
//! C++ internals or the C++ task abstractions, so most of these are safe inert
//! defaults (see the `TODO(qjs)` markers). The pieces QuickJS *can* back are
//! implemented for real:
//!   * `v8__Global__New` / `v8__Global__Reset` — `JS_DupValue` / `JS_FreeValue`
//!     against a stable context, with a process-wide protect refcount so the
//!     value outlives handle scopes and survives GC.
//!   * `v8__JSON__Parse` — `JS_ParseJSON`.
//!   * `v8__SnapshotCreator__GetIsolate` / `v8__WeakCallbackInfo__GetIsolate`
//!     surface the current isolate.
//!
//! Mirrors the C-ABI shape of the JSC backend (`src/misc.rs`) but routes
//! every JSValue through the QuickJS refcount helpers in `core`.
#![allow(non_snake_case, unused)]

use crate::quickjs::core::{
  adjust_external_memory, ctx_of, current_ctx, current_iso, intern, intern_dup,
  iso_state, jsval_of, release_external_string_memory,
};
use crate::quickjs::quickjs_sys::*;
use crate::{Context, Data, Object, RealIsolate, String as V8String, Value};

use std::os::raw::{c_char, c_void};
use std::ptr;
use std::sync::atomic::Ordering;

struct WeakCallbackInfoShim {
  isolate: *mut RealIsolate,
  parameter: *const c_void,
  second_pass: Option<crate::quickjs::core::WeakCallback>,
}

fn collect_weak_handles(isolate: *mut RealIsolate) {
  if isolate.is_null() {
    return;
  }

  let weak_handles = {
    let st = iso_state(isolate);
    std::mem::take(&mut st.weak_handles)
  };

  for weak in weak_handles {
    let mut info = WeakCallbackInfoShim {
      isolate,
      parameter: weak.parameter,
      second_pass: None,
    };
    unsafe { (weak.callback)(&info as *const _ as *const c_void) };
    if let Some(second_pass) = info.second_pass {
      unsafe { second_pass(&info as *const _ as *const c_void) };
    }
  }
}

fn run_gc_callbacks(
  isolate: *mut RealIsolate,
  callbacks: &[crate::quickjs::core::GcCallbackEntry],
) {
  let raw = crate::isolate::UnsafeRawIsolatePtr::from_real_ptr(isolate);
  for entry in callbacks {
    unsafe {
      (entry.callback)(
        raw,
        entry.gc_type_filter,
        crate::gc::GCCallbackFlags(0),
        entry.data,
      );
    }
  }
}

unsafe extern "C" {

  fn JS_ParseJSON(
    ctx: *mut JSContext,
    buf: *const c_char,
    buf_len: usize,
    filename: *const c_char,
  ) -> JSValue;
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__initialize_process(_platform: *mut c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__shutdown_process() {}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__heap__enable_detached_garbage_collections_for_testing(
  _heap: *mut c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__heap__collect_garbage_for_testing(
  _heap: *mut c_void,
  _stack_state: u8,
) {
}

// cppgc `Member<T>` / `WeakMember<T>` slots. NOTE: the bindgen-derived
// `cppgc__Member_SIZE` is only **4 bytes** (compressed pointer), so we must
// never write a raw 64-bit pointer into a member slot — that overflows the
// inline `[u8; 4]` field and corrupts adjacent memory. We don't run a real
// Oilpan GC, so members are inert: construct/destruct zero the 4-byte slot and
// `Get` returns null (`Set`/`Assign` are no-ops). This keeps `test_cppgc`
// *linking* and non-crashing; the GC-collection assertions in those tests need
// a real cppgc heap and are expected to fail.
#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Member__CONSTRUCT(
  member: *mut c_void,
  _obj: *mut c_void,
) {
  if !member.is_null() {
    unsafe { ptr::write_unaligned(member as *mut u32, 0) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Member__DESTRUCT(member: *mut c_void) {
  if !member.is_null() {
    unsafe { ptr::write_unaligned(member as *mut u32, 0) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Member__Get(_member: *const c_void) -> *mut c_void {
  ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Member__Assign(
  _member: *mut c_void,
  _obj: *mut c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__WeakMember__CONSTRUCT(
  member: *mut c_void,
  _obj: *mut c_void,
) {
  if !member.is_null() {
    unsafe { ptr::write_unaligned(member as *mut u32, 0) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__WeakMember__DESTRUCT(member: *mut c_void) {
  if !member.is_null() {
    unsafe { ptr::write_unaligned(member as *mut u32, 0) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__WeakMember__Get(
  _member: *const c_void,
) -> *mut c_void {
  ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__WeakMember__Assign(
  _member: *mut c_void,
  _obj: *mut c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Visitor__Trace__Member(
  _visitor: *mut c_void,
  _member: *const c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Visitor__Trace__WeakMember(
  _visitor: *mut c_void,
  _member: *const c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__RequestGarbageCollectionForTesting(
  isolate: *mut RealIsolate,
  _type: usize,
) {
  // Best-effort: run the engine GC. Our cppgc shim doesn't reclaim native
  // wrappers, so the cppgc-specific collection assertions still won't hold,
  // but this satisfies the many `test_api` tests that just need a GC to run.
  if isolate.is_null() {
    return;
  }
  let st = iso_state(isolate);
  let prologue_callbacks = st.gc_prologue_callbacks.clone();
  run_gc_callbacks(isolate, &prologue_callbacks);
  if st.kept_objects_cleared && !st.rt.is_null() {
    unsafe { JS_RunGC(st.rt) };
  }
  release_external_string_memory(st);
  let epilogue_callbacks = st.gc_epilogue_callbacks.clone();
  run_gc_callbacks(isolate, &epilogue_callbacks);
  collect_weak_handles(isolate);
}

thread_local! {
    static DUMMY_CPP_HEAP: std::cell::Cell<*mut c_void> =
        const { std::cell::Cell::new(ptr::null_mut()) };
}

fn current_cpp_heap() -> *mut c_void {
  DUMMY_CPP_HEAP.with(|c| {
    let mut h = c.get();
    if h.is_null() {
      h = Box::into_raw(Box::new(0u64)) as *mut c_void;
      c.set(h);
    }
    h
  })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CppHeap__Create(
  _platform: *mut c_void,
  _marking_support: u8,
  _sweeping_support: u8,
) -> *mut c_void {
  current_cpp_heap()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CppHeap__Terminate(_heap: *mut c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CppHeap__DELETE(_heap: *mut c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetCppHeap(
  _isolate: *mut RealIsolate,
) -> *mut c_void {
  current_cpp_heap()
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__make_garbage_collectable(
  _heap: *mut c_void,
  additional_bytes: usize,
  align: usize,
) -> *mut c_void {
  const RUST_OBJ_SIZE: usize = 8;
  let size = RUST_OBJ_SIZE + additional_bytes;
  let align = align.max(8);
  let Ok(layout) = std::alloc::Layout::from_size_align(size, align) else {
    return ptr::null_mut();
  };
  unsafe { std::alloc::alloc_zeroed(layout) as *mut c_void }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Persistent__CONSTRUCT(
  persistent: *mut *mut c_void,
  obj: *mut c_void,
) {
  if !persistent.is_null() {
    unsafe { *persistent = obj };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Persistent__DESTRUCT(persistent: *mut *mut c_void) {
  if !persistent.is_null() {
    unsafe { *persistent = ptr::null_mut() };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Persistent__Get(
  persistent: *const *mut c_void,
) -> *mut c_void {
  if persistent.is_null() {
    return ptr::null_mut();
  }
  unsafe { *persistent }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Visitor__Trace__TracedReference(
  _visitor: *mut c_void,
  _ref_: *const c_void,
) {
}

fn stable_ctx() -> *mut JSContext {
  let ctx = current_ctx();
  if !ctx.is_null() {
    return ctx;
  }
  let iso = current_iso();
  if iso.is_null() {
    return ptr::null_mut();
  }
  let st = iso_state(iso);
  st.contexts.last().copied().unwrap_or(st.ctx)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Global__New(
  _isolate: *mut RealIsolate,
  data: *const Data,
) -> *const Data {
  if data.is_null() {
    return ptr::null();
  }

  if super::core::is_non_value_handle(data) {
    return data;
  }
  let ctx = stable_ctx();
  if ctx.is_null() {
    return ptr::null();
  }

  let v = jsval_of(data);
  let dup = unsafe { JS_DupValue(ctx, v) };
  let cell = Box::into_raw(Box::new(dup));
  cell as *const Data
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Global__NewWeak(
  isolate: *mut RealIsolate,
  data: *const Data,
  parameter: *const c_void,
  callback: unsafe extern "C" fn(*const c_void),
) -> *const Data {
  let handle = v8__Global__New(isolate, data);
  if !handle.is_null() && !isolate.is_null() {
    iso_state(isolate)
      .weak_handles
      .push(crate::quickjs::core::WeakHandle {
        handle,
        parameter,
        callback,
      });
  }
  handle
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Global__Reset(data: *const Data) {
  if data.is_null() {
    return;
  }

  if super::core::is_non_value_handle(data) {
    return;
  }

  let iso = current_iso();
  if !iso.is_null() {
    iso_state(iso)
      .weak_handles
      .retain(|weak| weak.handle != data);
  }

  let ctx = stable_ctx();
  unsafe {
    let cell = data as *mut JSValue;
    let v = *cell;
    if !ctx.is_null() {
      JS_FreeValue(ctx, v);
    }
    drop(Box::from_raw(cell));
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TracedReference__CONSTRUCT(this: *mut usize) {
  if !this.is_null() {
    unsafe { this.write_unaligned(0) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TracedReference__DESTRUCT(this: *mut usize) {
  if this.is_null() {
    return;
  }
  unsafe {
    let cell = this.read_unaligned() as *mut JSValue;
    if !cell.is_null() {
      let v = *cell;
      let ctx = stable_ctx();
      if !ctx.is_null() {
        JS_FreeValue(ctx, v);
      }
      drop(Box::from_raw(cell));
      this.write_unaligned(0);
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TracedReference__Reset(
  this: *mut usize,
  _isolate: *mut RealIsolate,
  data: *const Data,
) {
  if this.is_null() {
    return;
  }
  let ctx = stable_ctx();
  unsafe {
    let old = this.read_unaligned() as *mut JSValue;
    if !old.is_null() {
      if !ctx.is_null() {
        JS_FreeValue(ctx, *old);
      }
      drop(Box::from_raw(old));
      this.write_unaligned(0);
    }
    if data.is_null() || ctx.is_null() {
      return;
    }

    if super::core::is_non_value_handle(data) {
      return;
    }
    let dup = JS_DupValue(ctx, jsval_of(data));
    if std::env::var_os("QJS_DEBUG_TR").is_some() {
      eprintln!(
        "[QJS TracedRef::Reset] store tag={} ptr={:?}",
        dup.tag, dup.u.ptr
      );
    }
    let cell = Box::into_raw(Box::new(dup));
    this.write_unaligned(cell as usize);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TracedReference__Get(
  this: *const usize,
  _isolate: *mut RealIsolate,
) -> *const Data {
  if this.is_null() {
    return ptr::null();
  }
  let slot = unsafe { this.read_unaligned() };
  if slot == 0 {
    if std::env::var_os("QJS_DEBUG_TR").is_some() {
      eprintln!("[QJS TracedRef::Get] EMPTY");
    }
    return ptr::null();
  }
  if std::env::var_os("QJS_DEBUG_TR").is_some() {
    let v = unsafe { *(slot as *const JSValue) };
    eprintln!("[QJS TracedRef::Get] tag={} ptr={:?}", v.tag, unsafe {
      v.u.ptr
    });
  }

  slot as *const Data
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__JSON__Parse(
  context: *const Context,
  json_string: *const V8String,
) -> *const Value {
  if context.is_null() || json_string.is_null() {
    return ptr::null();
  }
  let ctx = ctx_of(context);
  unsafe {
    let mut len: usize = 0;
    let cstr = JS_ToCStringLen(ctx, &mut len, jsval_of(json_string));
    if cstr.is_null() {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      return ptr::null();
    }
    let fname = c"<json>";
    let parsed = JS_ParseJSON(ctx, cstr, len, fname.as_ptr());
    JS_FreeCString(ctx, cstr);
    if parsed.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      return ptr::null();
    }

    intern::<Value>(parsed)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Proxy__GetHandler(this: *const c_void) -> *const Value {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  let v = unsafe { JS_GetProxyHandler(ctx, jsval_of(this as *const Value)) };
  if v.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  intern::<Value>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Proxy__GetTarget(this: *const c_void) -> *const Value {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  let v = unsafe { JS_GetProxyTarget(ctx, jsval_of(this as *const Value)) };
  if v.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  intern::<Value>(v)
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct RawStartupDataAbi {
  data: *const c_char,
  raw_size: std::os::raw::c_int,
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__CONSTRUCT(
  buf: *mut c_void,
  params: *const c_void,
) {
  // Params may carry an existing snapshot blob (creator-from-existing):
  // Isolate__New parses it, and each new context replays + re-records it.
  let iso = crate::quickjs::core::v8__Isolate__New(params);
  crate::quickjs::core::v8__Isolate__Enter(iso);
  crate::quickjs::core::iso_state(iso).snap =
    Some(Box::new(super::snapshot::SnapState::default()));
  if !buf.is_null() {
    unsafe { *(buf as *mut *mut RealIsolate) = iso };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__DESTRUCT(_this: *mut c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__GetIsolate(
  this: *const c_void,
) -> *mut c_void {
  if !this.is_null() {
    let iso = unsafe { *(this as *const *mut RealIsolate) };
    if !iso.is_null() {
      return iso as *mut c_void;
    }
  }
  current_iso() as *mut c_void
}

fn creator_iso(this: *const c_void) -> *mut RealIsolate {
  if !this.is_null() {
    let iso = unsafe { *(this as *const *mut RealIsolate) };
    if !iso.is_null() {
      return iso;
    }
  }
  current_iso()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__CreateBlob(
  this: *mut c_void,
  _function_code_handling: u32,
) -> RawStartupDataAbi {
  let iso = creator_iso(this);
  if !iso.is_null() {
    if let Some(snap) = crate::quickjs::core::iso_state(iso).snap.as_deref() {
      let bytes = super::snapshot::create_blob(snap);
      let (data, len) = super::snapshot::leak_blob(bytes);
      return RawStartupDataAbi {
        data: data as *const c_char,
        raw_size: len as std::os::raw::c_int,
      };
    }
  }
  RawStartupDataAbi {
    data: ptr::null(),
    raw_size: 0,
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__SetDefaultContext(
  this: *mut c_void,
  context: *const Context,
) {
  let iso = creator_iso(this);
  if iso.is_null() {
    return;
  }
  if let Some(snap) = crate::quickjs::core::iso_state(iso).snap.as_deref_mut() {
    snap.default_ctx = ctx_of(context) as usize;
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__AddContext(
  this: *mut c_void,
  context: *const Context,
) -> usize {
  let iso = creator_iso(this);
  if iso.is_null() {
    return 0;
  }
  if let Some(snap) = crate::quickjs::core::iso_state(iso).snap.as_deref_mut() {
    snap.added.push(ctx_of(context) as usize);
    return snap.added.len() - 1;
  }
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__AddData_to_isolate(
  this: *mut c_void,
  data: *const Data,
) -> usize {
  let iso = creator_iso(this);
  if iso.is_null() || data.is_null() {
    return 0;
  }
  let ctx = current_ctx();
  let bytes =
    super::snapshot::serialize_value(ctx, jsval_of(data)).unwrap_or_default();
  if let Some(snap) = crate::quickjs::core::iso_state(iso).snap.as_deref_mut() {
    snap.iso_data.push(bytes);
    return snap.iso_data.len() - 1;
  }
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__AddData_to_context(
  this: *mut c_void,
  context: *const Context,
  data: *const Data,
) -> usize {
  let iso = creator_iso(this);
  if iso.is_null() || data.is_null() {
    return 0;
  }
  let ctx = ctx_of(context);
  let bytes =
    super::snapshot::serialize_value(ctx, jsval_of(data)).unwrap_or_default();
  if let Some(snap) = crate::quickjs::core::iso_state(iso).snap.as_deref_mut() {
    let v = snap.ctx_data.entry(ctx as usize).or_default();
    v.push(bytes);
    return v.len() - 1;
  }
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StartupData__CanBeRehashed(_this: *const c_void) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StartupData__IsValid(this: *const c_void) -> bool {
  if this.is_null() {
    return false;
  }
  let raw = unsafe { &*(this as *const RawStartupDataAbi) };
  if raw.data.is_null() || raw.raw_size < 8 {
    return false;
  }
  let head = unsafe { std::slice::from_raw_parts(raw.data as *const u8, 8) };
  head == super::snapshot::SNAP_MAGIC
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StartupData__data__DELETE(this: *const c_char) {
  super::snapshot::free_blob(this as *const u8);
}

// ICU common-data loader (vendored rusty_v8 `icu::set_common_data_77`). QuickJS
// brings its own ICU/Intl, so we never actually *load* V8's blob — but we do
// validate its header exactly like ICU's `udata_setCommonData`: a real ICU data
// file (icudtl.dat) begins with `headerSize:u16` followed by the format magic
// bytes 0xDA 0x27. The test harness ships a blob with a valid header, so setup's
// `set_common_data_77(<icudtl.dat>)` returns Ok; a garbage blob — e.g. the
// `[1, 2, 3, 0, …]` from `icu_set_common_data_fail` — has the wrong magic and
// must return `U_INVALID_FORMAT_ERROR`. No length crosses this C ABI, so we read
// only the 4 header bytes every real caller is guaranteed to provide.
#[unsafe(no_mangle)]
pub extern "C" fn udata_setCommonData_77(
  data: *const u8,
  error_code: *mut i32,
) {
  // ICU's UErrorCode for a bad/unrecognized data header.
  const U_INVALID_FORMAT_ERROR: i32 = 3;
  let valid = !data.is_null()
    && unsafe {
      // ICU DataHeader: bytes [2] and [3] are the magic 0xDA 0x27.
      *data.add(2) == 0xDA && *data.add(3) == 0x27
    };
  if !error_code.is_null() {
    unsafe {
      *error_code = if valid { 0 } else { U_INVALID_FORMAT_ERROR };
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Task__Run(_task: *mut c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Task__DELETE(_task: *mut c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__IdleTask__Run(
  _task: *mut c_void,
  _deadline_in_seconds: f64,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__IdleTask__DELETE(_task: *mut c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WeakCallbackInfo__GetIsolate(
  this: *const c_void,
) -> *mut c_void {
  if this.is_null() {
    return current_iso() as *mut c_void;
  }
  unsafe { (*(this as *const WeakCallbackInfoShim)).isolate as *mut c_void }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WeakCallbackInfo__GetParameter(
  this: *const c_void,
) -> *mut c_void {
  if this.is_null() {
    return ptr::null_mut();
  }
  unsafe { (*(this as *const WeakCallbackInfoShim)).parameter as *mut c_void }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WeakCallbackInfo__SetSecondPassCallback(
  this: *const c_void,
  callback: unsafe extern "C" fn(*const c_void),
) {
  if this.is_null() {
    return;
  }
  unsafe {
    (*(this as *mut WeakCallbackInfoShim)).second_pass = Some(callback);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__Unpack(
  _isolate: *mut c_void,
  value: *const Value,
  that: *mut c_void,
) {
  super::wasm::streaming_unpack(value, that);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__shared_ptr_DESTRUCT(this: *mut c_void) {
  super::wasm::streaming_shared_ptr_destruct(this);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__OnBytesReceived(
  this: *mut c_void,
  data: *const u8,
  len: usize,
) {
  super::wasm::streaming_on_bytes_received(this, data, len);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__Finish(
  this: *mut c_void,
  callback: Option<unsafe extern "C" fn(*mut c_void)>,
) {
  super::wasm::streaming_finish(this, callback);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__Abort(
  this: *mut c_void,
  exception: *const Value,
) {
  super::wasm::streaming_abort(this, exception);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__SetUrl(
  this: *mut c_void,
  url: *const c_char,
  len: usize,
) {
  super::wasm::streaming_set_url(this, url, len);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleObject__FromCompiledModule(
  isolate: *mut c_void,
  compiled_module: *const c_void,
) -> *const c_void {
  super::wasm::module_object_from_compiled_module(isolate, compiled_module)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleObject__GetCompiledModule(
  this: *const c_void,
) -> *mut c_void {
  super::wasm::module_object_get_compiled_module(this)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleObject__Compile(
  isolate: *mut RealIsolate,
  wire_bytes_data: *const u8,
  length: usize,
) -> *const Object {
  if isolate.is_null() || wire_bytes_data.is_null() {
    return ptr::null();
  }
  let st = iso_state(isolate);
  let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
  if ctx.is_null() {
    return ptr::null();
  }
  let bytes = unsafe { std::slice::from_raw_parts(wire_bytes_data, length) };
  let v = unsafe { super::wasm::compile_module_object(ctx, bytes) };
  if v.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  intern::<Object>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CompiledWasmModule__DELETE(this: *mut c_void) {
  super::wasm::compiled_module_delete(this);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__NEW() -> *mut c_void {
  super::wasm::module_compilation_new()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__DELETE(this: *mut c_void) {
  super::wasm::module_compilation_delete(this);
}

unsafe extern "C" {
  fn JS_JSONStringify(
    ctx: *mut JSContext,
    obj: JSValue,
    replacer: JSValue,
    space0: JSValue,
  ) -> JSValue;
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__JSON__Stringify(
  context: *const Context,
  json_object: *const Value,
) -> *const V8String {
  let ctx = ctx_of(context);
  if ctx.is_null() || json_object.is_null() {
    return ptr::null();
  }
  unsafe {
    let s = JS_JSONStringify(
      ctx,
      jsval_of(json_object),
      jsv_undefined(),
      jsv_undefined(),
    );
    if s.tag == JS_TAG_EXCEPTION {
      // Leave the exception PENDING (v8's JSON::Stringify returns Empty and
      // keeps it set) so the caller's TryCatch can read it — e.g. console's
      // `%j` distinguishes circular-reference TypeErrors from real ones.
      // Clearing it here made TryCatch::Exception() return undefined.
      return ptr::null();
    }
    intern::<V8String>(s)
  }
}

unsafe fn call_unary_closure(
  ctx: *mut JSContext,
  src: &[u8],
  obj: JSValue,
) -> JSValue {
  unsafe {
    let f = JS_Eval(
      ctx,
      src.as_ptr() as *const c_char,
      src.len() - 1,
      c"<map-set>".as_ptr(),
      JS_EVAL_TYPE_GLOBAL,
    );
    if f.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      return jsv_exception();
    }
    let mut args = [JS_DupValue(ctx, obj)];
    let r = JS_Call(ctx, f, jsv_undefined(), 1, args.as_mut_ptr());
    JS_FreeValue(ctx, f);
    JS_FreeValue(ctx, args[0]);
    r
  }
}

fn map_set_size(v: *const Object) -> usize {
  let ctx = current_ctx();
  if ctx.is_null() || v.is_null() {
    return 0;
  }
  unsafe {
    let sz = JS_GetPropertyStr(ctx, jsval_of(v), c"size".as_ptr());
    if sz.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      return 0;
    }
    let mut n: i64 = 0;
    let rc = JS_ToInt64(ctx, &mut n, sz);
    JS_FreeValue(ctx, sz);
    if rc < 0 {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      return 0;
    }
    n.max(0) as usize
  }
}

fn mb(b: bool) -> crate::support::MaybeBool {
  if b {
    crate::support::MaybeBool::JustTrue
  } else {
    crate::support::MaybeBool::JustFalse
  }
}

// Invoke `this.<method>(args...)` and return the raw result JSValue (the caller
// must `JS_FreeValue` it) or `jsv_exception()` when the method is missing or
// threw (the pending exception is cleared). Used to back the Map/Set C-ABI on
// top of QuickJS's native Map/Set prototype methods.
unsafe fn call_collection_method(
  ctx: *mut JSContext,
  this: JSValue,
  method: *const c_char,
  args: &mut [JSValue],
) -> JSValue {
  unsafe {
    let f = JS_GetPropertyStr(ctx, this, method);
    if !JS_IsFunction(ctx, f) {
      JS_FreeValue(ctx, f);
      return jsv_exception();
    }
    let r = JS_Call(
      ctx,
      f,
      this,
      args.len() as std::os::raw::c_int,
      args.as_mut_ptr(),
    );
    JS_FreeValue(ctx, f);
    if r.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
    }
    r
  }
}

// `new <ctor_name>()` against the isolate's active context (Map / Set).
fn new_builtin(isolate: *mut RealIsolate, ctor_name: *const c_char) -> JSValue {
  if isolate.is_null() {
    return jsv_exception();
  }
  let st = iso_state(isolate);
  let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
  if ctx.is_null() {
    return jsv_exception();
  }
  unsafe {
    let global = JS_GetGlobalObject(ctx);
    let ctor = JS_GetPropertyStr(ctx, global, ctor_name);
    JS_FreeValue(ctx, global);
    if !JS_IsConstructor(ctx, ctor) {
      JS_FreeValue(ctx, ctor);
      return jsv_exception();
    }
    let v = JS_CallConstructor(ctx, ctor, 0, ptr::null_mut());
    JS_FreeValue(ctx, ctor);
    if v.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      return jsv_exception();
    }
    v
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Size(map: *const crate::Map) -> usize {
  map_set_size(map as *const Object)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__Size(set: *const crate::Set) -> usize {
  map_set_size(set as *const Object)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__As__Array(
  this: *const crate::Map,
) -> *const crate::Array {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }

  const SRC: &[u8] =
        b"(function(m){var r=[];m.forEach(function(v,k){r.push(k);r.push(v);});return r;})\0";
  let arr = unsafe { call_unary_closure(ctx, SRC, jsval_of(this)) };
  if arr.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  intern::<crate::Array>(arr)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__As__Array(
  this: *const crate::Set,
) -> *const crate::Array {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  const SRC: &[u8] =
    b"(function(s){var r=[];s.forEach(function(v){r.push(v);});return r;})\0";
  let arr = unsafe { call_unary_closure(ctx, SRC, jsval_of(this)) };
  if arr.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(ctx) };
    unsafe { JS_FreeValue(ctx, exc) };
    return ptr::null();
  }
  intern::<crate::Array>(arr)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__New(isolate: *mut RealIsolate) -> *const crate::Set {
  if isolate.is_null() {
    return ptr::null();
  }
  let st = iso_state(isolate);
  let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
  if ctx.is_null() {
    return ptr::null();
  }
  unsafe {
    let global = JS_GetGlobalObject(ctx);
    let ctor = JS_GetPropertyStr(ctx, global, c"Set".as_ptr());
    JS_FreeValue(ctx, global);
    if !JS_IsConstructor(ctx, ctor) {
      JS_FreeValue(ctx, ctor);
      return ptr::null();
    }
    let v = JS_CallConstructor(ctx, ctor, 0, ptr::null_mut());
    JS_FreeValue(ctx, ctor);
    if v.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      return ptr::null();
    }
    intern::<crate::Set>(v)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__Add(
  this: *const crate::Set,
  context: *const Context,
  key: *const Value,
) -> *const crate::Set {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  unsafe {
    let add = JS_GetPropertyStr(ctx, jsval_of(this), c"add".as_ptr());
    if !JS_IsFunction(ctx, add) {
      JS_FreeValue(ctx, add);
      return ptr::null();
    }
    let mut args = [JS_DupValue(ctx, jsval_of(key))];
    let r = JS_Call(ctx, add, jsval_of(this), 1, args.as_mut_ptr());
    JS_FreeValue(ctx, add);
    JS_FreeValue(ctx, args[0]);
    if r.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      return ptr::null();
    }
    JS_FreeValue(ctx, r);

    intern::<crate::Set>(JS_DupValue(ctx, jsval_of(this)))
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Date__New(
  context: *const Context,
  value: f64,
) -> *const crate::Date {
  let ctx = ctx_of(context);
  if ctx.is_null() {
    return ptr::null();
  }
  unsafe {
    let global = JS_GetGlobalObject(ctx);
    let ctor = JS_GetPropertyStr(ctx, global, c"Date".as_ptr());
    JS_FreeValue(ctx, global);
    if !JS_IsConstructor(ctx, ctor) {
      JS_FreeValue(ctx, ctor);
      return ptr::null();
    }
    let mut args = [JS_NewFloat64(ctx, value)];
    let v = JS_CallConstructor(ctx, ctor, 1, args.as_mut_ptr());
    JS_FreeValue(ctx, ctor);
    if v.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      return ptr::null();
    }
    intern::<crate::Date>(v)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Date__ValueOf(this: *const crate::Date) -> f64 {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return 0.0;
  }
  unsafe {
    let vo = JS_GetPropertyStr(ctx, jsval_of(this), c"valueOf".as_ptr());
    if !JS_IsFunction(ctx, vo) {
      JS_FreeValue(ctx, vo);
      return 0.0;
    }
    let r = JS_Call(ctx, vo, jsval_of(this), 0, ptr::null_mut());
    JS_FreeValue(ctx, vo);
    if r.tag == JS_TAG_EXCEPTION {
      let exc = JS_GetException(ctx);
      JS_FreeValue(ctx, exc);
      return 0.0;
    }
    let mut n: f64 = 0.0;
    JS_ToFloat64(ctx, &mut n, r);
    JS_FreeValue(ctx, r);
    n
  }
}

use std::collections::HashMap;

thread_local! {
    static EMBEDDER_DATA: std::cell::RefCell<HashMap<usize, Vec<JSValue>>> =
        std::cell::RefCell::new(HashMap::new());
    static SECURITY_TOKEN: std::cell::RefCell<HashMap<usize, JSValue>> =
        std::cell::RefCell::new(HashMap::new());
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__SetEmbedderData(
  this: *const Context,
  index: std::os::raw::c_int,
  value: *const Value,
) {
  let ctx = ctx_of(this);
  if ctx.is_null() || index < 0 {
    return;
  }
  let owned = unsafe { JS_DupValue(ctx, jsval_of(value)) };
  EMBEDDER_DATA.with(|m| {
    let mut map = m.borrow_mut();
    let slots = map.entry(ctx as usize).or_default();
    let idx = index as usize;
    while slots.len() <= idx {
      slots.push(jsv_undefined());
    }
    let old = slots[idx];
    if old.tag < 0 {
      unsafe { JS_FreeValue(ctx, old) };
    }
    slots[idx] = owned;
  });
}

/// Snapshot support: borrow every embedder-data value slot of `ctx`.
/// `None` marks unset (undefined) slots.
pub(crate) fn embedder_data_snapshot(
  ctx: *mut JSContext,
) -> Vec<Option<JSValue>> {
  EMBEDDER_DATA.with(|m| {
    m.borrow()
      .get(&(ctx as usize))
      .map(|slots| {
        slots
          .iter()
          .map(|v| if jsv_is_undefined(v) { None } else { Some(*v) })
          .collect()
      })
      .unwrap_or_default()
  })
}

/// Snapshot support: install a replayed value into an embedder-data slot.
/// Dups `value` (caller keeps its refcount).
pub(crate) fn set_embedder_data_raw(
  ctx: *mut JSContext,
  index: usize,
  value: JSValue,
) {
  let owned = unsafe { JS_DupValue(ctx, value) };
  EMBEDDER_DATA.with(|m| {
    let mut map = m.borrow_mut();
    let slots = map.entry(ctx as usize).or_default();
    while slots.len() <= index {
      slots.push(jsv_undefined());
    }
    let old = slots[index];
    if old.tag < 0 {
      unsafe { JS_FreeValue(ctx, old) };
    }
    slots[index] = owned;
  });
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetEmbedderData(
  this: *const Context,
  index: std::os::raw::c_int,
) -> *const Value {
  let ctx = ctx_of(this);
  if ctx.is_null() || index < 0 {
    return ptr::null();
  }
  let found = EMBEDDER_DATA.with(|m| {
    m.borrow()
      .get(&(ctx as usize))
      .and_then(|slots| slots.get(index as usize).copied())
  });
  match found {
    Some(v) => intern_dup::<Value>(ctx, v),

    None => intern::<Value>(jsv_undefined()),
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__SetSecurityToken(
  this: *const Context,
  value: *const Value,
) {
  let ctx = ctx_of(this);
  if ctx.is_null() {
    return;
  }
  let owned = unsafe { JS_DupValue(ctx, jsval_of(value)) };
  SECURITY_TOKEN.with(|m| {
    if let Some(old) = m.borrow_mut().insert(ctx as usize, owned) {
      if old.tag < 0 {
        unsafe { JS_FreeValue(ctx, old) };
      }
    }
  });
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetSecurityToken(
  this: *const Context,
) -> *const Value {
  let ctx = ctx_of(this);
  if ctx.is_null() {
    return ptr::null();
  }
  let found = SECURITY_TOKEN.with(|m| m.borrow().get(&(ctx as usize)).copied());
  match found {
    Some(v) => intern_dup::<Value>(ctx, v),
    None => intern::<Value>(jsv_undefined()),
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__CONSTRUCT(this: *mut *const Data) {
  if !this.is_null() {
    unsafe { this.write_unaligned(ptr::null()) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__DESTRUCT(_this: *mut *const Data) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__Get(
  this: *const *const Data,
  _isolate: *mut RealIsolate,
) -> *const Data {
  if this.is_null() {
    return ptr::null();
  }
  let stored = unsafe { this.read_unaligned() };
  if stored.is_null() {
    return ptr::null();
  }

  let ctx = current_ctx();
  if ctx.is_null() {
    return stored;
  }
  intern_dup::<Data>(ctx, jsval_of(stored))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__Set(
  this: *mut *const Data,
  _isolate: *mut RealIsolate,
  data: *mut Data,
) {
  if this.is_null() {
    return;
  }
  let ctx = current_ctx();
  if ctx.is_null() || data.is_null() {
    unsafe { this.write_unaligned(ptr::null()) };
    return;
  }

  let owned = unsafe { JS_DupValue(ctx, jsval_of(data)) };
  let boxed = Box::into_raw(Box::new(owned));
  unsafe { this.write_unaligned(boxed as *const Data) };
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__AddGCPrologueCallback(
  isolate: *mut RealIsolate,
  callback: crate::isolate::GcCallbackWithData,
  data: *mut c_void,
  gc_type_filter: crate::gc::GCType,
) {
  if !isolate.is_null() {
    iso_state(isolate).gc_prologue_callbacks.push(
      crate::quickjs::core::GcCallbackEntry {
        callback,
        data,
        gc_type_filter,
      },
    );
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__AddGCEpilogueCallback(
  isolate: *mut RealIsolate,
  callback: crate::isolate::GcCallbackWithData,
  data: *mut c_void,
  gc_type_filter: crate::gc::GCType,
) {
  if !isolate.is_null() {
    iso_state(isolate).gc_epilogue_callbacks.push(
      crate::quickjs::core::GcCallbackEntry {
        callback,
        data,
        gc_type_filter,
      },
    );
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__AddNearHeapLimitCallback(
  _isolate: *mut RealIsolate,
  _callback: crate::isolate::NearHeapLimitCallback,
  _data: *mut c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__AdjustAmountOfExternalAllocatedMemory(
  isolate: *mut RealIsolate,
  change_in_bytes: i64,
) -> i64 {
  let isolate = if isolate.is_null() {
    current_iso()
  } else {
    isolate
  };
  if isolate.is_null() {
    return change_in_bytes;
  }
  adjust_external_memory(iso_state(isolate), change_in_bytes)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__LowMemoryNotification(
  isolate: *mut RealIsolate,
) {
  if isolate.is_null() {
    return;
  }
  let st = iso_state(isolate);
  if !st.rt.is_null() {
    unsafe { JS_RunGC(st.rt) };
  }
  release_external_string_memory(st);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__NumberOfHeapSpaces(
  _isolate: *mut RealIsolate,
) -> usize {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetHeapSpaceStatistics(
  _isolate: *mut RealIsolate,
  space_statistics: *mut crate::binding::v8__HeapSpaceStatistics,
  _index: usize,
) -> bool {
  if !space_statistics.is_null() {
    unsafe {
      ptr::write_bytes(
        space_statistics as *mut u8,
        0,
        std::mem::size_of::<crate::binding::v8__HeapSpaceStatistics>(),
      );
    }
  }
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetHeapCodeAndMetadataStatistics(
  _isolate: *mut RealIsolate,
  code_statistics: *mut crate::binding::v8__HeapCodeStatistics,
) -> bool {
  if !code_statistics.is_null() {
    unsafe {
      ptr::write_bytes(
        code_statistics as *mut u8,
        0,
        std::mem::size_of::<crate::binding::v8__HeapCodeStatistics>(),
      );
    }
  }
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__DateTimeConfigurationChangeNotification(
  _isolate: *mut RealIsolate,
  _time_zone_detection: crate::isolate::TimeZoneDetection,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetAllowWasmCodeGenerationCallback(
  _isolate: *mut RealIsolate,
  _callback: crate::isolate::AllowWasmCodeGenerationCallback,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__HeapProfiler__TakeHeapSnapshot(
  _isolate: *mut RealIsolate,
  _callback: unsafe extern "C" fn(*mut c_void, *const u8, usize) -> bool,
  _arg: *mut c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CachedDataVersionTag() -> u32 {
  0x5145_4a53
}

// ---------------------------------------------------------------------------
// Link-stubs for v8 C-ABI symbols that `test_api.rs` references but this
// backend doesn't implement yet. Each returns a benign default
// (null / 0 / false / `Nothing`) so the target LINKS and the many tests that
// don't touch these paths run; tests that do exercise them fail gracefully
// without crashing. Promote individual stubs to real implementations over time.
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn icu_set_default_locale(locale: *const c_char) {
  if locale.is_null() {
    return;
  }
  let s = unsafe { std::ffi::CStr::from_ptr(locale) }.to_string_lossy();
  crate::quickjs::cli_extra::set_default_locale_str(&s);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CompiledWasmModule__GetWireBytesRef(
  this: *mut std::os::raw::c_void,
  length: *mut std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  super::wasm::compiled_module_wire_bytes(this, length)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__CompiledWasmModule__SourceUrl(
  this: *mut std::os::raw::c_void,
  length: *mut std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  super::wasm::compiled_module_source_url(this, length)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__Clear(_this: *mut std::os::raw::c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Eternal__IsEmpty(
  _this: *const std::os::raw::c_void,
) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Clear(this: *const crate::Map) {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return;
  }
  unsafe {
    let r =
      call_collection_method(ctx, jsval_of(this), c"clear".as_ptr(), &mut []);
    JS_FreeValue(ctx, r);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Delete(
  this: *const crate::Map,
  context: *const Context,
  key: *const Value,
) -> crate::support::MaybeBool {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return crate::support::MaybeBool::Nothing;
  }
  unsafe {
    let mut args = [JS_DupValue(ctx, jsval_of(key))];
    let r = call_collection_method(
      ctx,
      jsval_of(this),
      c"delete".as_ptr(),
      &mut args,
    );
    JS_FreeValue(ctx, args[0]);
    if r.tag == JS_TAG_EXCEPTION {
      return crate::support::MaybeBool::Nothing;
    }
    let b = JS_ToBool(ctx, r) != 0;
    JS_FreeValue(ctx, r);
    mb(b)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Get(
  this: *const crate::Map,
  context: *const Context,
  key: *const Value,
) -> *const Value {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  unsafe {
    let mut args = [JS_DupValue(ctx, jsval_of(key))];
    let r =
      call_collection_method(ctx, jsval_of(this), c"get".as_ptr(), &mut args);
    JS_FreeValue(ctx, args[0]);
    if r.tag == JS_TAG_EXCEPTION {
      return ptr::null();
    }
    intern::<Value>(r)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Has(
  this: *const crate::Map,
  context: *const Context,
  key: *const Value,
) -> crate::support::MaybeBool {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return crate::support::MaybeBool::Nothing;
  }
  unsafe {
    let mut args = [JS_DupValue(ctx, jsval_of(key))];
    let r =
      call_collection_method(ctx, jsval_of(this), c"has".as_ptr(), &mut args);
    JS_FreeValue(ctx, args[0]);
    if r.tag == JS_TAG_EXCEPTION {
      return crate::support::MaybeBool::Nothing;
    }
    let b = JS_ToBool(ctx, r) != 0;
    JS_FreeValue(ctx, r);
    mb(b)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__New(isolate: *mut RealIsolate) -> *const crate::Map {
  let v = new_builtin(isolate, c"Map".as_ptr());
  if v.tag == JS_TAG_EXCEPTION {
    return ptr::null();
  }
  intern::<crate::Map>(v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Map__Set(
  this: *const crate::Map,
  context: *const Context,
  key: *const Value,
  value: *const Value,
) -> *const crate::Map {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  unsafe {
    let mut args = [
      JS_DupValue(ctx, jsval_of(key)),
      JS_DupValue(ctx, jsval_of(value)),
    ];
    let r =
      call_collection_method(ctx, jsval_of(this), c"set".as_ptr(), &mut args);
    JS_FreeValue(ctx, args[0]);
    JS_FreeValue(ctx, args[1]);
    if r.tag == JS_TAG_EXCEPTION {
      return ptr::null();
    }
    // Map.prototype.set returns the map; return our handle to `this`.
    JS_FreeValue(ctx, r);
    intern::<crate::Map>(JS_DupValue(ctx, jsval_of(this)))
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Proxy__IsRevoked(
  _this: *const std::os::raw::c_void,
) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Proxy__New(
  _context: *const std::os::raw::c_void,
  _target: *const std::os::raw::c_void,
  _handler: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Proxy__Revoke(_this: *const std::os::raw::c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__Clear(this: *const crate::Set) {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return;
  }
  unsafe {
    let r =
      call_collection_method(ctx, jsval_of(this), c"clear".as_ptr(), &mut []);
    JS_FreeValue(ctx, r);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__Delete(
  this: *const crate::Set,
  context: *const Context,
  key: *const Value,
) -> crate::support::MaybeBool {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return crate::support::MaybeBool::Nothing;
  }
  unsafe {
    let mut args = [JS_DupValue(ctx, jsval_of(key))];
    let r = call_collection_method(
      ctx,
      jsval_of(this),
      c"delete".as_ptr(),
      &mut args,
    );
    JS_FreeValue(ctx, args[0]);
    if r.tag == JS_TAG_EXCEPTION {
      return crate::support::MaybeBool::Nothing;
    }
    let b = JS_ToBool(ctx, r) != 0;
    JS_FreeValue(ctx, r);
    mb(b)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Set__Has(
  this: *const crate::Set,
  context: *const Context,
  key: *const Value,
) -> crate::support::MaybeBool {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return crate::support::MaybeBool::Nothing;
  }
  unsafe {
    let mut args = [JS_DupValue(ctx, jsval_of(key))];
    let r =
      call_collection_method(ctx, jsval_of(this), c"has".as_ptr(), &mut args);
    JS_FreeValue(ctx, args[0]);
    if r.tag == JS_TAG_EXCEPTION {
      return crate::support::MaybeBool::Nothing;
    }
    let b = JS_ToBool(ctx, r) != 0;
    JS_FreeValue(ctx, r);
    mb(b)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__Abort(
  this: *mut std::os::raw::c_void,
) {
  super::wasm::module_compilation_abort(this);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__Finish(
  this: *mut std::os::raw::c_void,
  isolate: *mut std::os::raw::c_void,
  caching_callback: *const std::os::raw::c_void,
  resolution_callback: *const std::os::raw::c_void,
  resolution_data: *mut std::os::raw::c_void,
  drop_resolution_data: *const std::os::raw::c_void,
) {
  super::wasm::module_compilation_finish(
    this,
    isolate,
    caching_callback,
    resolution_callback,
    resolution_data,
    drop_resolution_data,
  );
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__OnBytesReceived(
  this: *mut std::os::raw::c_void,
  bytes: *const std::os::raw::c_void,
  size: usize,
) {
  super::wasm::module_compilation_on_bytes_received(this, bytes, size);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__SetUrl(
  this: *mut std::os::raw::c_void,
  url: *const std::os::raw::c_void,
  length: usize,
) {
  super::wasm::module_compilation_set_url(this, url, length);
}
