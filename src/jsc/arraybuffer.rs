#![allow(non_snake_case, unused)]

use crate::jsc::core::{
  ctx_of, current_ctx, current_iso, intern, intern_ctx, iso_state, jsval,
};
use crate::jsc::jsc_sys::*;
use crate::support::{
  Maybe, MaybeBool, SharedPtrBase, SharedRef, UniquePtr, long,
};
use crate::{
  Allocator, ArrayBuffer, ArrayBufferView, BackingStore,
  BackingStoreDeleterCallback, Context, DataView, RealIsolate,
  SharedArrayBuffer, Uint8ClampedArray, Value,
};
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};

#[repr(C)]
#[derive(Copy, Clone)]
struct MemorySpan {
  data: *mut u8,
  size: usize,
}

struct BsInner {
  refcount: AtomicUsize,
  data: *mut c_void,
  byte_length: usize,
  is_shared: bool,

  deleter: BackingStoreDeleterCallback,
  deleter_data: *mut c_void,

  owns_malloc: bool,
}

unsafe extern "C" {
  fn malloc(size: usize) -> *mut c_void;
  fn calloc(count: usize, size: usize) -> *mut c_void;
  fn free(ptr: *mut c_void);
}

unsafe extern "C" fn noop_deleter(
  _data: *mut c_void,
  _len: usize,
  _deleter_data: *mut c_void,
) {
}

impl BsInner {
  fn boxed(
    data: *mut c_void,
    byte_length: usize,
    is_shared: bool,
    deleter: BackingStoreDeleterCallback,
    deleter_data: *mut c_void,
    owns_malloc: bool,
  ) -> *mut BsInner {
    Box::into_raw(Box::new(BsInner {
      refcount: AtomicUsize::new(1),
      data,
      byte_length,
      is_shared,
      deleter,
      deleter_data,
      owns_malloc,
    }))
  }

  fn new_allocated(byte_length: usize, is_shared: bool) -> *mut BsInner {
    let data = if byte_length == 0 {
      ptr::null_mut()
    } else {
      unsafe { calloc(byte_length, 1) }
    };
    BsInner::boxed(
      data,
      byte_length,
      is_shared,
      noop_deleter,
      ptr::null_mut(),
      true,
    )
  }

  unsafe fn destroy(ptr: *mut BsInner) {
    if ptr.is_null() {
      return;
    }
    let b = unsafe { Box::from_raw(ptr) };
    if !b.data.is_null() {
      if b.owns_malloc {
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

fn backing_store_for_buffer(
  ctx: JSContextRef,
  buf: JSValueRef,
) -> SharedRef<BackingStore> {
  let obj = buf as JSObjectRef;
  let (data, len) = unsafe {
    (
      JSObjectGetArrayBufferBytesPtr(ctx, obj, ptr::null_mut()),
      JSObjectGetArrayBufferByteLength(ctx, obj, ptr::null_mut()),
    )
  };
  let inner =
    BsInner::boxed(data, len, false, noop_deleter, ptr::null_mut(), false);
  make_shared_ref(inner)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__New__with_byte_length(
  isolate: *mut RealIsolate,
  byte_length: usize,
) -> *const ArrayBuffer {
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  if ctx.is_null() {
    return ptr::null();
  }

  let data = if byte_length == 0 {
    ptr::null_mut()
  } else {
    unsafe { calloc(byte_length, 1) }
  };
  unsafe extern "C" fn dealloc(bytes: *mut c_void, _ctx: *mut c_void) {
    if !bytes.is_null() {
      unsafe { free(bytes) };
    }
  }
  let obj = unsafe {
    JSObjectMakeArrayBufferWithBytesNoCopy(
      ctx,
      data,
      byte_length,
      Some(dealloc),
      ptr::null_mut(),
      ptr::null_mut(),
    )
  };
  intern_ctx::<ArrayBuffer>(ctx, obj as JSValueRef)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__New__with_backing_store(
  isolate: *mut RealIsolate,
  backing_store: *const SharedRef<BackingStore>,
) -> *const ArrayBuffer {
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  if ctx.is_null() || backing_store.is_null() {
    return ptr::null();
  }
  let inner = sp_get(backing_store as *const SharedPtrBase<BackingStore>);
  if inner.is_null() {
    return ptr::null();
  }
  let (data, len) = unsafe { ((*inner).data, (*inner).byte_length) };

  unsafe { (*inner).refcount.fetch_add(1, Ordering::SeqCst) };
  unsafe extern "C" fn dealloc(_bytes: *mut c_void, ctx: *mut c_void) {
    let inner = ctx as *mut BsInner;
    if inner.is_null() {
      return;
    }
    if unsafe { (*inner).refcount.fetch_sub(1, Ordering::SeqCst) } == 1 {
      unsafe { BsInner::destroy(inner) };
    }
  }
  let obj = unsafe {
    JSObjectMakeArrayBufferWithBytesNoCopy(
      ctx,
      data,
      len,
      Some(dealloc),
      inner as *mut c_void,
      ptr::null_mut(),
    )
  };
  intern_ctx::<ArrayBuffer>(ctx, obj as JSValueRef)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__ByteLength(
  this: *const ArrayBuffer,
) -> usize {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return 0;
  }
  unsafe {
    JSObjectGetArrayBufferByteLength(
      ctx,
      jsval(this) as JSObjectRef,
      ptr::null_mut(),
    )
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__Data(
  this: *const ArrayBuffer,
) -> *mut c_void {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null_mut();
  }
  unsafe {
    JSObjectGetArrayBufferBytesPtr(
      ctx,
      jsval(this) as JSObjectRef,
      ptr::null_mut(),
    )
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__IsDetachable(
  this: *const ArrayBuffer,
) -> bool {
  !this.is_null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__WasDetached(
  this: *const ArrayBuffer,
) -> bool {
  false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__Detach(
  this: *const ArrayBuffer,
  key: *const Value,
) -> MaybeBool {
  let _ = (this, key);
  MaybeBool::JustTrue
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__GetBackingStore(
  this: *const ArrayBuffer,
) -> SharedRef<BackingStore> {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return make_shared_ref(BsInner::new_allocated(0, false));
  }
  backing_store_for_buffer(ctx, jsval(this))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__NewBackingStore__with_byte_length(
  isolate: *mut RealIsolate,
  byte_length: usize,
) -> *mut BackingStore {
  BsInner::new_allocated(byte_length, false) as *mut BackingStore
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__NewBackingStore__with_data(
  data: *mut c_void,
  byte_length: usize,
  deleter: BackingStoreDeleterCallback,
  deleter_data: *mut c_void,
) -> *mut BackingStore {
  BsInner::boxed(data, byte_length, false, deleter, deleter_data, false)
    as *mut BackingStore
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BackingStore__Data(
  this: *const BackingStore,
) -> *mut c_void {
  bs_inner(this).map_or(ptr::null_mut(), |b| b.data)
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
  if !inner.is_null() {
    if unsafe { (*inner).refcount.fetch_sub(1, Ordering::SeqCst) } == 1 {
      unsafe { BsInner::destroy(inner) };
    }
  }
  if !ptr.is_null() {
    sp_set(ptr, ptr::null_mut());
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn std__shared_ptr__v8__BackingStore__use_count(
  ptr: *const SharedPtrBase<BackingStore>,
) -> long {
  let inner = sp_get(ptr);
  if inner.is_null() {
    0
  } else {
    unsafe { (*inner).refcount.load(Ordering::SeqCst) as long }
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

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__Buffer(
  this: *const ArrayBufferView,
) -> *const ArrayBuffer {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null();
  }
  let buf = unsafe {
    JSObjectGetTypedArrayBuffer(
      ctx,
      jsval(this) as JSObjectRef,
      ptr::null_mut(),
    )
  };
  if buf.is_null() {
    return ptr::null();
  }
  intern_ctx::<ArrayBuffer>(ctx, buf as JSValueRef)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__Buffer__Data(
  this: *const ArrayBufferView,
) -> *mut c_void {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return ptr::null_mut();
  }

  let buf = unsafe {
    JSObjectGetTypedArrayBuffer(
      ctx,
      jsval(this) as JSObjectRef,
      ptr::null_mut(),
    )
  };
  if buf.is_null() {
    return ptr::null_mut();
  }
  unsafe { JSObjectGetArrayBufferBytesPtr(ctx, buf, ptr::null_mut()) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__ByteLength(
  this: *const ArrayBufferView,
) -> usize {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return 0;
  }
  unsafe {
    JSObjectGetTypedArrayByteLength(
      ctx,
      jsval(this) as JSObjectRef,
      ptr::null_mut(),
    )
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__ByteOffset(
  this: *const ArrayBufferView,
) -> usize {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return 0;
  }
  unsafe {
    JSObjectGetTypedArrayByteOffset(
      ctx,
      jsval(this) as JSObjectRef,
      ptr::null_mut(),
    )
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__TypedArray__Length(
  this: *const crate::TypedArray,
) -> usize {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return 0;
  }
  unsafe {
    JSObjectGetTypedArrayLength(
      ctx,
      jsval(this) as JSObjectRef,
      ptr::null_mut(),
    )
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__HasBuffer(
  this: *const ArrayBufferView,
) -> bool {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return false;
  }
  let buf = unsafe {
    JSObjectGetTypedArrayBuffer(
      ctx,
      jsval(this) as JSObjectRef,
      ptr::null_mut(),
    )
  };
  !buf.is_null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__GetContents(
  this: *const ArrayBufferView,
  storage: MemorySpan,
) -> MemorySpan {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return MemorySpan {
      data: ptr::null_mut(),
      size: 0,
    };
  }
  let obj = jsval(this) as JSObjectRef;
  // Compute the data pointer as ArrayBuffer base + byteOffset rather than
  // trusting JSObjectGetTypedArrayBytesPtr: for a typed array that is a VIEW
  // over an existing (e.g. pooled) ArrayBuffer, JSC returns the buffer base
  // (byteOffset NOT applied), so ops read from offset 0. Node's Buffer pools
  // many small buffers into one ArrayBuffer at increasing byteOffsets, so this
  // made every non-first pooled Buffer decode read stale bytes (e.g.
  // `Buffer.from("foo")` => "Hel"). Base + byteOffset is correct regardless.
  let (off, len) = unsafe {
    (
      JSObjectGetTypedArrayByteOffset(ctx, obj, ptr::null_mut()),
      JSObjectGetTypedArrayByteLength(ctx, obj, ptr::null_mut()),
    )
  };
  // Prefer ArrayBuffer base + byteOffset (correct for pooled VIEWS, where
  // JSObjectGetTypedArrayBytesPtr drops the offset). Fall back to the typed
  // array data pointer when the view has no materializable ArrayBuffer (e.g.
  // DataView / detached); that pointer already accounts for any offset.
  let base = unsafe {
    let buffer = JSObjectGetTypedArrayBuffer(ctx, obj, ptr::null_mut());
    if buffer.is_null() {
      ptr::null_mut()
    } else {
      JSObjectGetArrayBufferBytesPtr(ctx, buffer, ptr::null_mut())
    }
  };
  let data = if !base.is_null() {
    unsafe { (base as *mut u8).add(off) }
  } else {
    unsafe {
      JSObjectGetTypedArrayBytesPtr(ctx, obj, ptr::null_mut()) as *mut u8
    }
  };
  if data.is_null() {
    return MemorySpan {
      data: ptr::null_mut(),
      size: 0,
    };
  }
  MemorySpan { data, size: len }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SharedArrayBuffer__New__with_backing_store(
  isolate: *mut RealIsolate,
  backing_store: *const SharedRef<BackingStore>,
) -> *const SharedArrayBuffer {
  let st = iso_state(isolate);
  let ctx =
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
  if ctx.is_null() || backing_store.is_null() {
    return ptr::null();
  }
  let inner = sp_get(backing_store as *const SharedPtrBase<BackingStore>);
  if inner.is_null() {
    return ptr::null();
  }
  let (data, len) = unsafe { ((*inner).data, (*inner).byte_length) };
  unsafe { (*inner).refcount.fetch_add(1, Ordering::SeqCst) };
  unsafe extern "C" fn dealloc(_bytes: *mut c_void, ctx: *mut c_void) {
    let inner = ctx as *mut BsInner;
    if inner.is_null() {
      return;
    }
    if unsafe { (*inner).refcount.fetch_sub(1, Ordering::SeqCst) } == 1 {
      unsafe { BsInner::destroy(inner) };
    }
  }
  let obj = unsafe {
    JSObjectMakeArrayBufferWithBytesNoCopy(
      ctx,
      data,
      len,
      Some(dealloc),
      inner as *mut c_void,
      ptr::null_mut(),
    )
  };
  intern_ctx::<SharedArrayBuffer>(ctx, obj as JSValueRef)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SharedArrayBuffer__GetBackingStore(
  this: *const SharedArrayBuffer,
) -> SharedRef<BackingStore> {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return make_shared_ref(BsInner::new_allocated(0, true));
  }

  let mut sref = backing_store_for_buffer(ctx, jsval(this));
  let inner = sp_get(
    &sref as *const SharedRef<BackingStore>
      as *const SharedPtrBase<BackingStore>,
  );
  if !inner.is_null() {
    unsafe { (*inner).is_shared = true };
  }
  sref
}

#[inline]
fn make_typed_array(
  buf: *const ArrayBuffer,
  byte_offset: usize,
  length: usize,
  ty: JSTypedArrayType,
) -> JSValueRef {
  let ctx = current_ctx();
  if ctx.is_null() || buf.is_null() {
    return ptr::null();
  }
  let obj = unsafe {
    JSObjectMakeTypedArrayWithArrayBufferAndOffset(
      ctx,
      ty,
      jsval(buf) as JSObjectRef,
      byte_offset,
      length,
      ptr::null_mut(),
    )
  };
  obj as JSValueRef
}

macro_rules! typed_array_new {
  ($fn_name:ident, $ty_name:ident, $jsc_ty:expr) => {
    #[unsafe(no_mangle)]
    pub extern "C" fn $fn_name(
      buf_ptr: *const ArrayBuffer,
      byte_offset: usize,
      length: usize,
    ) -> *const crate::$ty_name {
      let v = make_typed_array(buf_ptr, byte_offset, length, $jsc_ty);
      intern::<crate::$ty_name>(v)
    }
  };
}

typed_array_new!(v8__Uint8Array__New, Uint8Array, kJSTypedArrayTypeUint8Array);
typed_array_new!(v8__Int8Array__New, Int8Array, kJSTypedArrayTypeInt8Array);
typed_array_new!(
  v8__Uint16Array__New,
  Uint16Array,
  kJSTypedArrayTypeUint16Array
);
typed_array_new!(v8__Int16Array__New, Int16Array, kJSTypedArrayTypeInt16Array);
typed_array_new!(
  v8__Uint32Array__New,
  Uint32Array,
  kJSTypedArrayTypeUint32Array
);
typed_array_new!(v8__Int32Array__New, Int32Array, kJSTypedArrayTypeInt32Array);
typed_array_new!(
  v8__Float32Array__New,
  Float32Array,
  kJSTypedArrayTypeFloat32Array
);
typed_array_new!(
  v8__Float64Array__New,
  Float64Array,
  kJSTypedArrayTypeFloat64Array
);
typed_array_new!(
  v8__BigInt64Array__New,
  BigInt64Array,
  kJSTypedArrayTypeBigInt64Array
);
typed_array_new!(
  v8__BigUint64Array__New,
  BigUint64Array,
  kJSTypedArrayTypeBigUint64Array
);

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__SetDetachKey(
  _this: *const ArrayBuffer,
  _key: *const Value,
) {
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__CopyContents(
  this: *const ArrayBufferView,
  dest: *mut c_void,
  byte_length: crate::support::int,
) -> usize {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() || dest.is_null() || byte_length <= 0 {
    return 0;
  }
  let obj = jsval(this) as JSObjectRef;
  let (ptr_bytes, len) = unsafe {
    (
      JSObjectGetTypedArrayBytesPtr(ctx, obj, ptr::null_mut()),
      JSObjectGetTypedArrayByteLength(ctx, obj, ptr::null_mut()),
    )
  };
  if ptr_bytes.is_null() {
    return 0;
  }
  let n = len.min(byte_length as usize);
  unsafe {
    ptr::copy_nonoverlapping(ptr_bytes as *const u8, dest as *mut u8, n);
  }
  n
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__DataView__New(
  arraybuffer: *const ArrayBuffer,
  byte_offset: usize,
  length: usize,
) -> *const DataView {
  let ctx = current_ctx();
  if ctx.is_null() || arraybuffer.is_null() {
    return ptr::null();
  }

  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let src = b"(function(buf,off,len){return new DataView(buf,off,len);})\0";
    let fs = JSStringCreateWithUTF8CString(
      src.as_ptr() as *const std::os::raw::c_char
    );
    let fnv =
      JSEvaluateScript(ctx, fs, ptr::null_mut(), ptr::null_mut(), 1, &mut exc);
    JSStringRelease(fs);
    if !exc.is_null() {
      return ptr::null();
    }
    let fnobj = JSValueToObject(ctx, fnv, &mut exc);
    if fnobj.is_null() {
      return ptr::null();
    }
    let args = [
      jsval(arraybuffer),
      JSValueMakeNumber(ctx, byte_offset as f64),
      JSValueMakeNumber(ctx, length as f64),
    ];
    let v = JSObjectCallAsFunction(
      ctx,
      fnobj,
      ptr::null_mut(),
      3,
      args.as_ptr(),
      &mut exc,
    );
    if !exc.is_null() || v.is_null() {
      return ptr::null();
    }
    intern_ctx::<DataView>(ctx, v)
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__SharedArrayBuffer__ByteLength(
  this: *const SharedArrayBuffer,
) -> usize {
  let ctx = current_ctx();
  if ctx.is_null() || this.is_null() {
    return 0;
  }
  unsafe {
    JSObjectGetArrayBufferByteLength(
      ctx,
      jsval(this) as JSObjectRef,
      ptr::null_mut(),
    )
  }
}

typed_array_new!(
  v8__Uint8ClampedArray__New,
  Uint8ClampedArray,
  kJSTypedArrayTypeUint8ClampedArray
);

#[unsafe(no_mangle)]
pub extern "C" fn v8__Float16Array__New(
  buf_ptr: *const ArrayBuffer,
  byte_offset: usize,
  length: usize,
) -> *const crate::Float16Array {
  let ctx = current_ctx();
  if ctx.is_null() || buf_ptr.is_null() {
    return ptr::null();
  }
  unsafe {
    let mut exc: JSValueRef = ptr::null();
    let src =
      b"(function(buf,off,len){return new Float16Array(buf,off,len);})\0";
    let fs = JSStringCreateWithUTF8CString(
      src.as_ptr() as *const std::os::raw::c_char
    );
    let fnv =
      JSEvaluateScript(ctx, fs, ptr::null_mut(), ptr::null_mut(), 1, &mut exc);
    JSStringRelease(fs);
    if !exc.is_null() {
      return ptr::null();
    }
    let fnobj = JSValueToObject(ctx, fnv, &mut exc);
    if fnobj.is_null() {
      return ptr::null();
    }
    let args = [
      jsval(buf_ptr),
      JSValueMakeNumber(ctx, byte_offset as f64),
      JSValueMakeNumber(ctx, length as f64),
    ];
    let v = JSObjectCallAsFunction(
      ctx,
      fnobj,
      ptr::null_mut(),
      3,
      args.as_ptr(),
      &mut exc,
    );
    if !exc.is_null() || v.is_null() {
      return ptr::null();
    }
    intern_ctx::<crate::Float16Array>(ctx, v)
  }
}
