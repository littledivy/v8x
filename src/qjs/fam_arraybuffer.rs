// Family: arraybuffer (QuickJS-ng backend)
// ArrayBuffer / ArrayBufferView / BackingStore / TypedArray / DataView /
// SharedArrayBuffer + typed-array `New` constructors, backed by QuickJS-ng.
//
// Mirrors src/shim_arraybuffer.rs (the JSC template) one-for-one, swapping JSC
// calls for QuickJS-ng calls (JS_NewArrayBuffer / JS_GetArrayBuffer /
// JS_NewTypedArray / JS_GetTypedArrayBuffer / JS_GetTypedArrayType /
// JS_DetachArrayBuffer).
//
// BackingStore is a Rust-owned, refcounted `BsInner` box that lives behind the
// vendored opaque `BackingStore` type; the same payload is stashed in word 0 of
// the `SharedPtrBase<BackingStore>` `[usize; 2]` control block, exactly as the
// JSC backend does.
#![allow(non_snake_case, unused)]

use crate::qjs::quickjs_sys::*;
use crate::qjs::shim_core::{ctx_of, current_ctx, current_iso, intern, iso_state, jsval_of};
use crate::binding::memory_span_t;
use crate::support::{MaybeBool, SharedPtrBase, SharedRef, UniquePtr, long};
use crate::{
    ArrayBuffer, ArrayBufferView, BackingStore, BackingStoreDeleterCallback, Context, DataView,
    RealIsolate, SharedArrayBuffer, Value,
};
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};

// ===================================================================
// Extra QuickJS-ng C API not present in quickjs_sys.rs
// ===================================================================

#[allow(non_camel_case_types)]
type JSTypedArrayEnum = i32;
const JS_TYPED_ARRAY_UINT8C: JSTypedArrayEnum = 0;
const JS_TYPED_ARRAY_INT8: JSTypedArrayEnum = 1;
const JS_TYPED_ARRAY_UINT8: JSTypedArrayEnum = 2;
const JS_TYPED_ARRAY_INT16: JSTypedArrayEnum = 3;
const JS_TYPED_ARRAY_UINT16: JSTypedArrayEnum = 4;
const JS_TYPED_ARRAY_INT32: JSTypedArrayEnum = 5;
const JS_TYPED_ARRAY_UINT32: JSTypedArrayEnum = 6;
const JS_TYPED_ARRAY_BIG_INT64: JSTypedArrayEnum = 7;
const JS_TYPED_ARRAY_BIG_UINT64: JSTypedArrayEnum = 8;
const JS_TYPED_ARRAY_FLOAT16: JSTypedArrayEnum = 9;
const JS_TYPED_ARRAY_FLOAT32: JSTypedArrayEnum = 10;
const JS_TYPED_ARRAY_FLOAT64: JSTypedArrayEnum = 11;

// `JSFreeArrayBufferDataFunc(rt, opaque, ptr)` — note the QuickJS deallocator
// signature differs from JSC's: (rt, opaque, ptr) vs JSC's (ptr, ctx).
#[allow(non_camel_case_types)]
type JSFreeArrayBufferDataFunc =
    Option<unsafe extern "C" fn(rt: *mut JSRuntime, opaque: *mut c_void, ptr: *mut c_void)>;

unsafe extern "C" {
    fn JS_NewArrayBuffer(
        ctx: *mut JSContext,
        buf: *mut u8,
        len: usize,
        free_func: JSFreeArrayBufferDataFunc,
        opaque: *mut c_void,
        is_shared: bool,
    ) -> JSValue;
    fn JS_GetArrayBuffer(ctx: *mut JSContext, psize: *mut usize, obj: JSValue) -> *mut u8;
    fn JS_DetachArrayBuffer(ctx: *mut JSContext, obj: JSValue);
    fn JS_NewTypedArray(
        ctx: *mut JSContext,
        argc: i32,
        argv: *mut JSValue,
        array_type: JSTypedArrayEnum,
    ) -> JSValue;
    fn JS_GetTypedArrayBuffer(
        ctx: *mut JSContext,
        obj: JSValue,
        pbyte_offset: *mut usize,
        pbyte_length: *mut usize,
        pbytes_per_element: *mut usize,
    ) -> JSValue;
    fn JS_GetTypedArrayType(obj: JSValue) -> i32;
}

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn calloc(count: usize, size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
}

// ===================================================================
// BackingStore representation
//
// `*mut BackingStore` (a UniqueRef) and the `SharedPtrBase<BackingStore>`
// payload both point at a refcounted `BsInner`. The vendored `BackingStore`
// type is opaque to v8; it only round-trips the raw pointer and calls our
// accessors, so we box our own structure behind it.
// ===================================================================

struct BsInner {
    refcount: AtomicUsize,
    data: *mut c_void,
    byte_length: usize,
    is_shared: bool,
    /// When set, called on final destruction to release `data`.
    deleter: BackingStoreDeleterCallback,
    deleter_data: *mut c_void,
    /// `data` was allocated by us via malloc/calloc and must be freed.
    owns_malloc: bool,
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
    let mut sref = unsafe {
        std::mem::transmute_copy::<SharedPtrBase<BackingStore>, SharedRef<BackingStore>>(&base)
    };
    std::mem::forget(base);
    sp_set(
        &mut sref as *mut SharedRef<BackingStore> as *mut SharedPtrBase<BackingStore>,
        inner,
    );
    sref
}

/// Make a fresh shared ref over a QuickJS array buffer's bytes (non-owning view).
fn backing_store_for_buffer(ctx: *mut JSContext, buf: JSValue) -> SharedRef<BackingStore> {
    let mut len: usize = 0;
    let data = unsafe { JS_GetArrayBuffer(ctx, &mut len, buf) } as *mut c_void;
    let inner = BsInner::boxed(data, len, false, noop_deleter, ptr::null_mut(), false);
    make_shared_ref(inner)
}

// QuickJS no-copy deallocator: releases a ref on the BsInner stashed in `opaque`.
unsafe extern "C" fn bs_free_func(_rt: *mut JSRuntime, opaque: *mut c_void, _ptr: *mut c_void) {
    let inner = opaque as *mut BsInner;
    if inner.is_null() {
        return;
    }
    if unsafe { (*inner).refcount.fetch_sub(1, Ordering::SeqCst) } == 1 {
        unsafe { BsInner::destroy(inner) };
    }
}

// Frees a plain malloc'd buffer that QuickJS holds no-copy (our own allocation).
unsafe extern "C" fn malloc_free_func(_rt: *mut JSRuntime, _opaque: *mut c_void, ptr: *mut c_void) {
    if !ptr.is_null() {
        unsafe { free(ptr) };
    }
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
    let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
    if ctx.is_null() {
        return ptr::null();
    }
    // Allocate zeroed bytes owned by QuickJS (it frees via our deallocator).
    let data = if byte_length == 0 {
        ptr::null_mut()
    } else {
        unsafe { calloc(byte_length, 1) as *mut u8 }
    };
    let obj = unsafe {
        JS_NewArrayBuffer(
            ctx,
            data,
            byte_length,
            Some(malloc_free_func),
            ptr::null_mut(),
            false,
        )
    };
    if obj.tag == JS_TAG_EXCEPTION {
        return ptr::null();
    }
    intern::<ArrayBuffer>(obj)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__New__with_backing_store(
    isolate: *mut RealIsolate,
    backing_store: *const SharedRef<BackingStore>,
) -> *const ArrayBuffer {
    let st = iso_state(isolate);
    let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
    if ctx.is_null() || backing_store.is_null() {
        return ptr::null();
    }
    let inner = sp_get(backing_store as *const SharedPtrBase<BackingStore>);
    if inner.is_null() {
        return ptr::null();
    }
    let (data, len) = unsafe { ((*inner).data, (*inner).byte_length) };
    // Keep the backing store alive while QuickJS references the bytes: take an
    // extra ref and release it from the (no-copy) deallocator.
    unsafe { (*inner).refcount.fetch_add(1, Ordering::SeqCst) };
    let obj = unsafe {
        JS_NewArrayBuffer(
            ctx,
            data as *mut u8,
            len,
            Some(bs_free_func),
            inner as *mut c_void,
            false,
        )
    };
    if obj.tag == JS_TAG_EXCEPTION {
        // Constructor failed; undo the extra ref we took.
        unsafe { bs_free_func(ptr::null_mut(), inner as *mut c_void, data) };
        return ptr::null();
    }
    intern::<ArrayBuffer>(obj)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__ByteLength(this: *const ArrayBuffer) -> usize {
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return 0;
    }
    let mut len: usize = 0;
    unsafe { JS_GetArrayBuffer(ctx, &mut len, jsval_of(this)) };
    len
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__Data(this: *const ArrayBuffer) -> *mut c_void {
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return ptr::null_mut();
    }
    let mut len: usize = 0;
    unsafe { JS_GetArrayBuffer(ctx, &mut len, jsval_of(this)) as *mut c_void }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__IsDetachable(this: *const ArrayBuffer) -> bool {
    // QuickJS supports detaching any ArrayBuffer; treat non-null buffers as
    // detachable.
    !this.is_null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__WasDetached(this: *const ArrayBuffer) -> bool {
    // QuickJS has no public "was detached" query. A detached buffer reports a
    // null data pointer; the vendored wrapper short-circuits on byte_length.
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return false;
    }
    let mut len: usize = 0;
    let data = unsafe { JS_GetArrayBuffer(ctx, &mut len, jsval_of(this)) };
    if data.is_null() {
        // JS_GetArrayBuffer throws on a detached buffer; clear the exception.
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return true;
    }
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__Detach(this: *const ArrayBuffer, key: *const Value) -> MaybeBool {
    let _ = key;
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return MaybeBool::Nothing;
    }
    unsafe { JS_DetachArrayBuffer(ctx, jsval_of(this)) };
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
    backing_store_for_buffer(ctx, jsval_of(this))
}

// ===================================================================
// BackingStore (standalone)
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBuffer__NewBackingStore__with_byte_length(
    isolate: *mut RealIsolate,
    byte_length: usize,
) -> *mut BackingStore {
    let _ = isolate;
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

// ===================================================================
// ArrayBufferView
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__Buffer(this: *const ArrayBufferView) -> *const ArrayBuffer {
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return ptr::null();
    }
    // JS_GetTypedArrayBuffer returns an owned (+1) ArrayBuffer value.
    let buf = unsafe {
        JS_GetTypedArrayBuffer(
            ctx,
            jsval_of(this),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if buf.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return ptr::null();
    }
    intern::<ArrayBuffer>(buf)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__Buffer__Data(this: *const ArrayBufferView) -> *mut c_void {
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return ptr::null_mut();
    }
    // Pointer to the start of the backing ArrayBuffer (offset added by caller).
    let buf = unsafe {
        JS_GetTypedArrayBuffer(
            ctx,
            jsval_of(this),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if buf.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return ptr::null_mut();
    }
    let mut len: usize = 0;
    let data = unsafe { JS_GetArrayBuffer(ctx, &mut len, buf) as *mut c_void };
    // JS_GetTypedArrayBuffer handed us an owned ref; release it.
    unsafe { JS_FreeValue(ctx, buf) };
    data
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__ByteLength(this: *const ArrayBufferView) -> usize {
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return 0;
    }
    let mut len: usize = 0;
    let buf = unsafe {
        JS_GetTypedArrayBuffer(ctx, jsval_of(this), ptr::null_mut(), &mut len, ptr::null_mut())
    };
    if buf.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return 0;
    }
    unsafe { JS_FreeValue(ctx, buf) };
    len
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__ByteOffset(this: *const ArrayBufferView) -> usize {
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return 0;
    }
    let mut off: usize = 0;
    let buf = unsafe {
        JS_GetTypedArrayBuffer(ctx, jsval_of(this), &mut off, ptr::null_mut(), ptr::null_mut())
    };
    if buf.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return 0;
    }
    unsafe { JS_FreeValue(ctx, buf) };
    off
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__HasBuffer(this: *const ArrayBufferView) -> bool {
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return false;
    }
    let buf = unsafe {
        JS_GetTypedArrayBuffer(
            ctx,
            jsval_of(this),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if buf.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return false;
    }
    let ok = buf.tag == JS_TAG_OBJECT;
    unsafe { JS_FreeValue(ctx, buf) };
    ok
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ArrayBufferView__GetContents(
    this: *const ArrayBufferView,
    storage: memory_span_t,
) -> memory_span_t {
    let _ = storage;
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return memory_span_t { data: ptr::null_mut(), size: 0 };
    }
    let mut off: usize = 0;
    let mut len: usize = 0;
    let buf = unsafe {
        JS_GetTypedArrayBuffer(ctx, jsval_of(this), &mut off, &mut len, ptr::null_mut())
    };
    if buf.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return memory_span_t { data: ptr::null_mut(), size: 0 };
    }
    let mut buf_len: usize = 0;
    let base = unsafe { JS_GetArrayBuffer(ctx, &mut buf_len, buf) };
    unsafe { JS_FreeValue(ctx, buf) };
    let data = if base.is_null() {
        ptr::null_mut()
    } else {
        // Off-heap backing store: hand back a view including the byte offset.
        unsafe { base.add(off) }
    };
    memory_span_t { data, size: len }
}

// ===================================================================
// SharedArrayBuffer
//
// QuickJS-ng has no first-class SharedArrayBuffer C constructor exposed here;
// back it with a plain ArrayBuffer over the same bytes so embedder code can
// still read/write. SAB threading semantics are not provided (deno tests that
// require true SAB are gated to V8).
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__SharedArrayBuffer__New__with_backing_store(
    isolate: *mut RealIsolate,
    backing_store: *const SharedRef<BackingStore>,
) -> *const SharedArrayBuffer {
    let st = iso_state(isolate);
    let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
    if ctx.is_null() || backing_store.is_null() {
        return ptr::null();
    }
    let inner = sp_get(backing_store as *const SharedPtrBase<BackingStore>);
    if inner.is_null() {
        return ptr::null();
    }
    let (data, len) = unsafe { ((*inner).data, (*inner).byte_length) };
    unsafe { (*inner).refcount.fetch_add(1, Ordering::SeqCst) };
    let obj = unsafe {
        JS_NewArrayBuffer(
            ctx,
            data as *mut u8,
            len,
            Some(bs_free_func),
            inner as *mut c_void,
            false,
        )
    };
    if obj.tag == JS_TAG_EXCEPTION {
        unsafe { bs_free_func(ptr::null_mut(), inner as *mut c_void, data) };
        return ptr::null();
    }
    intern::<SharedArrayBuffer>(obj)
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
    let sref = backing_store_for_buffer(ctx, jsval_of(this));
    let inner =
        sp_get(&sref as *const SharedRef<BackingStore> as *const SharedPtrBase<BackingStore>);
    if !inner.is_null() {
        unsafe { (*inner).is_shared = true };
    }
    sref
}

// ===================================================================
// Typed arrays — all share the `(buf, byte_offset, length) -> *const T` shape.
//
// QuickJS-ng builds typed arrays through the JS constructor path, so we pass
// `[buffer, byte_offset, length]` as JS args to JS_NewTypedArray.
// ===================================================================

#[inline]
fn make_typed_array(
    buf: *const ArrayBuffer,
    byte_offset: usize,
    length: usize,
    ty: JSTypedArrayEnum,
) -> JSValue {
    let ctx = current_ctx();
    if ctx.is_null() || buf.is_null() {
        return JSValue { u: JSValueUnion { int32: 0 }, tag: JS_TAG_NULL };
    }
    // argv[0] = buffer (borrowed — JS_NewTypedArray does not consume it),
    // argv[1] = byteOffset, argv[2] = length.
    let mut argv: [JSValue; 3] = [
        jsval_of(buf),
        unsafe { JS_NewInt64(ctx, byte_offset as i64) },
        unsafe { JS_NewInt64(ctx, length as i64) },
    ];
    let v = unsafe { JS_NewTypedArray(ctx, 3, argv.as_mut_ptr(), ty) };
    // byteOffset / length are plain numbers (no heap ref) but free defensively.
    unsafe { JS_FreeValue(ctx, argv[1]) };
    unsafe { JS_FreeValue(ctx, argv[2]) };
    v
}

macro_rules! typed_array_new {
    ($fn_name:ident, $ty_name:ident, $qjs_ty:expr) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn $fn_name(
            buf_ptr: *const ArrayBuffer,
            byte_offset: usize,
            length: usize,
        ) -> *const crate::$ty_name {
            let v = make_typed_array(buf_ptr, byte_offset, length, $qjs_ty);
            if v.tag == JS_TAG_EXCEPTION {
                let ctx = current_ctx();
                if !ctx.is_null() {
                    let exc = unsafe { JS_GetException(ctx) };
                    unsafe { JS_FreeValue(ctx, exc) };
                }
                return ptr::null();
            }
            if v.tag != JS_TAG_OBJECT {
                return ptr::null();
            }
            intern::<crate::$ty_name>(v)
        }
    };
}

typed_array_new!(v8__Uint8Array__New, Uint8Array, JS_TYPED_ARRAY_UINT8);
typed_array_new!(v8__Int8Array__New, Int8Array, JS_TYPED_ARRAY_INT8);
typed_array_new!(v8__Uint16Array__New, Uint16Array, JS_TYPED_ARRAY_UINT16);
typed_array_new!(v8__Int16Array__New, Int16Array, JS_TYPED_ARRAY_INT16);
typed_array_new!(v8__Uint32Array__New, Uint32Array, JS_TYPED_ARRAY_UINT32);
typed_array_new!(v8__Int32Array__New, Int32Array, JS_TYPED_ARRAY_INT32);
typed_array_new!(v8__Float32Array__New, Float32Array, JS_TYPED_ARRAY_FLOAT32);
typed_array_new!(v8__Float64Array__New, Float64Array, JS_TYPED_ARRAY_FLOAT64);
typed_array_new!(v8__BigInt64Array__New, BigInt64Array, JS_TYPED_ARRAY_BIG_INT64);
typed_array_new!(v8__BigUint64Array__New, BigUint64Array, JS_TYPED_ARRAY_BIG_UINT64);
