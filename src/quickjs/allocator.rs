//! QuickJS-backed ArrayBuffer::Allocator placeholder shims.
//!
//! QuickJS manages ArrayBuffer memory internally, so the v8 allocator is a
//! stateless sentinel. Needed because `CreateParams::default()` builds a
//! default allocator shared_ptr. Mirrors the JSC backend's allocator.
#![allow(non_snake_case)]

use crate::Allocator;
use crate::support::{SharedPtrBase, UniquePtr, long};
use std::ptr;

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
  if ptr.is_null() {
    return Default::default();
  }
  unsafe { ptr::read(ptr) }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__ArrayBuffer__Allocator__CONVERT__std__unique_ptr(
  unique_ptr: UniquePtr<Allocator>,
) -> SharedPtrBase<Allocator> {
  let raw = unique_ptr.into_raw();
  let mut out: SharedPtrBase<Allocator> = Default::default();
  unsafe {
    let words = &mut out as *mut SharedPtrBase<Allocator> as *mut usize;
    *words = raw as usize;
    *words.add(1) = 0;
  }
  out
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__ArrayBuffer__Allocator__get(
  ptr: *const SharedPtrBase<Allocator>,
) -> *mut Allocator {
  if ptr.is_null() {
    return ptr::null_mut();
  }
  unsafe { *(ptr as *const usize) as *mut Allocator }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__ArrayBuffer__Allocator__reset(
  ptr: *mut SharedPtrBase<Allocator>,
) {
  if ptr.is_null() {
    return;
  }
  unsafe {
    let words = ptr as *mut usize;
    let raw = *words as *mut Allocator;
    if !raw.is_null() {
      v8__ArrayBuffer__Allocator__DELETE(raw);
    }
    *words = 0;
    *words.add(1) = 0;
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__ArrayBuffer__Allocator__use_count(
  ptr: *const SharedPtrBase<Allocator>,
) -> long {
  if ptr.is_null() {
    return 0;
  }
  let raw = unsafe { *(ptr as *const usize) };
  if raw == 0 { 0 } else { 1 }
}
