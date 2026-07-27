//! cppgc__* (Oilpan) stubs for the Hermes backend.
//!
//! Cycle 0/1 scaffold only: Hermes has its own GC (Hades), so a real cppgc
//! bridge needs design work later (tracked alongside the rest of the object-
//! identity problem noted in docs/hermes-spike/NOTES.md). For now these are
//! safe no-ops with the exact signatures vendor/rusty_v8/src/cppgc.rs
//! declares, so `cppgc::Member`/`Persistent`/`Visitor` wrapper code compiles
//! and links. Anything beyond process init/shutdown panics if actually
//! invoked, since there is no backing heap yet.
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::os::raw::c_char;
use std::sync::{Mutex, OnceLock};

// ---- ICU trio (C8) --------------------------------------------------------
//
// The vendored rusty_v8 `icu` module declares three plain-C symbols with no
// `v8__` prefix, so tools/gen_hermes_shims.sh never stubs them and they went
// undefined at link time (blocking rv8_test_api/rv8_test_cppgc). Hermes brings
// its own Intl/ICU, so V8's ICU data blob is never actually loaded; these
// mirror the QuickJS backend's approach (src/quickjs/cli_extra.rs +
// src/quickjs/misc.rs): a process-global default locale string, and a
// header-only validation of the common-data blob. Pure Rust, no JSI, so they
// compile in both the stub (`hermes`) and real (`link_hermes`) builds.

fn locale_store() -> &'static Mutex<String> {
  static LOCALE: OnceLock<Mutex<String>> = OnceLock::new();
  LOCALE.get_or_init(|| Mutex::new("en-US".to_string()))
}

#[unsafe(no_mangle)]
pub extern "C" fn icu_get_default_locale(
  output: *mut c_char,
  output_len: usize,
) -> usize {
  let loc: Vec<u8> = match locale_store().lock() {
    Ok(g) => g.as_bytes().to_vec(),
    Err(_) => b"en-US".to_vec(),
  };
  if output.is_null() || output_len == 0 {
    return loc.len();
  }
  let n = loc.len().min(output_len.saturating_sub(1));
  unsafe {
    std::ptr::copy_nonoverlapping(loc.as_ptr() as *const c_char, output, n);
    *output.add(n) = 0;
  }
  n
}

#[unsafe(no_mangle)]
pub extern "C" fn icu_set_default_locale(locale: *const c_char) {
  if locale.is_null() {
    return;
  }
  let s = unsafe { std::ffi::CStr::from_ptr(locale) }
    .to_string_lossy()
    .into_owned();
  if let Ok(mut g) = locale_store().lock() {
    *g = s;
  }
}

// ICU common-data loader. Hermes never loads V8's blob, but the header magic is
// validated exactly like ICU's `udata_setCommonData`: a real icudtl.dat begins
// with a DataHeader whose bytes [2],[3] are 0xDA 0x27. A garbage blob gets
// U_INVALID_FORMAT_ERROR (3), matching the QuickJS backend and the vendored
// `icu::set_common_data_77` contract. Only the 4 header bytes every caller
// provides are read (no length crosses this C ABI).
#[unsafe(no_mangle)]
pub extern "C" fn udata_setCommonData_77(
  data: *const u8,
  error_code: *mut i32,
) {
  const U_INVALID_FORMAT_ERROR: i32 = 3;
  let valid = !data.is_null()
    && unsafe { *data.add(2) == 0xDA && *data.add(3) == 0x27 };
  if !error_code.is_null() {
    unsafe {
      *error_code = if valid { 0 } else { U_INVALID_FORMAT_ERROR };
    }
  }
}

// ---- TypedArray / ArrayBuffer link stubs (stub-hermes build only) ---------
//
// The real implementations live in src/hermes/core.rs (compiled only under
// `link_hermes`, since they call the JSI bridge). tools/gen_hermes_shims.sh
// cannot emit stubs for the `v8__<Name>Array__New` family (they are produced by
// the vendored `paste!` typed_array! macro, so the symbol name never appears as
// a whole token the generator's regex can match). So the stub-only `hermes`
// build needs these hand-written no-op stubs to link the rusty_v8 test targets.
// Under `link_hermes` the real core.rs definitions take over and these are
// cfg'd out to avoid duplicate symbols.
#[cfg(not(feature = "link_hermes"))]
macro_rules! typed_array_new_stub {
  ($name:ident) => {
    #[unsafe(no_mangle)]
    pub extern "C" fn $name() -> *const c_void { std::ptr::null() }
  };
}

// These four ALSO have real core.rs bodies (link_hermes) but, unlike the
// __New family, the generator DOES emit a stub for them (their names match its
// regex). It drops that stub once core.rs implements them, so the stub-only
// build needs the replacements here. Empty bodies, matching the generator's
// non-aborting stub convention.
#[cfg(not(feature = "link_hermes"))]
mod array_buffer_stubs {
  use std::ffi::c_void;
  #[unsafe(no_mangle)]
  pub extern "C" fn v8__V8__GetVersion() -> *const c_void {
    std::ptr::null()
  }
  #[unsafe(no_mangle)]
  pub extern "C" fn v8__Context__GetMicrotaskQueue() -> *const c_void {
    std::ptr::null()
  }
  #[unsafe(no_mangle)]
  pub extern "C" fn v8__Context__SetMicrotaskQueue() -> *const c_void {
    std::ptr::null()
  }
  #[unsafe(no_mangle)]
  pub extern "C" fn v8__ArrayBuffer__New__with_byte_length() -> *const c_void {
    std::ptr::null()
  }
  #[unsafe(no_mangle)]
  pub extern "C" fn v8__ArrayBuffer__ByteLength() -> *const c_void {
    std::ptr::null()
  }
  #[unsafe(no_mangle)]
  pub extern "C" fn v8__ArrayBuffer__Data() -> *const c_void {
    std::ptr::null()
  }
  #[unsafe(no_mangle)]
  pub extern "C" fn v8__TypedArray__Length() -> *const c_void {
    std::ptr::null()
  }
}

#[cfg(not(feature = "link_hermes"))]
mod typed_array_stubs {
  typed_array_new_stub!(v8__Uint8Array__New);
  typed_array_new_stub!(v8__Uint8ClampedArray__New);
  typed_array_new_stub!(v8__Int8Array__New);
  typed_array_new_stub!(v8__Uint16Array__New);
  typed_array_new_stub!(v8__Int16Array__New);
  typed_array_new_stub!(v8__Uint32Array__New);
  typed_array_new_stub!(v8__Int32Array__New);
  typed_array_new_stub!(v8__Float16Array__New);
  typed_array_new_stub!(v8__Float32Array__New);
  typed_array_new_stub!(v8__Float64Array__New);
  typed_array_new_stub!(v8__BigUint64Array__New);
  typed_array_new_stub!(v8__BigInt64Array__New);
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__initialize_process(_platform: *mut c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__shutdown_process() {}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__make_garbage_collectable(
  _heap: *mut c_void,
  _size: usize,
  _alignment: usize,
) -> *mut c_void {
  unimplemented!("cppgc__make_garbage_collectable")
}

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
pub extern "C" fn cppgc__Visitor__Trace__TracedReference(
  _visitor: *mut c_void,
  _reference: *const c_void,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Member__CONSTRUCT(
  member: *mut c_void,
  obj: *mut c_void,
) {
  if !member.is_null() {
    unsafe { std::ptr::write_unaligned(member as *mut *mut c_void, obj) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Member__DESTRUCT(_member: *mut c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Member__Get(member: *const c_void) -> *mut c_void {
  if member.is_null() {
    return std::ptr::null_mut();
  }
  unsafe { std::ptr::read_unaligned(member as *const *mut c_void) }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Member__Assign(member: *mut c_void, other: *mut c_void) {
  cppgc__Member__CONSTRUCT(member, other);
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__WeakMember__CONSTRUCT(
  member: *mut c_void,
  obj: *mut c_void,
) {
  cppgc__Member__CONSTRUCT(member, obj);
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__WeakMember__DESTRUCT(_member: *mut c_void) {}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__WeakMember__Get(
  member: *const c_void,
) -> *mut c_void {
  cppgc__Member__Get(member)
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__WeakMember__Assign(
  member: *mut c_void,
  other: *mut c_void,
) {
  cppgc__Member__CONSTRUCT(member, other);
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Persistent__CONSTRUCT(
  obj: *mut c_void,
) -> *mut c_void {
  Box::into_raw(Box::new(obj)) as *mut c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Persistent__DESTRUCT(this: *mut c_void) {
  if !this.is_null() {
    unsafe { drop(Box::from_raw(this as *mut *mut c_void)) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Persistent__Assign(this: *mut c_void, ptr: *mut c_void) {
  if !this.is_null() {
    unsafe { std::ptr::write_unaligned(this as *mut *mut c_void, ptr) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__Persistent__Get(this: *const c_void) -> *mut c_void {
  if this.is_null() {
    return std::ptr::null_mut();
  }
  unsafe { std::ptr::read_unaligned(this as *const *mut c_void) }
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__WeakPersistent__CONSTRUCT(
  obj: *mut c_void,
) -> *mut c_void {
  cppgc__Persistent__CONSTRUCT(obj)
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__WeakPersistent__DESTRUCT(this: *mut c_void) {
  cppgc__Persistent__DESTRUCT(this)
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__WeakPersistent__Assign(
  this: *mut c_void,
  ptr: *mut c_void,
) {
  cppgc__Persistent__Assign(this, ptr)
}

#[unsafe(no_mangle)]
pub extern "C" fn cppgc__WeakPersistent__Get(this: *const c_void) -> *mut c_void {
  cppgc__Persistent__Get(this)
}
