//! QuickJS-backed ArrayBuffer::Allocator placeholder shims.
//!
//! QuickJS manages ArrayBuffer memory internally, so the v8 allocator is a
//! stateless sentinel. Needed because `CreateParams::default()` builds a
//! default allocator shared_ptr. Mirrors the JSC backend's allocator.
#![allow(non_snake_case)]

use crate::Allocator;
use crate::support::{SharedPtrBase, UniquePtr, long};
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};

// The allocator `std::shared_ptr` is modelled with a real, atomically
// refcounted control block so that `clone()` / drop behave like C++'s
// shared_ptr — i.e. `use_count()` is accurate and the underlying allocator is
// freed exactly once (when the last reference drops). The previous
// implementation bitwise-copied the two words and freed on every `reset`, which
// (a) reported a bogus use_count of 1 and (b) double-freed when more than one
// `SharedRef`/`SharedPtr` pointed at the same allocator — an abort that took
// down the whole `test_api` binary (`backing_store_segfault` et al.).
//
// `SharedPtrBase<Allocator>` is `[usize; 2]`:
//   word[0] = the `*mut Allocator` object pointer (0 when null)
//   word[1] = a `*mut AtomicUsize` control block (0 when null)
fn read_words(ptr: *const SharedPtrBase<Allocator>) -> (usize, usize) {
  if ptr.is_null() {
    return (0, 0);
  }
  let w = ptr as *const usize;
  unsafe { (*w, *w.add(1)) }
}

unsafe fn write_words(ptr: *mut SharedPtrBase<Allocator>, obj: usize, ctrl: usize) {
  let w = ptr as *mut usize;
  unsafe {
    *w = obj;
    *w.add(1) = ctrl;
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__Allocator__NewDefaultAllocator()
-> *mut Allocator {
  Box::into_raw(Box::new(0u8)) as *mut Allocator
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__Allocator__DELETE(this: *mut Allocator) {
  if !this.is_null() {
    unsafe { drop(Box::from_raw(this as *mut u8)) };
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__ArrayBuffer__Allocator__COPY(
  ptr: *const SharedPtrBase<Allocator>,
) -> SharedPtrBase<Allocator> {
  let (obj, ctrl) = read_words(ptr);
  if ctrl != 0 {
    unsafe { (*(ctrl as *const AtomicUsize)).fetch_add(1, Ordering::Relaxed) };
  }
  let mut out: SharedPtrBase<Allocator> = Default::default();
  unsafe { write_words(&mut out, obj, ctrl) };
  out
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
  unsafe { write_words(&mut out, raw as usize, ctrl as usize) };
  out
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__ArrayBuffer__Allocator__get(
  ptr: *const SharedPtrBase<Allocator>,
) -> *mut Allocator {
  read_words(ptr).0 as *mut Allocator
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__ArrayBuffer__Allocator__reset(
  ptr: *mut SharedPtrBase<Allocator>,
) {
  if ptr.is_null() {
    return;
  }
  let (obj, ctrl) = read_words(ptr);
  if ctrl != 0 {
    // Decrement; free the allocator + control block only on the last reference.
    let prev =
      unsafe { (*(ctrl as *const AtomicUsize)).fetch_sub(1, Ordering::AcqRel) };
    if prev == 1 {
      if obj != 0 {
        v8__ArrayBuffer__Allocator__DELETE(obj as *mut Allocator);
      }
      unsafe { drop(Box::from_raw(ctrl as *mut AtomicUsize)) };
    }
  }
  unsafe { write_words(ptr, 0, 0) };
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__ArrayBuffer__Allocator__use_count(
  ptr: *const SharedPtrBase<Allocator>,
) -> long {
  let (obj, ctrl) = read_words(ptr);
  if ctrl != 0 {
    unsafe { (*(ctrl as *const AtomicUsize)).load(Ordering::Acquire) as long }
  } else if obj != 0 {
    1
  } else {
    0
  }
}
