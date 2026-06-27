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
  ctx_of, current_ctx, current_iso, intern, intern_dup, iso_state, jsval_of,
};
use crate::quickjs::quickjs_sys::*;
use crate::{Context, Data, Object, RealIsolate, String as V8String, Value};

use std::os::raw::{c_char, c_void};
use std::ptr;

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

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Member__CONSTRUCT(
  member: *mut *mut c_void,
  obj: *mut c_void,
) {
  if !member.is_null() {
    unsafe { *member = obj };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Member__DESTRUCT(member: *mut *mut c_void) {
  if !member.is_null() {
    unsafe { *member = ptr::null_mut() };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__WeakMember__CONSTRUCT(
  member: *mut *mut c_void,
  obj: *mut c_void,
) {
  if !member.is_null() {
    unsafe { *member = obj };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__WeakMember__DESTRUCT(member: *mut *mut c_void) {
  if !member.is_null() {
    unsafe { *member = ptr::null_mut() };
  }
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
  _isolate: *mut RealIsolate,
  data: *const Data,
  _parameter: *const c_void,
  _callback: unsafe extern "C" fn(*const c_void),
) -> *const Data {
  v8__Global__New(_isolate, data)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Global__Reset(data: *const Data) {
  if data.is_null() {
    return;
  }

  if super::core::is_non_value_handle(data) {
    return;
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
  _params: *const c_void,
) {
  let iso = crate::quickjs::core::v8__Isolate__New(ptr::null());
  crate::quickjs::core::v8__Isolate__Enter(iso);
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

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__CreateBlob(
  _this: *mut c_void,
  _function_code_handling: u32,
) -> RawStartupDataAbi {
  RawStartupDataAbi {
    data: ptr::null(),
    raw_size: 0,
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__SetDefaultContext(
  _this: *mut c_void,
  _context: *const Context,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__AddContext(
  _this: *mut c_void,
  _context: *const Context,
) -> usize {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SnapshotCreator__AddData_to_context(
  _this: *mut c_void,
  _context: *const Context,
  _data: *const Data,
) -> usize {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StartupData__CanBeRehashed(_this: *const c_void) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StartupData__IsValid(_this: *const c_void) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__StartupData__data__DELETE(_this: *const c_char) {}

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
  _this: *const c_void,
) -> *mut c_void {
  current_iso() as *mut c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WeakCallbackInfo__GetParameter(
  _this: *const c_void,
) -> *mut c_void {
  ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WeakCallbackInfo__SetSecondPassCallback(
  _this: *const c_void,
  _callback: unsafe extern "C" fn(*const c_void),
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__Unpack(
  _isolate: *mut c_void,
  _value: *const Value,
  _that: *mut c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__shared_ptr_DESTRUCT(_this: *mut c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__OnBytesReceived(
  _this: *mut c_void,
  _data: *const u8,
  _len: usize,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__Finish(
  _this: *mut c_void,
  _callback: Option<unsafe extern "C" fn(*mut c_void)>,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__Abort(
  _this: *mut c_void,
  _exception: *const Value,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmStreaming__SetUrl(
  _this: *mut c_void,
  _url: *const c_char,
  _len: usize,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleObject__FromCompiledModule(
  _isolate: *mut c_void,
  _compiled_module: *const c_void,
) -> *const c_void {
  ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleObject__GetCompiledModule(
  _this: *const c_void,
) -> *mut c_void {
  ptr::null_mut()
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
pub extern "C" fn v8__CompiledWasmModule__DELETE(_this: *mut c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__NEW() -> *mut c_void {
  ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__WasmModuleCompilation__DELETE(_this: *mut c_void) {}

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
    if JS_IsConstructor(ctx, ctor) == 0 {
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
    if JS_IsFunction(ctx, add) == 0 {
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
    if JS_IsConstructor(ctx, ctor) == 0 {
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
    if JS_IsFunction(ctx, vo) == 0 {
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
  _isolate: *mut RealIsolate,
  _callback: crate::isolate::GcCallbackWithData,
  _data: *mut c_void,
  _gc_type_filter: crate::gc::GCType,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__AddGCEpilogueCallback(
  _isolate: *mut RealIsolate,
  _callback: crate::isolate::GcCallbackWithData,
  _data: *mut c_void,
  _gc_type_filter: crate::gc::GCType,
) {
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
  _isolate: *mut RealIsolate,
  change_in_bytes: i64,
) -> i64 {
  use std::sync::atomic::{AtomicI64, Ordering};
  static TOTAL: AtomicI64 = AtomicI64::new(0);
  TOTAL.fetch_add(change_in_bytes, Ordering::SeqCst) + change_in_bytes
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
