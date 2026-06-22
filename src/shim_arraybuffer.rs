// Family: arraybuffer
// ArrayBuffer / ArrayBufferView / BackingStore / TypedArray / DataView /
// SharedArrayBuffer + typed-array `New` constructors, backed by JavaScriptCore.
#![allow(non_snake_case, unused)]

use crate::jsc_sys::*;
use crate::support::{Maybe, MaybeBool, SharedPtrBase, SharedRef, UniquePtr, long};
use crate::{
    Allocator, ArrayBuffer, ArrayBufferView, BackingStore, BackingStoreDeleterCallback, Context,
    DataView, RealIsolate, SharedArrayBuffer, Value,
};
use crate::shim_core::{ctx_of, current_ctx, current_iso, intern, intern_ctx, iso_state, jsval};
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};

// ===================================================================
// Extra JSC C API not present in jsc_sys.rs
// ===================================================================

// `JSTypedArrayType` from JSValueRef.h (C enum, i32-sized).
#[allow(non_camel_case_types)]
type JSTypedArrayType = u32;
const kJSTypedArrayTypeInt8Array: JSTypedArrayType = 0;
const kJSTypedArrayTypeInt16Array: JSTypedArrayType = 1;
const kJSTypedArrayTypeInt32Array: JSTypedArrayType = 2;
const kJSTypedArrayTypeUint8Array: JSTypedArrayType = 3;
const kJSTypedArrayTypeUint8ClampedArray: JSTypedArrayType = 4;
const kJSTypedArrayTypeUint16Array: JSTypedArrayType = 5;
const kJSTypedArrayTypeUint32Array: JSTypedArrayType = 6;
const kJSTypedArrayTypeFloat32Array: JSTypedArrayType = 7;
const kJSTypedArrayTypeFloat64Array: JSTypedArrayType = 8;
const kJSTypedArrayTypeArrayBuffer: JSTypedArrayType = 9;
const kJSTypedArrayTypeNone: JSTypedArrayType = 10;
const kJSTypedArrayTypeBigInt64Array: JSTypedArrayType = 11;
const kJSTypedArrayTypeBigUint64Array: JSTypedArrayType = 12;

#[allow(non_camel_case_types)]
type JSTypedArrayBytesDeallocator =
    Option<unsafe extern "C" fn(bytes: *mut c_void, deallocator_context: *mut c_void)>;

unsafe extern "C" {
    fn JSObjectMakeTypedArrayWithArrayBufferAndOffset(
        ctx: JSContextRef,
        arrayType: JSTypedArrayType,
        buffer: JSObjectRef,
        byteOffset: usize,
        length: usize,
        exception: *mut JSValueRef,
    ) -> JSObjectRef;
    fn JSObjectMakeArrayBufferWithBytesNoCopy(
        ctx: JSContextRef,
        bytes: *mut c_void,
        byteLength: usize,
        bytesDeallocator: JSTypedArrayBytesDeallocator,
        deallocatorContext: *mut c_void,
        exception: *mut JSValueRef,
    ) -> JSObjectRef;
    fn JSObjectGetArrayBufferBytesPtr(
        ctx: JSContextRef,
        object: JSObjectRef,
        exception: *mut JSValueRef,
    ) -> *mut c_void;
    fn JSObjectGetArrayBufferByteLength(
        ctx: JSContextRef,
        object: JSObjectRef,
        exception: *mut JSValueRef,
    ) -> usize;
    fn JSObjectGetTypedArrayBytesPtr(
        ctx: JSContextRef,
        object: JSObjectRef,
        exception: *mut JSValueRef,
    ) -> *mut c_void;
    fn JSObjectGetTypedArrayByteLength(
        ctx: JSContextRef,
        object: JSObjectRef,
        exception: *mut JSValueRef,
    ) -> usize;
    fn JSObjectGetTypedArrayByteOffset(
        ctx: JSContextRef,
        object: JSObjectRef,
        exception: *mut JSValueRef,
    ) -> usize;
    fn JSObjectGetTypedArrayBuffer(
        ctx: JSContextRef,
        object: JSObjectRef,
        exception: *mut JSValueRef,
    ) -> JSObjectRef;
    fn JSValueGetTypedArrayType(
        ctx: JSContextRef,
        value: JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSTypedArrayType;
}

// memory_span_t mirror (matches `crate::binding::memory_span_t`: { data: *mut u8, size: usize }).
#[repr(C)]
#[derive(Copy, Clone)]
struct MemorySpan {
    data: *mut u8,
    size: usize,
}

// ===================================================================
// BackingStore representation
//
// `*mut BackingStore` (a UniqueRef) and the `SharedPtrBase<BackingStore>`
// payload both point at a refcounted `BsInner`. The vendored `BackingStore`
// type is opaque to v8; it only ever round-trips the raw pointer and calls our
// accessors, so we are free to box our own structure behind it.
// ===================================================================

struct BsInner {
    refcount: AtomicUsize,
    data: *mut c_void,
    byte_length: usize,
    is_shared: bool,
    /// When set, called on final destruction to release `data`.
    deleter: BackingStoreDeleterCallback,
    deleter_data: *mut c_void,
    /// `data` was allocated by us via malloc and must be freed.
    owns_malloc: bool,
}

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn calloc(count: usize, size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
}

unsafe extern "C" fn noop_deleter(_data: *mut c_void, _len: usize, _deleter_data: *mut c_void) {}

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

    /// Allocate `byte_length` zeroed bytes and wrap them.
    fn new_allocated(byte_length: usize, is_shared: bool) -> *mut BsInner {
        let data = if byte_length == 0 {
            ptr::null_mut()
        } else {
            unsafe { calloc(byte_length, 1) }
        };
        BsInner::boxed(data, byte_length, is_shared, noop_deleter, ptr::null_mut(), true)
    }

    /// Run the deleter / free, then drop the box.
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
        // Box drop frees the BsInner allocation.
    }
}

#[inline]
fn bs_inner<'a>(p: *const BackingStore) -> Option<&'a BsInner> {
    unsafe { (p as *const BsInner).as_ref() }
}

// --- SharedPtrBase<BackingStore> payload helpers ---------------------
// We stash the BsInner pointer in word 0 of the `[usize; 2]` payload.

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

/// Build a populated `SharedRef<BackingStore>` (by value) that owns one ref to
/// `inner`. The caller is handing over an existing reference count.
#[inline]
fn make_shared_ref(inner: *mut BsInner) -> SharedRef<BackingStore> {
    // SharedRef and SharedPtrBase are both repr(C) `[usize; 2]`-sized.
    let base: SharedPtrBase<BackingStore> = Default::default();
    // SharedRef(SharedPtrBase) — same repr(C) layout. Write the payload words
    // directly so we don't trip SharedPtrBase's Drop on a temporary.
    let mut sref =
        unsafe { std::mem::transmute_copy::<SharedPtrBase<BackingStore>, SharedRef<BackingStore>>(&base) };
    std::mem::forget(base);
    sp_set(
        &mut sref as *mut SharedRef<BackingStore> as *mut SharedPtrBase<BackingStore>,
        inner,
    );
    sref
}

/// Make a fresh shared ref over a JSC array buffer's bytes (non-owning view).
fn backing_store_for_buffer(ctx: JSContextRef, buf: JSValueRef) -> SharedRef<BackingStore> {
    let obj = buf as JSObjectRef;
    let (data, len) = unsafe {
        (
            JSObjectGetArrayBufferBytesPtr(ctx, obj, ptr::null_mut()),
            JSObjectGetArrayBufferByteLength(ctx, obj, ptr::null_mut()),
        )
    };
    let inner = BsInner::boxed(data, len, false, noop_deleter, ptr::null_mut(), false);
    make_shared_ref(inner)
}

// ===================================================================
// ArrayBuffer
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__New__with_byte_length(
    isolate: *mut RealIsolate,
    byte_length: usize,
) -> *const ArrayBuffer {
    let st = iso_state(isolate);
    let ctx = st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
    if ctx.is_null() {
        return ptr::null();
    }
    // Allocate zeroed bytes owned by JSC (it frees via our deallocator).
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
    let ctx = st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
    if ctx.is_null() || backing_store.is_null() {
        return ptr::null();
    }
    let inner = sp_get(backing_store as *const SharedPtrBase<BackingStore>);
    if inner.is_null() {
        return ptr::null();
    }
    let (data, len) = unsafe { ((*inner).data, (*inner).byte_length) };
    // Keep the backing store alive while JSC references the bytes: take an
    // extra ref and release it from the (no-copy) deallocator.
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
pub extern "C" fn v8__ArrayBuffer__ByteLength(this: *const ArrayBuffer) -> usize {
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return 0;
    }
    unsafe { JSObjectGetArrayBufferByteLength(ctx, jsval(this) as JSObjectRef, ptr::null_mut()) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__Data(this: *const ArrayBuffer) -> *mut c_void {
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return ptr::null_mut();
    }
    unsafe { JSObjectGetArrayBufferBytesPtr(ctx, jsval(this) as JSObjectRef, ptr::null_mut()) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__IsDetachable(this: *const ArrayBuffer) -> bool {
    // JSC does not expose detach-key semantics; treat buffers as detachable.
    !this.is_null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__WasDetached(this: *const ArrayBuffer) -> bool {
    // JSC has no public "was detached" query. A detached buffer reports zero
    // length; the vendored wrapper already short-circuits on byte_length != 0.
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__Detach(
    this: *const ArrayBuffer,
    key: *const Value,
) -> MaybeBool {
    // JSC has no public ArrayBuffer detach API. Report success without action.
    // TODO(v82jsc): real detach is unsupported by the JSC C API.
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

// ===================================================================
// BackingStore (standalone)
// ===================================================================

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
    BsInner::boxed(data, byte_length, false, deleter, deleter_data, false) as *mut BackingStore
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BackingStore__Data(this: *const BackingStore) -> *mut c_void {
    bs_inner(this).map_or(ptr::null_mut(), |b| b.data)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BackingStore__ByteLength(this: *const BackingStore) -> usize {
    bs_inner(this).map_or(0, |b| b.byte_length)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BackingStore__IsShared(this: *const BackingStore) -> bool {
    bs_inner(this).map_or(false, |b| b.is_shared)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BackingStore__IsResizableByUserJavaScript(this: *const BackingStore) -> bool {
    // We never create resizable/growable backing stores.
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__BackingStore__DELETE(this: *mut BackingStore) {
    // A bare UniqueRef destruction: drop the single owned reference.
    let inner = this as *mut BsInner;
    if inner.is_null() {
        return;
    }
    if unsafe { (*inner).refcount.fetch_sub(1, Ordering::SeqCst) } == 1 {
        unsafe { BsInner::destroy(inner) };
    }
}

// ===================================================================
// std::shared_ptr<BackingStore>
// ===================================================================

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
    // The UniquePtr is transparent over a `*mut BackingStore` == `*mut BsInner`.
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
pub extern "C" fn std__shared_ptr__v8__BackingStore__reset(ptr: *mut SharedPtrBase<BackingStore>) {
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

// ===================================================================
// ArrayBuffer::Allocator
//
// JSC manages ArrayBuffer memory internally, so a v8 allocator is purely a
// placeholder object. We hand out a sentinel non-null pointer that DELETE frees.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__Allocator__NewDefaultAllocator() -> *mut Allocator {
    // Box a tiny marker so the pointer is unique and freeable.
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
    // Shallow copy of the control words; the allocator is a stateless sentinel.
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

// ===================================================================
// ArrayBufferView
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__Buffer(
    this: *const ArrayBufferView,
) -> *const ArrayBuffer {
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return ptr::null();
    }
    let buf = unsafe { JSObjectGetTypedArrayBuffer(ctx, jsval(this) as JSObjectRef, ptr::null_mut()) };
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
    // Pointer to the start of the backing ArrayBuffer (offset added by caller).
    let buf = unsafe { JSObjectGetTypedArrayBuffer(ctx, jsval(this) as JSObjectRef, ptr::null_mut()) };
    if buf.is_null() {
        return ptr::null_mut();
    }
    unsafe { JSObjectGetArrayBufferBytesPtr(ctx, buf, ptr::null_mut()) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__ByteLength(this: *const ArrayBufferView) -> usize {
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return 0;
    }
    unsafe { JSObjectGetTypedArrayByteLength(ctx, jsval(this) as JSObjectRef, ptr::null_mut()) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__ByteOffset(this: *const ArrayBufferView) -> usize {
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return 0;
    }
    unsafe { JSObjectGetTypedArrayByteOffset(ctx, jsval(this) as JSObjectRef, ptr::null_mut()) }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__HasBuffer(this: *const ArrayBufferView) -> bool {
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return false;
    }
    let buf = unsafe { JSObjectGetTypedArrayBuffer(ctx, jsval(this) as JSObjectRef, ptr::null_mut()) };
    !buf.is_null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__GetContents(
    this: *const ArrayBufferView,
    storage: MemorySpan,
) -> MemorySpan {
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return MemorySpan { data: ptr::null_mut(), size: 0 };
    }
    let obj = jsval(this) as JSObjectRef;
    let (ptr_bytes, off, len) = unsafe {
        (
            JSObjectGetTypedArrayBytesPtr(ctx, obj, ptr::null_mut()),
            JSObjectGetTypedArrayByteOffset(ctx, obj, ptr::null_mut()),
            JSObjectGetTypedArrayByteLength(ctx, obj, ptr::null_mut()),
        )
    };
    // JSC bytes-ptr already accounts for the view's byte offset.
    let data = if ptr_bytes.is_null() {
        ptr::null_mut()
    } else {
        ptr_bytes as *mut u8
    };
    MemorySpan { data, size: len }
}

// ===================================================================
// SharedArrayBuffer
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__SharedArrayBuffer__New__with_backing_store(
    isolate: *mut RealIsolate,
    backing_store: *const SharedRef<BackingStore>,
) -> *const SharedArrayBuffer {
    // JSC has no SharedArrayBuffer C constructor; back it with a plain
    // ArrayBuffer over the same bytes so embedder code can still read/write.
    let st = iso_state(isolate);
    let ctx = st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
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
    // Reuse the ArrayBuffer reader; mark the resulting store as shared.
    let mut sref = backing_store_for_buffer(ctx, jsval(this));
    let inner = sp_get(&sref as *const SharedRef<BackingStore> as *const SharedPtrBase<BackingStore>);
    if !inner.is_null() {
        unsafe { (*inner).is_shared = true };
    }
    sref
}

// ===================================================================
// Typed arrays — all share the `(buf, byte_offset, length) -> *const T` shape.
// ===================================================================

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
typed_array_new!(v8__Uint16Array__New, Uint16Array, kJSTypedArrayTypeUint16Array);
typed_array_new!(v8__Int16Array__New, Int16Array, kJSTypedArrayTypeInt16Array);
typed_array_new!(v8__Uint32Array__New, Uint32Array, kJSTypedArrayTypeUint32Array);
typed_array_new!(v8__Int32Array__New, Int32Array, kJSTypedArrayTypeInt32Array);
typed_array_new!(v8__Float32Array__New, Float32Array, kJSTypedArrayTypeFloat32Array);
typed_array_new!(v8__Float64Array__New, Float64Array, kJSTypedArrayTypeFloat64Array);
typed_array_new!(v8__BigInt64Array__New, BigInt64Array, kJSTypedArrayTypeBigInt64Array);
typed_array_new!(v8__BigUint64Array__New, BigUint64Array, kJSTypedArrayTypeBigUint64Array);
