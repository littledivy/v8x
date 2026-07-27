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
