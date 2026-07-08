//! QuickJS-ng-backed shims for the "function" family:
//! Function / FunctionCallbackInfo / ReturnValue / Template / ObjectTemplate /
//! Signature / External.
//!
//! Ported from the JSC backend (`src/function.rs`) with JSC C-API calls
//! swapped for QuickJS-ng calls, and from the deno PR's QuickJS logic
//! (`reference/qjs_v8_compat/src/function.rs`, `external.rs`). The C-ABI shape
//! (the `RawFunctionCallbackInfoParts` / `RawReturnValue` layouts and the
//! `*const FunctionCallbackInfo` contract) is identical to the JSC backend — the
//! vendored `src/function.rs` only cares about layout, not which engine backs it.
//!
//! ## Host functions
//! QuickJS-ng's `JS_NewCFunctionData` is documented to crash in this build (see
//! the PR), so — exactly like the PR's `build_op_function` — we create callable
//! functions with `JS_NewCFunction2(..., JS_CFUNC_generic_magic, magic)` and
//! recover the (v8 callback, data) pair from a per-thread dispatch table keyed by
//! the integer `magic`. Constructor (`new F()`) dispatch reuses the same table
//! via a `JS_CFUNC_constructor_magic` trampoline.
//!
//! ## Refcount discipline
//! Every shim that RETURNS a v8 handle routes its `JSValue` through
//! `intern`/`intern_dup` so the arena owns exactly one refcount. The dispatch
//! table holds an owned (`JS_DupValue`'d) copy of each callback's `data`
//! JSValue; it is intentionally never freed (lives for the isolate's lifetime,
//! like a v8 FunctionTemplate's data).

#![allow(non_snake_case, unused)]

use crate::quickjs::core::{
  ctx_of, current_ctx, current_iso, intern, intern_dup, iso_state, jsval_of,
};
use crate::quickjs::quickjs_sys::*;
use crate::{
  Context, Data, External, Function, FunctionCallback, FunctionCallbackInfo,
  FunctionTemplate, Name, Object, ObjectTemplate, PropertyAttribute,
  RealIsolate, Signature, String, Value,
};
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::ptr::NonNull;

unsafe extern "C" {

  fn JS_NewClassID(rt: *mut JSRuntime, pclass_id: *mut JSClassID) -> JSClassID;
  fn JS_NewClass(
    rt: *mut JSRuntime,
    class_id: JSClassID,
    class_def: *const JSClassDef,
  ) -> c_int;
  fn JS_NewObjectClass(ctx: *mut JSContext, class_id: c_int) -> JSValue;
  fn JS_SetOpaque(obj: JSValue, opaque: *mut c_void);
  fn JS_GetOpaque(obj: JSValue, class_id: JSClassID) -> *mut c_void;
  fn JS_SetPrototype(
    ctx: *mut JSContext,
    obj: JSValue,
    proto: JSValue,
  ) -> c_int;
  fn JS_PreventExtensions(ctx: *mut JSContext, obj: JSValue) -> c_int;

  fn JS_DefinePropertyValueStr(
    ctx: *mut JSContext,
    this_obj: JSValue,
    prop: *const c_char,
    val: JSValue,
    flags: c_int,
  ) -> c_int;

  fn JS_ValueToAtom(ctx: *mut JSContext, val: JSValue) -> JSAtom;
  fn JS_DupAtom(ctx: *mut JSContext, v: JSAtom) -> JSAtom;
  fn JS_GetProperty(
    ctx: *mut JSContext,
    this_obj: JSValue,
    prop: JSAtom,
  ) -> JSValue;
  fn JS_DefinePropertyValue(
    ctx: *mut JSContext,
    this_obj: JSValue,
    prop: JSAtom,
    val: JSValue,
    flags: c_int,
  ) -> c_int;
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
}

type JSClassFinalizer = unsafe extern "C" fn(rt: *mut JSRuntime, val: JSValue);

#[repr(C)]
struct JSClassDef {
  class_name: *const c_char,
  finalizer: Option<JSClassFinalizer>,
  gc_mark: *const c_void,
  call: *const c_void,
  exotic: *const c_void,
}

#[repr(C)]
struct RawReturnValue(usize);

#[repr(C)]
struct RawFunctionCallbackInfoParts {
  isolate: *mut RealIsolate,
  return_value: usize,
  data: *const Value,
  length: crate::support::int,
}

#[repr(C)]
struct CbInfo {
  isolate: *mut RealIsolate,
  ctx: *mut JSContext,
  this: JSValue,
  data: JSValue,
  new_target: JSValue,
  is_construct: bool,
  args: Vec<JSValue>,

  return_slot: Box<JSValue>,
}

struct FnTemplate {
  callback: FunctionCallback,

  data: JSValue,

  constructable: bool,
  length: i32,
  class_name: Option<std::string::String>,
  proto: *mut ObjTemplate,
  instance: *mut ObjTemplate,
  parent: *const FnTemplate,

  props: Vec<(JSValue, JSValue, u32)>,
  accessors: Vec<TemplAccessor>,

  cached_proto: JSValue,
}

struct TemplAccessor {
  key: JSValue,
  getter: *const FnTemplate,
  setter: *const FnTemplate,
  attr: u32,
}

struct NamedHandler {
  getter: Option<crate::NamedPropertyGetterCallback>,
  data: JSValue,
  owner_ctx: *mut JSContext,
}

struct ObjTemplate {
  internal_field_count: i32,
  props: Vec<(JSValue, JSValue, u32)>,
  accessors: Vec<TemplAccessor>,
  named_handler: Option<NamedHandler>,
  immutable_proto: bool,

  parent_fn: *const FnTemplate,
}

impl ObjTemplate {
  /// Bare template for tape replay.
  fn default_for_tape() -> ObjTemplate {
    ObjTemplate {
      internal_field_count: 0,
      props: Vec::new(),
      accessors: Vec::new(),
      named_handler: None,
      immutable_proto: false,
      parent_fn: std::ptr::null(),
    }
  }
}

#[repr(C)]
struct JSPropertyEnum {
  is_enumerable: bool,
  atom: JSAtom,
}

const JS_GPN_STRING_MASK_QJS: c_int = 1 << 0;

struct DispatchEntry {
  callback: FunctionCallback,

  data: JSValue,

  instance: *const ObjTemplate,
}

thread_local! {
    static DISPATCH: std::cell::RefCell<Vec<DispatchEntry>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static NAMED_GETTER_DISPATCH: std::cell::RefCell<Vec<NamedGetterEntry>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[derive(Clone, Copy)]
struct NamedGetterEntry {
  getter: crate::NamedPropertyGetterCallback,
  data: JSValue,
  owner_ctx: *mut JSContext,
  atom: JSAtom,
}

pub(crate) mod timing {
  use std::cell::Cell;
  use std::time::{Duration, Instant};
  thread_local! {
      pub static ON: Cell<bool> = Cell::new(std::env::var_os("V82JSC_TIMING").is_some());
      pub static GETFN_N: Cell<u64> = const { Cell::new(0) };
      pub static GETFN_T: Cell<Duration> = const { Cell::new(Duration::ZERO) };
      pub static PROTO_N: Cell<u64> = const { Cell::new(0) };
      pub static PROTO_T: Cell<Duration> = const { Cell::new(Duration::ZERO) };
      pub static APPLY_N: Cell<u64> = const { Cell::new(0) };
      pub static NEWINST_N: Cell<u64> = const { Cell::new(0) };
      pub static NEWINST_T: Cell<Duration> = const { Cell::new(Duration::ZERO) };
  }
  #[inline]
  pub fn on() -> bool {
    ON.with(|c| c.get())
  }
  pub fn now() -> Option<Instant> {
    if on() { Some(Instant::now()) } else { None }
  }
  pub fn add(
    n: &'static std::thread::LocalKey<Cell<u64>>,
    t: &'static std::thread::LocalKey<Cell<Duration>>,
    start: Option<Instant>,
  ) {
    if let Some(s) = start {
      n.with(|c| c.set(c.get() + 1));
      t.with(|c| c.set(c.get() + s.elapsed()));
    }
  }
  pub fn dump() {
    if !on() {
      return;
    }
    let g = (GETFN_N.with(|c| c.get()), GETFN_T.with(|c| c.get()));
    let p = (PROTO_N.with(|c| c.get()), PROTO_T.with(|c| c.get()));
    let a = APPLY_N.with(|c| c.get());
    let ni = (NEWINST_N.with(|c| c.get()), NEWINST_T.with(|c| c.get()));
    eprintln!(
      "[V82JSC_TIMING] GetFunction: {} calls, {:.2} ms",
      g.0,
      g.1.as_secs_f64() * 1000.0
    );
    eprintln!(
      "[V82JSC_TIMING] build_prototype_object: {} calls, {:.2} ms",
      p.0,
      p.1.as_secs_f64() * 1000.0
    );
    eprintln!(
      "[V82JSC_TIMING] ObjectTemplate::NewInstance: {} calls, {:.2} ms",
      ni.0,
      ni.1.as_secs_f64() * 1000.0
    );
    eprintln!("[V82JSC_TIMING] apply_props: {} calls", a);
  }
}

fn register_dispatch(
  callback: FunctionCallback,
  data: JSValue,
  instance: *const ObjTemplate,
) -> c_int {
  DISPATCH.with(|t| {
    let mut t = t.borrow_mut();
    let idx = t.len() as c_int;
    t.push(DispatchEntry {
      callback,
      data,
      instance,
    });
    idx
  })
}

fn lookup_dispatch(
  idx: c_int,
) -> Option<(FunctionCallback, JSValue, *const ObjTemplate)> {
  DISPATCH.with(|t| {
    t.borrow()
      .get(idx as usize)
      .map(|e| (e.callback, e.data, e.instance))
  })
}

fn register_named_getter(
  ctx: *mut JSContext,
  handler: &NamedHandler,
  atom: JSAtom,
) -> c_int {
  let Some(getter) = handler.getter else {
    return 0;
  };
  NAMED_GETTER_DISPATCH.with(|d| {
    let mut d = d.borrow_mut();
    let idx = d.len() as c_int;
    d.push(NamedGetterEntry {
      getter,
      data: unsafe { JS_DupValue(ctx, handler.data) },
      owner_ctx: handler.owner_ctx,
      atom: unsafe { JS_DupAtom(ctx, atom) },
    });
    idx
  })
}

fn lookup_named_getter(idx: c_int) -> Option<NamedGetterEntry> {
  NAMED_GETTER_DISPATCH.with(|d| d.borrow().get(idx as usize).copied())
}

unsafe fn materialize_named_handler_data(
  ctx: *mut JSContext,
  entry: NamedGetterEntry,
) -> (JSValue, bool) {
  if entry.owner_ctx.is_null() || entry.owner_ctx == ctx {
    return (entry.data, false);
  }

  let obj = unsafe { JS_NewObject(ctx) };
  let value =
    unsafe { JS_GetProperty(entry.owner_ctx, entry.data, entry.atom) };
  if value.tag == JS_TAG_EXCEPTION {
    let exc = unsafe { JS_GetException(entry.owner_ctx) };
    unsafe { JS_FreeValue(entry.owner_ctx, exc) };
    return (obj, true);
  }
  unsafe {
    JS_DefinePropertyValue(
      ctx,
      obj,
      entry.atom,
      value,
      JS_PROP_CONFIGURABLE | JS_PROP_ENUMERABLE | JS_PROP_WRITABLE,
    );
  }
  (obj, true)
}

unsafe fn dispatch(
  ctx: *mut JSContext,
  callback: FunctionCallback,
  data: JSValue,
  this: JSValue,
  new_target: JSValue,
  is_construct: bool,
  argc: c_int,
  argv: *mut JSValue,
) -> JSValue {
  let iso = current_iso();

  // Emulate `v8::Isolate::TerminateExecution`: once a termination is pending, V8
  // throws an uncatchable exception at the next safe point and runs no further
  // JS until it is cancelled. The most important safe point for deno_core is the
  // op call itself — e.g. after an unhandled-rejection handler calls
  // `reportUnhandledException` (which dispatches an exception and terminates),
  // the runtime would otherwise go on to `op_dispatch_exception` the *original*
  // rejection, overwriting the reported one. Refuse to run the op and surface an
  // uncatchable "interrupted" error instead.
  if !iso.is_null() && iso_state(iso).is_terminating() {
    unsafe {
      // `JS_GetException` takes the pending exception (clearing it), so mark it
      // uncatchable and re-throw — mirroring QuickJS's own `JS_ThrowInterrupted`.
      JS_ThrowInternalError(ctx, c"interrupted".as_ptr());
      let exc = JS_GetException(ctx);
      JS_SetUncatchableError(ctx, exc);
      JS_Throw(ctx, exc);
    }
    return jsv_exception();
  }

  let n = argc.max(0) as usize;
  let mut args = Vec::with_capacity(n);
  for i in 0..n {
    args.push(unsafe { *argv.add(i) });
  }

  // Every handle the callback interns (its arguments, `data`, `this`, any Local
  // it creates) lands in the isolate's handle arena. V8 frees those when the
  // callback's implicit HandleScope unwinds; we have no such scope, so without
  // this boundary they accumulate for the lifetime of the *enclosing* scope —
  // which, for an op called in a 6000-iteration JIT-warmup loop, means tens of
  // thousands of live arena slots, each pinning a QuickJS reference (notably the
  // op's `data` External, whose refcount then balloons into the thousands).
  // Bracket each callback with a save/restore of the arena depth so it behaves
  // like V8's per-callback HandleScope. Record the depth now and release
  // everything the callback added once we're done.
  let saved_depth = if iso.is_null() {
    0
  } else {
    iso_state(iso).handles.len()
  };

  let info = Box::new(CbInfo {
    isolate: iso,
    ctx,
    this,
    data,
    new_target,
    is_construct,
    args,
    return_slot: Box::new(jsv_undefined()),
  });
  let info_ptr = Box::into_raw(info) as *const FunctionCallbackInfo;
  unsafe { (callback)(info_ptr) };

  let info = unsafe { Box::from_raw(info_ptr as *mut CbInfo) };
  let ret = *info.return_slot;

  // Materialize the result as a value we own *before* tearing down the arena:
  // `ret` is only a borrowed copy of whatever Local the callback stored via
  // ReturnValue::set, and that Local lives in the slots we're about to free.
  let result = if unsafe { JS_HasException(ctx) } {
    jsv_exception()
  } else if jsv_is_undefined(&ret) {
    jsv_undefined()
  } else {
    unsafe { JS_DupValue(ctx, ret) }
  };

  if !iso.is_null() {
    let st = iso_state(iso);
    while st.handles.len() > saved_depth {
      if let Some(slot) = st.handles.pop() {
        unsafe {
          JS_FreeValue(st.ctx, *slot);
          drop(Box::from_raw(slot));
        }
      }
    }
  }

  result
}

pub(crate) unsafe fn call_callback_for_wasm(
  ctx: *mut JSContext,
  callback: FunctionCallback,
  data: JSValue,
  args: &mut [JSValue],
) -> JSValue {
  unsafe {
    dispatch(
      ctx,
      callback,
      data,
      jsv_undefined(),
      jsv_undefined(),
      false,
      args.len() as c_int,
      args.as_mut_ptr(),
    )
  }
}

unsafe extern "C" fn fn_trampoline(
  ctx: *mut JSContext,
  this_val: JSValue,
  argc: c_int,
  argv: *mut JSValue,
  magic: c_int,
) -> JSValue {
  let Some((callback, data, _instance)) = lookup_dispatch(magic) else {
    return jsv_undefined();
  };
  unsafe {
    dispatch(
      ctx,
      callback,
      data,
      this_val,
      jsv_undefined(),
      false,
      argc,
      argv,
    )
  }
}

unsafe extern "C" fn fn_construct_trampoline(
  ctx: *mut JSContext,
  new_target: JSValue,
  argc: c_int,
  argv: *mut JSValue,
  magic: c_int,
) -> JSValue {
  let Some((callback, data, instance)) = lookup_dispatch(magic) else {
    return unsafe { JS_NewObject(ctx) };
  };

  if jsv_is_undefined(&new_target) {
    return unsafe {
      dispatch(
        ctx,
        callback,
        data,
        jsv_undefined(),
        jsv_undefined(),
        false,
        argc,
        argv,
      )
    };
  }

  let this = unsafe { JS_NewObject(ctx) };
  unsafe {
    let proto = JS_GetPropertyStr(ctx, new_target, c"prototype".as_ptr());
    if std::env::var_os("QJS_DEBUG_TMPL").is_some() {
      let is_obj = jsv_is_object(&proto);
      let has_log = if is_obj {
        let l = JS_GetPropertyStr(ctx, proto, c"log".as_ptr());
        let f = !jsv_is_undefined(&l);
        JS_FreeValue(ctx, l);
        f
      } else {
        false
      };
      eprintln!(
        "[QJS construct] proto_is_obj={is_obj} proto_has_log={has_log}"
      );
    }
    if jsv_is_object(&proto) {
      JS_SetPrototype(ctx, this, proto);
    }
    JS_FreeValue(ctx, proto);
  }

  if !instance.is_null() {
    let t = unsafe { &*instance };
    super::object::set_internal_field_count_for_value(
      this,
      t.internal_field_count,
    );
    apply_props(ctx, this, &t.props);
    apply_accessors(ctx, this, &t.accessors);
  }

  let r = unsafe {
    dispatch(ctx, callback, data, this, new_target, true, argc, argv)
  };
  if jsv_is_exception(&r) {
    unsafe { JS_FreeValue(ctx, this) };
    return r;
  }

  let result = if jsv_is_object(&r) {
    unsafe { JS_FreeValue(ctx, this) };
    r
  } else {
    unsafe { JS_FreeValue(ctx, r) };
    this
  };
  result
}

unsafe extern "C" fn named_getter_trampoline(
  ctx: *mut JSContext,
  this_val: JSValue,
  _argc: c_int,
  _argv: *mut JSValue,
  magic: c_int,
) -> JSValue {
  let Some(entry) = lookup_named_getter(magic) else {
    return jsv_undefined();
  };

  if !entry.owner_ctx.is_null()
    && entry.owner_ctx != ctx
    && !super::misc::contexts_share_security_token(ctx, entry.owner_ctx)
  {
    return unsafe { JS_ThrowTypeError(ctx, c"no access".as_ptr()) };
  }

  let iso = current_iso();
  let saved_depth = if iso.is_null() {
    0
  } else {
    iso_state(iso).handles.len()
  };
  let (callback_data, free_callback_data) =
    unsafe { materialize_named_handler_data(ctx, entry) };

  let info = Box::new(CbInfo {
    isolate: iso,
    ctx,
    this: this_val,
    data: callback_data,
    new_target: jsv_undefined(),
    is_construct: false,
    args: Vec::new(),
    return_slot: Box::new(jsv_undefined()),
  });
  let info_ptr = Box::into_raw(info);
  let mut raw_info = info_ptr as *mut c_void;
  let prop_info = &mut raw_info as *mut *mut c_void
    as *const crate::PropertyCallbackInfo<Value>;

  let key = unsafe { JS_AtomToValue(ctx, entry.atom) };
  let key_handle = intern::<Name>(key);
  let Some(key_handle) = NonNull::new(key_handle as *mut Name) else {
    let _ = unsafe { Box::from_raw(info_ptr) };
    return jsv_undefined();
  };
  let intercepted =
    unsafe { (entry.getter)(crate::SealedLocal(key_handle), prop_info) };

  let info = unsafe { Box::from_raw(info_ptr) };
  let ret = *info.return_slot;
  let intercepted = unsafe { std::mem::transmute_copy::<_, u32>(&intercepted) };
  let result = if unsafe { JS_HasException(ctx) } {
    jsv_exception()
  } else if intercepted == 0 {
    if jsv_is_undefined(&ret) {
      jsv_undefined()
    } else {
      unsafe { JS_DupValue(ctx, ret) }
    }
  } else {
    jsv_undefined()
  };

  if !iso.is_null() {
    let st = iso_state(iso);
    while st.handles.len() > saved_depth {
      if let Some(slot) = st.handles.pop() {
        unsafe {
          JS_FreeValue(st.ctx, *slot);
          drop(Box::from_raw(slot));
        }
      }
    }
  }
  if free_callback_data {
    unsafe { JS_FreeValue(ctx, callback_data) };
  }

  result
}

unsafe fn make_cfunc_magic(
  ctx: *mut JSContext,
  trampoline: unsafe extern "C" fn(
    *mut JSContext,
    JSValue,
    c_int,
    *mut JSValue,
    c_int,
  ) -> JSValue,
  name: *const c_char,
  length: c_int,
  cproto: c_int,
  magic: c_int,
) -> JSValue {
  unsafe {
    let f: JSCFunction = std::mem::transmute::<
      unsafe extern "C" fn(
        *mut JSContext,
        JSValue,
        c_int,
        *mut JSValue,
        c_int,
      ) -> JSValue,
      JSCFunction,
    >(trampoline);
    JS_NewCFunction2(ctx, f, name, length, cproto, magic)
  }
}

unsafe extern "C" fn native_function_to_string(
  ctx: *mut JSContext,
  this_val: JSValue,
  _argc: c_int,
  _argv: *mut JSValue,
) -> JSValue {
  let name = unsafe { JS_GetPropertyStr(ctx, this_val, c"name".as_ptr()) };
  let name_text = if jsv_is_string(&name) {
    let cstr = unsafe { JS_ToCString(ctx, name) };
    let text = if cstr.is_null() {
      std::string::String::new()
    } else {
      unsafe { std::ffi::CStr::from_ptr(cstr) }
        .to_string_lossy()
        .into_owned()
    };
    if !cstr.is_null() {
      unsafe { JS_FreeCString(ctx, cstr) };
    }
    text
  } else {
    std::string::String::new()
  };
  unsafe { JS_FreeValue(ctx, name) };

  let text = if name_text.is_empty() {
    "function () { [native code] }".to_owned()
  } else {
    format!("function {name_text}() {{ [native code] }}")
  };
  unsafe { JS_NewStringLen(ctx, text.as_ptr() as *const c_char, text.len()) }
}

unsafe fn install_native_function_to_string(
  ctx: *mut JSContext,
  function: JSValue,
) {
  let to_string = unsafe {
    JS_NewCFunction(ctx, native_function_to_string, c"toString".as_ptr(), 0)
  };
  unsafe {
    JS_DefinePropertyValueStr(
      ctx,
      function,
      c"toString".as_ptr(),
      to_string,
      JS_PROP_CONFIGURABLE | JS_PROP_WRITABLE,
    );
  }
}

pub(crate) unsafe fn make_function_len(
  ctx: *mut JSContext,
  callback: FunctionCallback,
  data: JSValue,
  length: i32,
  construct: bool,
) -> JSValue {
  unsafe {
    make_function_len_with_instance(
      ctx,
      callback,
      data,
      length,
      construct,
      ptr::null(),
    )
  }
}

unsafe fn make_function_len_with_instance(
  ctx: *mut JSContext,
  callback: FunctionCallback,
  data: JSValue,
  length: i32,
  construct: bool,
  instance: *const ObjTemplate,
) -> JSValue {
  let data_owned = if jsv_is_undefined(&data) {
    jsv_undefined()
  } else {
    unsafe { JS_DupValue(ctx, data) }
  };
  let magic = register_dispatch(callback, data_owned, instance);
  let cproto = if construct {
    JS_CFUNC_CONSTRUCTOR_OR_FUNC_MAGIC
  } else {
    JS_CFUNC_GENERIC_MAGIC
  };
  let tramp = if construct {
    fn_construct_trampoline
  } else {
    fn_trampoline
  };
  let f = unsafe {
    make_cfunc_magic(ctx, tramp, ptr::null(), length.max(0), cproto, magic)
  };
  unsafe { install_native_function_to_string(ctx, f) };
  f
}

const JS_CFUNC_CONSTRUCTOR_MAGIC: c_int = 3;
const JS_CFUNC_CONSTRUCTOR_OR_FUNC_MAGIC: c_int = 5;

#[inline]
fn cbinfo<'a>(this: *const FunctionCallbackInfo) -> &'a mut CbInfo {
  unsafe { &mut *(this as *mut CbInfo) }
}

unsafe fn prop_cbinfo<'a>(this: *const c_void) -> Option<&'a mut CbInfo> {
  if this.is_null() {
    return None;
  }
  let info_ptr = unsafe { *(this as *const *mut c_void) as *mut CbInfo };
  if info_ptr.is_null() {
    None
  } else {
    Some(unsafe { &mut *info_ptr })
  }
}

fn property_return_scratch() -> usize {
  thread_local! {
      static SCRATCH: std::cell::UnsafeCell<JSValue> =
          const { std::cell::UnsafeCell::new(JSValue {
              u: JSValueUnion { int32: 0 },
              tag: JS_TAG_UNDEFINED,
          }) };
  }
  SCRATCH.with(|s| s.get() as usize)
}

unsafe extern "C" fn ext_finalize(_rt: *mut JSRuntime, _val: JSValue) {}

fn external_values()
-> &'static std::sync::Mutex<std::collections::HashMap<usize, usize>> {
  static T: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<usize, usize>>,
  > = std::sync::OnceLock::new();
  T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

#[inline]
fn external_key(v: JSValue) -> usize {
  jsv_get_ptr(&v) as usize
}

/// Allocate a class id and register the `v8::External` class on `rt`. Must be
/// called BEFORE the runtime's first context is created (a context sizes its
/// `class_proto` array to `rt->class_count` at creation time and never grows it,
/// so a class registered afterward makes `JS_NewObjectClass` read `class_proto`
/// out of bounds). Called once per isolate from `v8__Isolate__New`; the id is
/// stored in `IsoState` rather than a thread-local so it stays correct across
/// multiple isolates on one thread (each runtime registers its own class).
pub(crate) fn register_external_class(rt: *mut JSRuntime) -> JSClassID {
  let mut id: JSClassID = 0;
  let id = unsafe { JS_NewClassID(rt, &mut id) };
  let def = JSClassDef {
    class_name: c"v8jsc_external".as_ptr(),
    finalizer: Some(ext_finalize),
    gc_mark: ptr::null(),
    call: ptr::null(),
    exotic: ptr::null(),
  };
  unsafe { JS_NewClass(rt, id, &def) };
  id
}

/// The `v8::External` class id for the current isolate (0 if unavailable).
fn ext_class_id_current() -> JSClassID {
  let iso = current_iso();
  if iso.is_null() {
    return 0;
  }
  iso_state(iso).ext_class_id
}

/// Build a v8::External-style wrapper object (owned +1 JSValue). Shared by
/// `v8__External__New` and the snapshot tape replayer.
pub(crate) fn make_external_jsvalue(
  isolate: *mut RealIsolate,
  ctx: *mut JSContext,
  value: *mut c_void,
) -> JSValue {
  if isolate.is_null() || ctx.is_null() {
    return jsv_undefined();
  }
  let cid = iso_state(isolate).ext_class_id;
  let obj = unsafe { JS_NewObjectClass(ctx, cid as c_int) };
  if jsv_is_exception(&obj) {
    return jsv_undefined();
  }
  unsafe { JS_SetOpaque(obj, value) };
  external_values()
    .lock()
    .unwrap()
    .insert(external_key(obj), value as usize);
  obj
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__External__New(
  isolate: *mut RealIsolate,
  value: *mut c_void,
) -> *const External {
  if isolate.is_null() {
    return ptr::null();
  }
  let st = iso_state(isolate);
  let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
  if ctx.is_null() {
    return ptr::null();
  }
  let obj = make_external_jsvalue(isolate, ctx, value);
  if jsv_is_undefined(&obj) {
    return ptr::null();
  }
  let h = intern::<External>(obj);
  h
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__External__Value(this: *const External) -> *mut c_void {
  if this.is_null() {
    return ptr::null_mut();
  }
  let cid = ext_class_id_current();
  if cid == 0 {
    return external_values()
      .lock()
      .unwrap()
      .get(&external_key(jsval_of(this)))
      .copied()
      .unwrap_or(0) as *mut c_void;
  }
  let v = jsval_of(this);
  let opaque = unsafe { JS_GetOpaque(v, cid) };
  if !opaque.is_null() {
    return opaque;
  }
  external_values()
    .lock()
    .unwrap()
    .get(&external_key(v))
    .copied()
    .unwrap_or(0) as *mut c_void
}

pub(crate) fn value_is_external(v: JSValue) -> bool {
  if !jsv_is_object(&v) {
    return false;
  }
  if external_values()
    .lock()
    .unwrap()
    .contains_key(&external_key(v))
  {
    return true;
  }
  let cid = ext_class_id_current();
  if cid == 0 {
    return false;
  }
  !unsafe { JS_GetOpaque(v, cid) }.is_null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__New(
  context: *const Context,
  callback: FunctionCallback,
  data_or_null: *const Value,
  length: i32,
  constructor_behavior: crate::ConstructorBehavior,
  side_effect_type: crate::SideEffectType,
) -> *const Function {
  let _ = side_effect_type;
  let ctx = ctx_of(context);
  if ctx.is_null() {
    return ptr::null();
  }
  let construct =
    matches!(constructor_behavior, crate::ConstructorBehavior::Allow);
  let data = jsval_of(data_or_null);
  let f = unsafe { make_function_len(ctx, callback, data, length, construct) };
  let h = intern::<Function>(f);
  h
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__Call(
  this: *const Function,
  context: *const Context,
  recv: *const Value,
  argc: crate::support::int,
  argv: *const *const Value,
) -> *const Value {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  let func = jsval_of(this);
  let recv_v = if recv.is_null() {
    jsv_undefined()
  } else {
    jsval_of(recv)
  };
  let n = argc.max(0) as usize;
  let mut args: Vec<JSValue> = Vec::with_capacity(n);
  for i in 0..n {
    args.push(jsval_of(unsafe { *argv.add(i) }));
  }
  let r = unsafe { JS_Call(ctx, func, recv_v, n as c_int, args.as_mut_ptr()) };
  if jsv_is_exception(&r) {
    if std::env::var("V82JSC_DEBUG").is_ok() {
      unsafe {
        let exc = JS_GetException(ctx);
        let cs = JS_ToCString(ctx, exc);
        if !cs.is_null() {
          eprintln!(
            "[qjs] Function__Call threw: {}",
            std::ffi::CStr::from_ptr(cs).to_string_lossy()
          );
          JS_FreeCString(ctx, cs);
        }

        JS_Throw(ctx, exc);
      }
    }

    return ptr::null();
  }

  let h = intern::<Value>(r);
  h
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__NewInstance(
  this: *const Function,
  context: *const Context,
  argc: crate::support::int,
  argv: *const *const Value,
) -> *const Object {
  let ctx = ctx_of(context);
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  let func = jsval_of(this);
  let n = argc.max(0) as usize;
  let mut args: Vec<JSValue> = Vec::with_capacity(n);
  for i in 0..n {
    args.push(jsval_of(unsafe { *argv.add(i) }));
  }
  let r =
    unsafe { JS_CallConstructor(ctx, func, n as c_int, args.as_mut_ptr()) };
  if jsv_is_exception(&r) {
    return ptr::null();
  }
  intern::<Object>(r)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__SetName(
  this: *const Function,
  name: *const String,
) {
  if this.is_null() || name.is_null() {
    return;
  }
  let ctx = current_ctx();
  if ctx.is_null() {
    return;
  }

  const JS_PROP_CONFIGURABLE: c_int = 1 << 0;

  let v = unsafe { JS_DupValue(ctx, jsval_of(name)) };
  unsafe {
    JS_DefinePropertyValueStr(
      ctx,
      jsval_of(this),
      c"name".as_ptr(),
      v,
      JS_PROP_CONFIGURABLE,
    );
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__CreateCodeCache(
  script: *const Function,
) -> *mut crate::CachedData<'static> {
  let _ = script;
  super::module::make_placeholder_code_cache()
}

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
  let slot = &mut *info.return_slot as *mut JSValue;

  let data = intern_dup::<Value>(info.ctx, info.data);
  RawFunctionCallbackInfoParts {
    isolate: info.isolate,
    return_value: slot as usize,
    data,
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
  let info = cbinfo(this);
  intern_dup::<Value>(info.ctx, info.data)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__This(
  this: *const FunctionCallbackInfo,
) -> *const Object {
  if this.is_null() {
    return ptr::null();
  }
  let info = cbinfo(this);
  intern_dup::<Object>(info.ctx, info.this)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__NewTarget(
  this: *const FunctionCallbackInfo,
) -> *const Value {
  if this.is_null() {
    return ptr::null();
  }
  let info = cbinfo(this);

  let nt = if info.is_construct {
    info.new_target
  } else {
    jsv_undefined()
  };
  intern_dup::<Value>(info.ctx, nt)
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
    return intern_dup::<Value>(info.ctx, jsv_undefined());
  }
  match info.args.get(index as usize) {
    Some(&v) => intern_dup::<Value>(info.ctx, v),
    None => intern_dup::<Value>(info.ctx, jsv_undefined()),
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
  (&mut *info.return_slot as *mut JSValue) as usize
}

#[inline]
unsafe fn rv_slot(this: *mut RawReturnValue) -> *mut JSValue {
  if this.is_null() {
    return ptr::null_mut();
  }
  unsafe { (*this).0 as *mut JSValue }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Set(
  this: *mut RawReturnValue,
  value: *const Value,
) {
  let slot = unsafe { rv_slot(this) };
  if !slot.is_null() {
    unsafe { *slot = jsval_of(value) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Set__Bool(
  this: *mut RawReturnValue,
  value: bool,
) {
  let slot = unsafe { rv_slot(this) };
  if !slot.is_null() {
    unsafe { *slot = jsv_bool(value) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Set__Int32(
  this: *mut RawReturnValue,
  value: i32,
) {
  let slot = unsafe { rv_slot(this) };
  if !slot.is_null() {
    unsafe { *slot = jsv_int32(value) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Set__Uint32(
  this: *mut RawReturnValue,
  value: u32,
) {
  let slot = unsafe { rv_slot(this) };
  if !slot.is_null() {
    let v = if value <= i32::MAX as u32 {
      jsv_int32(value as i32)
    } else {
      jsv_float64(value as f64)
    };
    unsafe { *slot = v };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Set__Double(
  this: *mut RawReturnValue,
  value: f64,
) {
  let slot = unsafe { rv_slot(this) };
  if !slot.is_null() {
    unsafe { *slot = jsv_float64(value) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__SetNull(this: *mut RawReturnValue) {
  let slot = unsafe { rv_slot(this) };
  if !slot.is_null() {
    unsafe { *slot = jsv_null() };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__SetUndefined(
  this: *mut RawReturnValue,
) {
  let slot = unsafe { rv_slot(this) };
  if !slot.is_null() {
    unsafe { *slot = jsv_undefined() };
  }
}

trait AttrU32 {
  fn as_u32_lenient(&self) -> u32;
}
impl AttrU32 for PropertyAttribute {
  fn as_u32_lenient(&self) -> u32 {
    unsafe { *(self as *const PropertyAttribute as *const u32) }
  }
}

thread_local! {
    static TEMPLATES: std::cell::RefCell<std::collections::HashMap<usize, TemplKind>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TemplKind {
  Func,
  Obj,
}

fn register_template(p: usize, kind: TemplKind) {
  TEMPLATES.with(|t| {
    t.borrow_mut().insert(p, kind);
  });
}

pub(crate) fn is_template_ptr(p: *const c_void) -> bool {
  if p.is_null() {
    return false;
  }
  TEMPLATES.with(|t| t.borrow().contains_key(&(p as usize)))
}

pub(crate) fn template_kind(p: *const c_void) -> Option<TemplKind> {
  if p.is_null() {
    return None;
  }
  TEMPLATES.with(|t| t.borrow().get(&(p as usize)).copied())
}

fn with_template_props(
  p: usize,
  f: impl FnOnce(&mut Vec<(JSValue, JSValue, u32)>),
) {
  let kind = TEMPLATES.with(|t| t.borrow().get(&p).copied());
  match kind {
    Some(TemplKind::Func) => {
      let t = unsafe { &mut *(p as *mut FnTemplate) };
      f(&mut t.props);
    }
    Some(TemplKind::Obj) => {
      let t = unsafe { &mut *(p as *mut ObjTemplate) };
      f(&mut t.props);
    }
    None => {}
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Template__Set(
  this: *const crate::Template,
  key: *const Name,
  value: *const Data,
  attr: PropertyAttribute,
) {
  if this.is_null() {
    return;
  }
  let raw = this as *const c_void as usize;

  // The `key` and `value` arrive as borrowed handles (Local) owned by the
  // caller's HandleScope; the props Vec outlives that scope (it lives for the
  // template's lifetime). Storing the raw JSValue would leave a dangling
  // reference once the scope frees the handle — a use-after-free that QuickJS's
  // GC later trips over (NULL/garbage `shape` while marking) once the freed
  // slot is reused. So take our own reference on anything refcounted.
  let ctx = current_ctx();
  let key_owned = own_template_value(ctx, jsval_of(key));
  let stored = if is_template_ptr(value as *const c_void) {
    // A FunctionTemplate/ObjectTemplate pointer tagged for later
    // materialization — a raw pointer, not a refcounted JSValue.
    make_value(
      JS_TAG_TEMPLATE,
      JSValueUnion {
        ptr: value as *mut c_void,
      },
    )
  } else {
    own_template_value(ctx, jsval_of(value))
  };
  with_template_props(raw, |props| {
    props.push((key_owned, stored, attr.as_u32_lenient()));
  });
}

/// Take an owning reference on a JSValue that is about to be stashed in a
/// long-lived template structure (props / accessors). No-op for non-refcounted
/// values (ints, bools, the `JS_TAG_TEMPLATE`/`JS_TAG_V8_CONTEXT` sentinels) and
/// when no context is available.
#[inline]
fn own_template_value(ctx: *mut JSContext, v: JSValue) -> JSValue {
  if ctx.is_null() {
    return v;
  }
  unsafe { JS_DupValue(ctx, v) }
}

const JS_TAG_TEMPLATE: i64 = 0x7633;

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__New(
  isolate: *mut RealIsolate,
  callback: FunctionCallback,
  data_or_null: *const Value,
  signature_or_null: *const Signature,
  length: i32,
  constructor_behavior: crate::ConstructorBehavior,
  side_effect_type: crate::SideEffectType,
  c_functions: *const crate::fast_api::CFunction,
  c_functions_len: usize,
) -> *const FunctionTemplate {
  let _ = (
    isolate,
    signature_or_null,
    side_effect_type,
    c_functions,
    c_functions_len,
  );
  let constructable =
    matches!(constructor_behavior, crate::ConstructorBehavior::Allow);

  let data = {
    let ctx = current_ctx();
    let d = jsval_of(data_or_null);
    if !ctx.is_null() && !jsv_is_undefined(&d) {
      unsafe { JS_DupValue(ctx, d) }
    } else {
      jsv_undefined()
    }
  };
  let proto = Box::into_raw(Box::new(ObjTemplate {
    internal_field_count: 0,
    props: Vec::new(),
    accessors: Vec::new(),
    named_handler: None,
    immutable_proto: false,
    parent_fn: ptr::null(),
  }));
  let instance = Box::into_raw(Box::new(ObjTemplate {
    internal_field_count: 0,
    props: Vec::new(),
    accessors: Vec::new(),
    named_handler: None,
    immutable_proto: false,
    parent_fn: ptr::null(),
  }));
  register_template(proto as usize, TemplKind::Obj);
  register_template(instance as usize, TemplKind::Obj);
  let t = Box::into_raw(Box::new(FnTemplate {
    callback,
    data,
    constructable,
    length,
    class_name: None,
    proto,
    instance,
    parent: ptr::null(),
    props: Vec::new(),
    accessors: Vec::new(),
    cached_proto: jsv_undefined(),
  }));
  unsafe { (*instance).parent_fn = t };
  register_template(t as usize, TemplKind::Func);
  t as *const FunctionTemplate
}

/// Tape replay: reconstruct a FunctionTemplate (same struct the C-ABI ctor
/// builds) with a remapped callback/data pointer.
pub(crate) fn tape_make_template(
  callback: FunctionCallback,
  data_ptr: Option<*mut c_void>,
  length: i32,
  constructable: bool,
) -> *const FunctionTemplate {
  let ctx = crate::quickjs::core::current_ctx();
  let data = match data_ptr {
    Some(p) if !p.is_null() && !ctx.is_null() => {
      make_external_jsvalue(crate::quickjs::core::current_iso(), ctx, p)
    }
    _ => jsv_undefined(),
  };
  let proto = Box::into_raw(Box::new(ObjTemplate::default_for_tape()));
  let instance = Box::into_raw(Box::new(ObjTemplate::default_for_tape()));
  register_template(proto as usize, TemplKind::Obj);
  register_template(instance as usize, TemplKind::Obj);
  let t = Box::into_raw(Box::new(FnTemplate {
    callback,
    data,
    constructable,
    length,
    class_name: None,
    proto,
    instance,
    parent: ptr::null(),
    props: Vec::new(),
    accessors: Vec::new(),
    cached_proto: jsv_undefined(),
  }));
  unsafe { (*instance).parent_fn = t };
  register_template(t as usize, TemplKind::Func);
  t as *const FunctionTemplate
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__GetFunction(
  this: *const FunctionTemplate,
  context: *const Context,
) -> *const Function {
  if this.is_null() {
    return ptr::null();
  }
  let ctx = ctx_of(context);
  if ctx.is_null() {
    return ptr::null();
  }
  let _tm = timing::now();
  let t = unsafe { &*(this as *const FnTemplate) };

  let f = unsafe {
    make_function_len_with_instance(
      ctx,
      t.callback,
      t.data,
      t.length,
      t.constructable,
      t.instance,
    )
  };

  if let Some(name) = &t.class_name {
    if let Ok(cname) = std::ffi::CString::new(name.as_str()) {
      const JS_PROP_CONFIGURABLE: c_int = 1 << 0;
      let nameval = unsafe { JS_NewString(ctx, cname.as_ptr()) };
      unsafe {
        JS_DefinePropertyValueStr(
          ctx,
          f,
          c"name".as_ptr(),
          nameval,
          JS_PROP_CONFIGURABLE,
        );
      }
    }
  }

  apply_props(ctx, f, &t.props);
  apply_accessors(ctx, f, &t.accessors);

  let proto_obj =
    unsafe { build_prototype_object(ctx, this as *const FnTemplate) };
  let proto_dup = unsafe { JS_DupValue(ctx, proto_obj) };
  unsafe {
    JS_SetPropertyStr(ctx, f, c"prototype".as_ptr(), proto_dup);
    JS_DefinePropertyValueStr(
      ctx,
      proto_obj,
      c"constructor".as_ptr(),
      JS_DupValue(ctx, f),
      JS_PROP_CONFIGURABLE | JS_PROP_WRITABLE,
    );
  }

  timing::add(&timing::GETFN_N, &timing::GETFN_T, _tm);
  let h = intern::<Function>(f);
  h
}

unsafe fn build_prototype_object(
  ctx: *mut JSContext,
  tp: *const FnTemplate,
) -> JSValue {
  let t = unsafe { &mut *(tp as *mut FnTemplate) };
  if jsv_is_object(&t.cached_proto) {
    return t.cached_proto;
  }
  let _tm = timing::now();
  let proto_obj = unsafe { JS_NewObject(ctx) };
  let proto = unsafe { &*t.proto };
  if std::env::var_os("QJS_DEBUG_TMPL").is_some() {
    eprintln!(
      "[QJS build_proto] class={:?} n_props={} n_accessors={}",
      t.class_name,
      proto.props.len(),
      proto.accessors.len()
    );
  }
  if !proto.props.is_empty() {
    apply_props(ctx, proto_obj, &proto.props);
  }
  apply_accessors(ctx, proto_obj, &proto.accessors);
  if !t.parent.is_null() {
    let parent_proto = unsafe { build_prototype_object(ctx, t.parent) };
    let dup = unsafe { JS_DupValue(ctx, parent_proto) };
    unsafe { JS_SetPrototype(ctx, proto_obj, dup) };
    unsafe { JS_FreeValue(ctx, dup) };
  }

  t.cached_proto = proto_obj;
  timing::add(&timing::PROTO_N, &timing::PROTO_T, _tm);
  proto_obj
}

fn apply_props(
  ctx: *mut JSContext,
  obj: JSValue,
  props: &[(JSValue, JSValue, u32)],
) {
  timing::APPLY_N.with(|c| c.set(c.get() + 1));
  for &(key, value, attr) in props {
    if jsv_is_undefined(&key) {
      continue;
    }

    // Atom-based define so Symbol keys (e.g. a cppgc method exposed under
    // `Symbol.for("Deno_bitmapData")`) round-trip — `JS_ToCStringLen` would
    // collapse a symbol to its string description and lose it.
    let atom = unsafe { JS_ValueToAtom(ctx, key) };
    if atom == 0 {
      continue;
    }

    let value = materialize_template_value(ctx, value);
    let v = unsafe { JS_DupValue(ctx, value) };
    unsafe {
      JS_DefinePropertyValue(ctx, obj, atom, v, prop_flags_from_attr(attr));
      JS_FreeAtom(ctx, atom);
    }
  }
}

fn prop_flags_from_attr(attr: u32) -> c_int {
  let mut flags = 0;
  if (attr & 1) == 0 {
    flags |= JS_PROP_WRITABLE;
  }
  if (attr & 2) == 0 {
    flags |= JS_PROP_ENUMERABLE;
  }
  if (attr & 4) == 0 {
    flags |= JS_PROP_CONFIGURABLE;
  }
  flags
}

fn materialize_template_value(ctx: *mut JSContext, value: JSValue) -> JSValue {
  let raw = unsafe { value.u.ptr } as usize;
  let kind = TEMPLATES.with(|t| t.borrow().get(&raw).copied());
  match kind {
    Some(TemplKind::Func) => {
      let f = v8__FunctionTemplate__GetFunction(
        raw as *const FunctionTemplate,
        ctx as *const Context,
      );
      if f.is_null() { value } else { jsval_of(f) }
    }
    Some(TemplKind::Obj) => {
      let o = v8__ObjectTemplate__NewInstance(
        raw as *const ObjectTemplate,
        ctx as *const Context,
      );
      if o.is_null() { value } else { jsval_of(o) }
    }
    None => value,
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__Inherit(
  this: *const FunctionTemplate,
  parent: *const FunctionTemplate,
) {
  if this.is_null() {
    return;
  }
  let t = unsafe { &mut *(this as *mut FnTemplate) };
  t.parent = parent as *const FnTemplate;
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__PrototypeTemplate(
  this: *const FunctionTemplate,
) -> *const ObjectTemplate {
  if this.is_null() {
    return ptr::null();
  }
  let t = unsafe { &*(this as *const FnTemplate) };
  t.proto as *const ObjectTemplate
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__InstanceTemplate(
  this: *const FunctionTemplate,
) -> *const ObjectTemplate {
  if this.is_null() {
    return ptr::null();
  }
  let t = unsafe { &*(this as *const FnTemplate) };
  t.instance as *const ObjectTemplate
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__SetClassName(
  this: *const FunctionTemplate,
  name: *const String,
) {
  if this.is_null() || name.is_null() {
    return;
  }
  let t = unsafe { &mut *(this as *mut FnTemplate) };
  let ctx = current_ctx();
  if ctx.is_null() {
    return;
  }
  let mut len: usize = 0;
  let s = unsafe { JS_ToCStringLen(ctx, &mut len, jsval_of(name)) };
  if s.is_null() {
    return;
  }
  let bytes = unsafe { std::slice::from_raw_parts(s as *const u8, len) };
  if let Ok(name) = std::str::from_utf8(bytes) {
    t.class_name = Some(name.to_owned());
  }
  unsafe { JS_FreeCString(ctx, s) };
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__New(
  isolate: *mut RealIsolate,
  templ: *const FunctionTemplate,
) -> *const ObjectTemplate {
  let _ = isolate;
  let t = Box::into_raw(Box::new(ObjTemplate {
    internal_field_count: 0,
    props: Vec::new(),
    accessors: Vec::new(),
    named_handler: None,
    immutable_proto: false,
    parent_fn: templ as *const FnTemplate,
  }));
  register_template(t as usize, TemplKind::Obj);
  t as *const ObjectTemplate
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__NewInstance(
  this: *const ObjectTemplate,
  context: *const Context,
) -> *const Object {
  let ctx = ctx_of(context);
  if ctx.is_null() {
    return ptr::null();
  }
  let _tm = timing::now();
  let obj = unsafe { JS_NewObject(ctx) };
  if !this.is_null() {
    let t = unsafe { &*(this as *const ObjTemplate) };
    super::object::set_internal_field_count_for_value(
      obj,
      t.internal_field_count,
    );
    if !t.parent_fn.is_null() {
      let _ = v8__FunctionTemplate__GetFunction(
        t.parent_fn as *const FunctionTemplate,
        context,
      );
      let proto_obj = unsafe { build_prototype_object(ctx, t.parent_fn) };
      let dup = unsafe { JS_DupValue(ctx, proto_obj) };
      unsafe { JS_SetPrototype(ctx, obj, dup) };
      unsafe { JS_FreeValue(ctx, dup) };
    }
    apply_props(ctx, obj, &t.props);
    apply_accessors(ctx, obj, &t.accessors);
    if t.immutable_proto {
      unsafe { JS_PreventExtensions(ctx, obj) };
    }
  }
  timing::add(&timing::NEWINST_N, &timing::NEWINST_T, _tm);
  intern::<Object>(obj)
}

fn install_named_global_handler(
  ctx: *mut JSContext,
  global: JSValue,
  handler: &NamedHandler,
) {
  if handler.getter.is_none() || jsv_is_undefined(&handler.data) {
    return;
  }

  let mut tab: *mut JSPropertyEnum = ptr::null_mut();
  let mut len: u32 = 0;
  let rc = unsafe {
    JS_GetOwnPropertyNames(
      ctx,
      &mut tab,
      &mut len,
      handler.data,
      JS_GPN_STRING_MASK_QJS,
    )
  };
  if rc < 0 {
    return;
  }

  for i in 0..len {
    let atom = unsafe { (*tab.add(i as usize)).atom };
    let has = unsafe { JS_HasProperty(ctx, global, atom) };
    if has != 0 {
      continue;
    }

    let magic = register_named_getter(ctx, handler, atom);
    let getter = unsafe {
      make_cfunc_magic(
        ctx,
        named_getter_trampoline,
        ptr::null(),
        0,
        JS_CFUNC_GENERIC_MAGIC,
        magic,
      )
    };
    unsafe {
      JS_DefinePropertyGetSet(
        ctx,
        global,
        atom,
        getter,
        jsv_undefined(),
        JS_PROP_CONFIGURABLE | JS_PROP_ENUMERABLE,
      );
    }
  }

  unsafe { JS_FreePropertyEnum(ctx, tab, len) };
}

pub(crate) fn apply_object_template_to_global(
  ctx: *mut JSContext,
  global: JSValue,
  templ: *const ObjectTemplate,
) {
  if ctx.is_null() || templ.is_null() {
    return;
  }
  let t = unsafe { &*(templ as *const ObjTemplate) };
  super::object::set_internal_field_count_for_value(
    global,
    t.internal_field_count,
  );
  apply_props(ctx, global, &t.props);
  apply_accessors(ctx, global, &t.accessors);
  if let Some(handler) = &t.named_handler {
    install_named_global_handler(ctx, global, handler);
  }
  if t.immutable_proto {
    unsafe { JS_PreventExtensions(ctx, global) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__SetAccessorProperty(
  this: *const ObjectTemplate,
  key: *const Name,
  getter: *const FunctionTemplate,
  setter: *const FunctionTemplate,
  attr: PropertyAttribute,
) {
  if this.is_null() {
    return;
  }
  let t = unsafe { &mut *(this as *mut ObjTemplate) };
  // Own the key for the accessor's lifetime — see `own_template_value` in
  // `v8__Template__Set`: the borrowed handle would otherwise dangle.
  let key_owned = own_template_value(current_ctx(), jsval_of(key));
  t.accessors.push(TemplAccessor {
    key: key_owned,
    getter: getter as *const FnTemplate,
    setter: setter as *const FnTemplate,
    attr: attr.as_u32_lenient(),
  });
}

fn apply_accessors(
  ctx: *mut JSContext,
  obj: JSValue,
  accessors: &[TemplAccessor],
) {
  if accessors.is_empty() {
    return;
  }
  unsafe {
    let global = JS_GetGlobalObject(ctx);
    let object_ctor = JS_GetPropertyStr(ctx, global, c"Object".as_ptr());
    JS_FreeValue(ctx, global);
    if !jsv_is_object(&object_ctor) {
      JS_FreeValue(ctx, object_ctor);
      return;
    }
    let define =
      JS_GetPropertyStr(ctx, object_ctor, c"defineProperty".as_ptr());
    if !jsv_is_object(&define) {
      JS_FreeValue(ctx, define);
      JS_FreeValue(ctx, object_ctor);
      return;
    }

    for acc in accessors {
      if jsv_is_undefined(&acc.key) {
        continue;
      }
      let desc = JS_NewObject(ctx);
      if !acc.getter.is_null() {
        let gf = v8__FunctionTemplate__GetFunction(
          acc.getter as *const FunctionTemplate,
          ctx as *const Context,
        );
        if !gf.is_null() {
          let v = JS_DupValue(ctx, jsval_of(gf));
          JS_SetPropertyStr(ctx, desc, c"get".as_ptr(), v);
        }
      }
      if !acc.setter.is_null() {
        let sf = v8__FunctionTemplate__GetFunction(
          acc.setter as *const FunctionTemplate,
          ctx as *const Context,
        );
        if !sf.is_null() {
          let v = JS_DupValue(ctx, jsval_of(sf));
          JS_SetPropertyStr(ctx, desc, c"set".as_ptr(), v);
        }
      }

      let enumerable = (acc.attr & 2) == 0;
      let configurable = (acc.attr & 4) == 0;
      JS_SetPropertyStr(
        ctx,
        desc,
        c"enumerable".as_ptr(),
        jsv_bool(enumerable),
      );
      JS_SetPropertyStr(
        ctx,
        desc,
        c"configurable".as_ptr(),
        jsv_bool(configurable),
      );

      let mut args = [JS_DupValue(ctx, obj), JS_DupValue(ctx, acc.key), desc];
      let r = JS_Call(ctx, define, object_ctor, 3, args.as_mut_ptr());
      JS_FreeValue(ctx, args[0]);
      JS_FreeValue(ctx, args[1]);
      JS_FreeValue(ctx, args[2]);
      if !jsv_is_exception(&r) {
        JS_FreeValue(ctx, r);
      }
    }

    JS_FreeValue(ctx, define);
    JS_FreeValue(ctx, object_ctor);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__SetIndexedPropertyHandler(
  this: *const ObjectTemplate,
  getter: Option<crate::IndexedPropertyGetterCallback>,
  setter: Option<crate::IndexedPropertySetterCallback>,
  query: Option<crate::IndexedPropertyQueryCallback>,
  deleter: Option<crate::IndexedPropertyDeleterCallback>,
  enumerator: Option<crate::IndexedPropertyEnumeratorCallback>,
  definer: Option<crate::IndexedPropertyDefinerCallback>,
  descriptor: Option<crate::IndexedPropertyDescriptorCallback>,
  data_or_null: *const Value,
  flags: crate::PropertyHandlerFlags,
) {
  let _ = (
    this,
    getter,
    setter,
    query,
    deleter,
    enumerator,
    definer,
    descriptor,
    data_or_null,
    flags,
  );
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
  t.internal_field_count = value;
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__SetNamedPropertyHandler(
  this: *const ObjectTemplate,
  getter: Option<crate::NamedPropertyGetterCallback>,
  setter: Option<crate::NamedPropertySetterCallback>,
  query: Option<crate::NamedPropertyQueryCallback>,
  deleter: Option<crate::NamedPropertyDeleterCallback>,
  enumerator: Option<crate::NamedPropertyEnumeratorCallback>,
  definer: Option<crate::NamedPropertyDefinerCallback>,
  descriptor: Option<crate::NamedPropertyDescriptorCallback>,
  data_or_null: *const Value,
  flags: crate::PropertyHandlerFlags,
) {
  let _ = (
    setter, query, deleter, enumerator, definer, descriptor, flags,
  );
  if this.is_null() {
    return;
  }
  let data = if data_or_null.is_null() {
    jsv_undefined()
  } else {
    own_template_value(current_ctx(), jsval_of(data_or_null))
  };
  let t = unsafe { &mut *(this as *mut ObjTemplate) };
  t.named_handler = Some(NamedHandler {
    getter,
    data,
    owner_ctx: current_ctx(),
  });
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyCallbackInfo__GetIsolate(
  this: *const c_void,
) -> *mut RealIsolate {
  unsafe { prop_cbinfo(this) }
    .map(|info| info.isolate)
    .unwrap_or_else(current_iso)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyCallbackInfo__GetReturnValue(
  this: *const c_void,
) -> usize {
  if this.is_null() {
    return property_return_scratch();
  }
  let Some(info) = (unsafe { prop_cbinfo(this) }) else {
    return property_return_scratch();
  };
  (&mut *info.return_slot as *mut JSValue) as usize
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyCallbackInfo__Holder(
  this: *const c_void,
) -> *const Object {
  if this.is_null() {
    return ptr::null();
  }
  let Some(info) = (unsafe { prop_cbinfo(this) }) else {
    return ptr::null();
  };
  intern_dup::<Object>(info.ctx, info.this)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyCallbackInfo__ShouldThrowOnError(
  _this: *const c_void,
) -> bool {
  false
}

// ---------------------------------------------------------------------------
// Link-stubs for v8 C-ABI symbols that `test_api.rs` references but this
// backend doesn't implement yet. Each returns a benign default
// (null / 0 / false / `Nothing`) so the target LINKS and the many tests that
// don't touch these paths run; tests that do exercise them fail gracefully
// without crashing. Promote individual stubs to real implementations over time.
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionCallbackInfo__IsConstructCall(
  this: *const std::os::raw::c_void,
) -> bool {
  if this.is_null() {
    return false;
  }
  cbinfo(this as *const FunctionCallbackInfo).is_construct
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FunctionTemplate__SetAccessorProperty(
  this: *const std::os::raw::c_void,
  key: *const std::os::raw::c_void,
  getter: *const std::os::raw::c_void,
  setter: *const std::os::raw::c_void,
  attr: crate::PropertyAttribute,
) {
  if this.is_null() {
    return;
  }
  let t = unsafe { &mut *(this as *mut FnTemplate) };
  let key_owned = own_template_value(current_ctx(), jsval_of(key));
  t.accessors.push(TemplAccessor {
    key: key_owned,
    getter: getter as *const FnTemplate,
    setter: setter as *const FnTemplate,
    attr: attr.as_u32_lenient(),
  });
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__GetName(
  this: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  let ctx = current_ctx();
  if ctx.is_null() {
    return ptr::null();
  }
  if this.is_null() {
    let empty = unsafe { JS_NewStringLen(ctx, c"".as_ptr(), 0) };
    return intern::<String>(empty) as *const std::os::raw::c_void;
  }

  let name = unsafe {
    JS_GetPropertyStr(ctx, jsval_of(this as *const Function), c"name".as_ptr())
  };
  if jsv_is_string(&name) {
    return intern::<String>(name) as *const std::os::raw::c_void;
  }
  unsafe { JS_FreeValue(ctx, name) };
  let empty = unsafe { JS_NewStringLen(ctx, c"".as_ptr(), 0) };
  intern::<String>(empty) as *const std::os::raw::c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__GetScriptColumnNumber(
  _this: *const std::os::raw::c_void,
) -> crate::support::int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__GetScriptLineNumber(
  _this: *const std::os::raw::c_void,
) -> crate::support::int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__GetScriptOrigin(
  _this: *const std::os::raw::c_void,
  _out: *mut std::os::raw::c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Function__ScriptId(
  _this: *const std::os::raw::c_void,
) -> crate::support::int {
  0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__InternalFieldCount(
  this: *const std::os::raw::c_void,
) -> crate::support::int {
  if this.is_null() {
    return 0;
  }
  let t = unsafe { &*(this as *const ObjTemplate) };
  t.internal_field_count
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__SetImmutableProto(
  this: *const std::os::raw::c_void,
) {
  if this.is_null() {
    return;
  }
  let t = unsafe { &mut *(this as *mut ObjTemplate) };
  t.immutable_proto = true;
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ObjectTemplate__SetNativeDataProperty(
  _this: *const std::os::raw::c_void,
  _key: *const std::os::raw::c_void,
  _getter: *const std::os::raw::c_void,
  _setter: *const std::os::raw::c_void,
  _data_or_null: *const std::os::raw::c_void,
  _attr: crate::PropertyAttribute,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__PropertyCallbackInfo__Data(
  this: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  let Some(info) = (unsafe { prop_cbinfo(this) }) else {
    return std::ptr::null();
  };
  intern_dup::<Value>(info.ctx, info.data) as *const std::os::raw::c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__Get(
  this: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  let slot = unsafe { rv_slot(this as *mut RawReturnValue) };
  if slot.is_null() {
    return ptr::null();
  }
  let value = unsafe { *slot };
  intern_dup::<Value>(current_ctx(), value) as *const std::os::raw::c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ReturnValue__Value__SetEmptyString(
  this: *mut std::os::raw::c_void,
) {
  let slot = unsafe { rv_slot(this as *mut RawReturnValue) };
  let ctx = current_ctx();
  if !slot.is_null() && !ctx.is_null() {
    unsafe {
      *slot = JS_NewStringLen(ctx, c"".as_ptr(), 0);
    }
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Signature__New(
  _isolate: *mut std::os::raw::c_void,
  _templ: *const std::os::raw::c_void,
) -> *const std::os::raw::c_void {
  std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Template__SetIntrinsicDataProperty(
  _this: *const std::os::raw::c_void,
  _key: *const std::os::raw::c_void,
  _intrinsic: crate::Intrinsic,
  _attr: crate::PropertyAttribute,
) {
}
