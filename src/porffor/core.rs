//! Real (non-stub) v8 C-ABI surface over the porffor embed ABI.
//!
//! Only compiled under `link_porffor`, when build.rs has actually run porffor
//! over `seed.js` and linked the emitted object. See `mod.rs` for why handles
//! are arena slots and what porffor being AOT costs us.
//!
//! ## One heap, one isolate
//!
//! Porffor emits a single global heap (`MEM`) per binary; there is no
//! per-isolate state to parameterise. So `RealIsolate` here is a Box whose
//! address is the handle, and creating a second live isolate is rejected rather
//! than silently sharing one heap between two of them.
#![allow(non_snake_case)]

use crate::support::MaybeBool;
use crate::{
  Boolean, Context, Data, Number, Object, ObjectTemplate, Primitive,
  RealIsolate, String as V8String, Value,
};
use std::cell::Cell;
use std::mem::MaybeUninit;
use std::os::raw::{c_char, c_int};
use std::ptr;

// ---------------------------------------------------------------------------
// The porffor embed ABI (see porffor's embed/porffor_embed.h).
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Jsval {
  pub val: f64,
  pub ty: i32,
}

// porffor type tags (compiler/types.js)
const T_UNDEFINED: i32 = 0x00;
const T_NUMBER: i32 = 0x01;
const T_BOOLEAN: i32 = 0x02;
const T_STRING: i32 = 0x43;
const T_FUNCTION: i32 = 0x06;
const T_OBJECT: i32 = 0x07;
const T_BYTESTRING: i32 = 0xC3;

unsafe extern "C" {
  fn porf_embed_init(argc: c_int, argv: *mut *mut c_char, stack_top: *mut c_void_);
  fn porf_embed_run_entry();

  fn porf_embed_undefined() -> Jsval;
  fn porf_embed_null() -> Jsval;
  fn porf_embed_number(d: f64) -> Jsval;
  fn porf_embed_boolean(b: i32) -> Jsval;
  fn porf_embed_number_value(v: Jsval) -> f64;
  fn porf_embed_truthy(v: Jsval) -> i32;
  fn porf_embed_strict_eq(a: Jsval, b: Jsval) -> i32;

  fn porf_embed_string_new(utf8: *const u8, len: i32) -> Jsval;
  fn porf_embed_string_len(v: Jsval) -> i32;
  fn porf_embed_string_utf8(v: Jsval, buf: *mut u8, cap: i32) -> i32;
  fn porf_embed_string_data(v: Jsval) -> *const c_void_;
  fn porf_embed_string_is_onebyte(v: Jsval) -> i32;

  fn porf_embed_object_new() -> Jsval;
  fn porf_embed_object_set(obj: Jsval, key: Jsval, value: Jsval);
  fn porf_embed_object_get(obj: Jsval, key: Jsval) -> Jsval;
  fn porf_embed_globalthis() -> Jsval;

  fn porf_embed_scope_enter() -> u64;
  fn porf_embed_scope_exit(depth: u64);
  fn porf_embed_handle_new(v: Jsval) -> *mut Jsval;

  fn porf_embed_global_new(v: Jsval) -> *mut Jsval;
  fn porf_embed_global_free(h: *mut Jsval);

  fn porf_embed_gc();
}

#[allow(non_camel_case_types)]
type c_void_ = std::ffi::c_void;

// ---------------------------------------------------------------------------
// Handles
// ---------------------------------------------------------------------------

/// Move a value into a fresh arena slot in the current scope; the slot address
/// is the v8 handle. Mirrors `intern` in the QuickJS backend, minus refcounts —
/// here the slot's job is to be a GC root, not to own a reference.
#[inline]
fn intern<T>(v: Jsval) -> *const T {
  unsafe { porf_embed_handle_new(v) as *const T }
}

/// Read the value back out of a handle slot.
#[inline]
fn jsval_of<T>(p: *const T) -> Jsval {
  if p.is_null() {
    return Jsval { val: 0.0, ty: T_UNDEFINED };
  }
  unsafe { *(p as *const Jsval) }
}

#[inline]
fn is_str(v: Jsval) -> bool {
  v.ty == T_STRING || v.ty == T_BYTESTRING
}

// ---------------------------------------------------------------------------
// Isolate
// ---------------------------------------------------------------------------

struct IsoState {
  /// innermost entered context handle, if any
  entered: Cell<*const Context>,
  initialized: Cell<bool>,
  /// embedder slots; rusty_v8 asserts there are at least
  /// `Isolate::INTERNAL_DATA_SLOT_COUNT` of these before it will build one
  data_slots: [Cell<*mut c_void_>; 4],
}

thread_local! {
  static CURRENT: Cell<*mut RealIsolate> = const { Cell::new(ptr::null_mut()) };
}

/// Porffor emits ONE global heap per binary, shared by every thread. Isolate
/// liveness is therefore a process-wide fact, not a per-thread one: two threads
/// each creating an "isolate" would both re-run `porf_embed_init` over the same
/// heap and corrupt it.
static LIVE: std::sync::atomic::AtomicBool =
  std::sync::atomic::AtomicBool::new(false);

#[inline]
fn iso_state<'a>(p: *mut RealIsolate) -> &'a IsoState {
  unsafe { &*(p as *const IsoState) }
}

// rusty_v8 asserts its own CreateParams layout against these before it will
// build one, so they must be real even in a backend that ignores the contents.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__CreateParams__SIZEOF() -> usize {
  std::mem::size_of::<crate::isolate_create_params::raw::CreateParams>()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__CreateParams__CONSTRUCT(
  buf: *mut MaybeUninit<crate::isolate_create_params::raw::CreateParams>,
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
pub extern "C" fn v8__Isolate__New(_params: *const c_void_) -> *mut RealIsolate {
  // Porffor compiles one global heap per binary, so two live isolates would
  // silently share it. Refuse rather than corrupt.
  if LIVE.swap(true, std::sync::atomic::Ordering::SeqCst) {
    panic!(
      "porffor backend supports one live isolate per process (it compiles a \
       single global heap); dispose the previous isolate first"
    );
  }

  let st = Box::into_raw(Box::new(IsoState {
    entered: Cell::new(ptr::null()),
    initialized: Cell::new(false),
    data_slots: [const { Cell::new(ptr::null_mut()) }; 4],
  })) as *mut RealIsolate;

  // The heap is process-wide and never torn down, so init runs exactly once.
  // The stack anchor it records bounds the conservative root scan, so it must
  // come from a frame that outlives every later call into porffor.
  static INIT: std::sync::Once = std::sync::Once::new();
  INIT.call_once(|| unsafe {
    let mut anchor: i32 = 0;
    porf_embed_init(0, ptr::null_mut(), &mut anchor as *mut i32 as *mut c_void_);
  });
  iso_state(st).initialized.set(true);
  CURRENT.with(|c| c.set(st));
  st
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__Dispose(this: *mut RealIsolate) {
  if this.is_null() {
    return;
  }
  CURRENT.with(|c| {
    if c.get() == this {
      c.set(ptr::null_mut())
    }
  });
  LIVE.store(false, std::sync::atomic::Ordering::SeqCst);
  drop(unsafe { Box::from_raw(this as *mut IsoState) });
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetCurrent() -> *mut RealIsolate {
  CURRENT.with(|c| c.get())
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
) -> *mut c_void_ {
  if isolate.is_null() {
    return ptr::null_mut();
  }
  iso_state(isolate as *mut RealIsolate)
    .data_slots
    .get(slot as usize)
    .map_or(ptr::null_mut(), Cell::get)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetData(
  isolate: *const RealIsolate,
  slot: u32,
  data: *mut c_void_,
) {
  if isolate.is_null() {
    return;
  }
  if let Some(s) = iso_state(isolate as *mut RealIsolate).data_slots.get(slot as usize) {
    s.set(data);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__RequestGarbageCollectionForTesting(
  _this: *mut RealIsolate,
  _ty: c_int,
) {
  unsafe { porf_embed_gc() }
}

// ---------------------------------------------------------------------------
// HandleScope
// ---------------------------------------------------------------------------
//
// The scope object is opaque to rusty_v8; it just gives us a buffer. Store the
// isolate and the arena depth, exactly as the QuickJS backend stores
// isolate + handles.len().

#[unsafe(no_mangle)]
pub extern "C" fn v8__HandleScope__CONSTRUCT(
  buf: *mut MaybeUninit<[usize; 2]>,
  isolate: *mut RealIsolate,
) {
  CURRENT.with(|c| c.set(isolate));
  let depth = unsafe { porf_embed_scope_enter() };
  unsafe {
    let slots = buf as *mut usize;
    *slots.offset(0) = isolate as usize;
    *slots.offset(1) = depth as usize;
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__HandleScope__DESTRUCT(this: *mut usize) {
  if this.is_null() {
    return;
  }
  unsafe {
    let depth = *this.offset(1) as u64;
    porf_embed_scope_exit(depth);
  }
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------
//
// Porffor has exactly one global object, so a context is not a separable thing
// here: every context handle refers to the same globalThis. Entering and
// exiting are bookkeeping only.

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__New(
  _isolate: *mut RealIsolate,
  _templ: *const ObjectTemplate,
  _global_object: *const Value,
  _microtask_queue: *mut c_void_,
) -> *const Context {
  intern::<Context>(unsafe { porf_embed_globalthis() })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__Enter(this: *const Context) {
  let iso = CURRENT.with(|c| c.get());
  if !iso.is_null() {
    iso_state(iso).entered.set(this);
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__Exit(_this: *const Context) {
  let iso = CURRENT.with(|c| c.get());
  if !iso.is_null() {
    iso_state(iso).entered.set(ptr::null());
  }
}

/// The innermost entered context. Porffor has a single global object, so if
/// nothing was explicitly entered this still hands back a usable context rather
/// than null — a null here trips a `NonNull::new_unchecked` inside the scope
/// machinery, which aborts the process instead of failing one call.
#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetCurrentContext(
  isolate: *mut RealIsolate,
) -> *const Context {
  if !isolate.is_null() {
    let entered = iso_state(isolate).entered.get();
    if !entered.is_null() {
      return entered;
    }
  }
  intern::<Context>(unsafe { porf_embed_globalthis() })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__GetIsolate(
  _this: *const Context,
) -> *mut RealIsolate {
  CURRENT.with(|c| c.get())
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__Global(_this: *const Context) -> *const Object {
  intern::<Object>(unsafe { porf_embed_globalthis() })
}

// ---------------------------------------------------------------------------
// Primitives
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__Number__New(
  _isolate: *mut RealIsolate,
  value: f64,
) -> *const Number {
  intern::<Number>(unsafe { porf_embed_number(value) })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Boolean__New(
  _isolate: *mut RealIsolate,
  value: bool,
) -> *const Boolean {
  intern::<Boolean>(unsafe { porf_embed_boolean(value as i32) })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Undefined(_isolate: *mut RealIsolate) -> *const Primitive {
  intern::<Primitive>(unsafe { porf_embed_undefined() })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Null(_isolate: *mut RealIsolate) -> *const Primitive {
  intern::<Primitive>(unsafe { porf_embed_null() })
}

// ---------------------------------------------------------------------------
// Value predicates / conversions
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsUndefined(this: *const Value) -> bool {
  jsval_of(this).ty == T_UNDEFINED
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsNull(this: *const Value) -> bool {
  let v = jsval_of(this);
  v.ty == T_OBJECT && v.val == 0.0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsNumber(this: *const Value) -> bool {
  jsval_of(this).ty == T_NUMBER
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsString(this: *const Value) -> bool {
  is_str(jsval_of(this))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsBoolean(this: *const Value) -> bool {
  jsval_of(this).ty == T_BOOLEAN
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsFunction(this: *const Value) -> bool {
  jsval_of(this).ty == T_FUNCTION
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__IsObject(this: *const Value) -> bool {
  let v = jsval_of(this);
  v.ty == T_OBJECT && v.val != 0.0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__StrictEquals(
  this: *const Value,
  that: *const Value,
) -> bool {
  unsafe { porf_embed_strict_eq(jsval_of(this), jsval_of(that)) != 0 }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__BooleanValue(
  this: *const Value,
  _isolate: *mut RealIsolate,
) -> bool {
  unsafe { porf_embed_truthy(jsval_of(this)) != 0 }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Value__NumberValue(
  this: *const Value,
  _context: *const Context,
  out: *mut MaybeF64,
) {
  let v = jsval_of(this);
  unsafe {
    (*out).has_value = true;
    (*out).value = porf_embed_number_value(v);
  }
}

/// Matches `Maybe<f64>`'s layout in the vendored binding.
#[repr(C)]
pub struct MaybeF64 {
  has_value: bool,
  value: f64,
}

// ---------------------------------------------------------------------------
// String
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewFromUtf8(
  _isolate: *mut RealIsolate,
  data: *const u8,
  _new_type: c_int,
  length: c_int,
) -> *const V8String {
  if data.is_null() {
    return ptr::null();
  }
  intern::<V8String>(unsafe { porf_embed_string_new(data, length) })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__NewFromOneByte(
  isolate: *mut RealIsolate,
  data: *const u8,
  new_type: c_int,
  length: c_int,
) -> *const V8String {
  v8__String__NewFromUtf8(isolate, data, new_type, length)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__Empty(
  _isolate: *mut RealIsolate,
) -> *const V8String {
  intern::<V8String>(unsafe { porf_embed_string_new(b"".as_ptr(), 0) })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__Length(this: *const V8String) -> c_int {
  let n = unsafe { porf_embed_string_len(jsval_of(this)) };
  if n < 0 { 0 } else { n }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__Utf8Length(
  this: *const V8String,
  _isolate: *mut RealIsolate,
) -> c_int {
  let v = jsval_of(this);
  if !is_str(v) {
    return 0;
  }
  // pass a zero-capacity buffer: the shim still returns the byte count needed
  unsafe { porf_embed_string_utf8(v, ptr::null_mut(), 0) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__WriteUtf8_v2(
  this: *const V8String,
  _isolate: *mut RealIsolate,
  buffer: *mut u8,
  capacity: usize,
  _flags: c_int,
  processed: *mut usize,
) -> c_int {
  let v = jsval_of(this);
  if !is_str(v) || buffer.is_null() {
    return 0;
  }
  let needed = unsafe { porf_embed_string_utf8(v, buffer, capacity as i32) };
  let written = if (needed as usize) < capacity {
    needed
  } else {
    capacity as c_int
  };
  if !processed.is_null() {
    unsafe { *processed = written as usize };
  }
  written
}

/// `ValueView` is rusty_v8's zero-copy window onto a string, and it is what
/// `to_rust_string_lossy` actually goes through. Porffor's collector does not
/// move objects and the caller holds a handle rooting the string, so the view
/// can point straight into the heap rather than copying.
///
/// The buffer rusty_v8 hands us is opaque; store the jsval in it and read the
/// payload back on demand.
#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ValueView__CONSTRUCT(
  buf: *mut Jsval,
  _isolate: *mut RealIsolate,
  string: *const V8String,
) {
  if !buf.is_null() {
    unsafe { *buf = jsval_of(string) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ValueView__DESTRUCT(_this: *mut Jsval) {}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ValueView__is_one_byte(
  this: *const Jsval,
) -> bool {
  if this.is_null() {
    return true;
  }
  unsafe { porf_embed_string_is_onebyte(*this) != 0 }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ValueView__data(
  this: *const Jsval,
) -> *const c_void_ {
  if this.is_null() {
    // never hand back null with a nonzero length: rusty_v8 builds a slice
    // straight from this pair
    return ptr::NonNull::<u8>::dangling().as_ptr() as *const c_void_;
  }
  let d = unsafe { porf_embed_string_data(*this) };
  if d.is_null() {
    return ptr::NonNull::<u8>::dangling().as_ptr() as *const c_void_;
  }
  d
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__String__ValueView__length(this: *const Jsval) -> c_int {
  if this.is_null() {
    return 0;
  }
  let n = unsafe { porf_embed_string_len(*this) };
  if n < 0 { 0 } else { n }
}

// ---------------------------------------------------------------------------
// Object
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__New(
  _isolate: *mut RealIsolate,
) -> *const Object {
  intern::<Object>(unsafe { porf_embed_object_new() })
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__Set(
  this: *const Object,
  _context: *const Context,
  key: *const Value,
  value: *const Value,
) -> MaybeBool {
  if this.is_null() || key.is_null() {
    return MaybeBool::Nothing;
  }
  unsafe { porf_embed_object_set(jsval_of(this), jsval_of(key), jsval_of(value)) };
  MaybeBool::JustTrue
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Object__Get(
  this: *const Object,
  _context: *const Context,
  key: *const Value,
) -> *const Value {
  if this.is_null() || key.is_null() {
    return ptr::null();
  }
  intern::<Value>(unsafe { porf_embed_object_get(jsval_of(this), jsval_of(key)) })
}

// ---------------------------------------------------------------------------
// Global<T>
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn v8__Global__New(
  _isolate: *mut RealIsolate,
  other: *const Data,
) -> *const Data {
  unsafe { porf_embed_global_new(jsval_of(other)) as *const Data }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Global__Reset(data: *const Data) {
  if !data.is_null() {
    unsafe { porf_embed_global_free(data as *mut Jsval) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Local__New(
  _isolate: *mut RealIsolate,
  other: *const Data,
) -> *const Data {
  intern::<Data>(jsval_of(other))
}

// ---------------------------------------------------------------------------
// Entry point for the AOT-compiled seed
// ---------------------------------------------------------------------------

/// Run whatever `seed.js` compiled to. Not a v8 symbol — porffor has no runtime
/// compile step, so this is how the baked-in program gets executed at all.
#[doc(hidden)]
pub fn run_compiled_entry() {
  unsafe { porf_embed_run_entry() }
}
