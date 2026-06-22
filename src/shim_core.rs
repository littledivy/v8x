//! Foundation for the JSC-backed C-ABI shims.
//!
//! This module owns the representation that backs `*mut RealIsolate` and the
//! handle-scope / context machinery. Every other `shim_*` module builds on the
//! helpers exported here:
//!
//! - `iso_state(p)` — `&mut IsoState` behind a `*mut RealIsolate`.
//! - `current_ctx()` — innermost entered `JSContextRef` (thread-local).
//! - `current_iso()` — current `*mut RealIsolate` (thread-local).
//! - `intern::<T>(jsval)` — protect `jsval` against the current context, record
//!   it in the current handle scope, return it as a `*const T`. The pointer of
//!   a `Local<T>` *is* the JSC `JSValueRef`.
//! - `jsval(p)` / `ctx_of(c)` — reinterpret a handle / context pointer.
#![allow(non_snake_case)]

use crate::jsc_sys::*;
use crate::{Context, Data, Object, Primitive, RealIsolate};
use std::cell::RefCell;
use std::os::raw::c_void;
use std::ptr;

/// The JSC-backed state behind a `*mut RealIsolate`.
pub(crate) struct IsoState {
    pub group: JSContextGroupRef,
    /// Entered-context stack; the last is current.
    pub contexts: Vec<JSGlobalContextRef>,
    /// Contexts this isolate created, released on dispose.
    pub owned_contexts: Vec<JSGlobalContextRef>,
    /// Protected handles, sliced by handle scopes via saved lengths.
    pub handles: Vec<(JSContextRef, JSValueRef)>,
    /// Embedder data slots (v8's kNumIsolateDataSlots == 4).
    pub data_slots: [*mut c_void; 4],
}

thread_local! {
    static CURRENT_ISO: RefCell<*mut RealIsolate> = const { RefCell::new(ptr::null_mut()) };
    static CURRENT_CTX: RefCell<JSContextRef> = const { RefCell::new(ptr::null()) };
}

#[inline(always)]
pub(crate) fn iso_state<'a>(p: *mut RealIsolate) -> &'a mut IsoState {
    unsafe { &mut *(p as *mut IsoState) }
}

#[inline(always)]
pub(crate) fn current_iso() -> *mut RealIsolate {
    CURRENT_ISO.with(|c| *c.borrow())
}

#[inline(always)]
pub(crate) fn current_ctx() -> JSContextRef {
    CURRENT_CTX.with(|c| *c.borrow())
}

fn set_current(iso: *mut RealIsolate) {
    CURRENT_ISO.with(|c| *c.borrow_mut() = iso);
}

fn refresh_current_ctx(st: &IsoState) {
    let ctx = st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
    CURRENT_CTX.with(|c| *c.borrow_mut() = ctx);
}

/// Reinterpret a handle pointer as a `JSValueRef` (they are identical).
#[inline(always)]
pub(crate) fn jsval<T>(p: *const T) -> JSValueRef {
    p as JSValueRef
}

/// Reinterpret a `*const Context` as its `JSGlobalContextRef`.
#[inline(always)]
pub(crate) fn ctx_of(c: *const Context) -> JSGlobalContextRef {
    c as JSGlobalContextRef
}

/// Protect `v` against `ctx`, record it in the current isolate's handle stack,
/// and return it as a `*const T`. Returns null for a null input.
#[inline]
pub(crate) fn intern_ctx<T>(ctx: JSContextRef, v: JSValueRef) -> *const T {
    if v.is_null() {
        return ptr::null();
    }
    // Fall back to the current context if none was supplied; protecting or
    // unprotecting against a null context crashes inside JSC.
    let ctx = if ctx.is_null() { current_ctx() } else { ctx };
    let iso = current_iso();
    if !iso.is_null() && !ctx.is_null() {
        unsafe {
            JSValueProtect(ctx, v);
            iso_state(iso).handles.push((ctx, v));
        }
    }
    v as *const T
}

/// Like `intern_ctx`, using the current thread-local context.
#[inline]
pub(crate) fn intern<T>(v: JSValueRef) -> *const T {
    intern_ctx(current_ctx(), v)
}

// ===================================================================
// Isolate
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__New(_params: *const c_void) -> *mut RealIsolate {
    let group = unsafe { JSContextGroupCreate() };
    let state = Box::new(IsoState {
        group,
        contexts: Vec::new(),
        owned_contexts: Vec::new(),
        handles: Vec::new(),
        data_slots: [ptr::null_mut(); 4],
    });
    Box::into_raw(state) as *mut RealIsolate
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__Dispose(this: *mut RealIsolate) {
    if this.is_null() {
        return;
    }
    unsafe {
        let mut st = Box::from_raw(this as *mut IsoState);
        while let Some((ctx, v)) = st.handles.pop() {
            if !ctx.is_null() && !v.is_null() {
                JSValueUnprotect(ctx, v);
            }
        }
        for ctx in st.owned_contexts.drain(..) {
            JSGlobalContextRelease(ctx);
        }
        JSContextGroupRelease(st.group);
    }
    set_current(ptr::null_mut());
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__Enter(this: *mut RealIsolate) {
    set_current(this);
    refresh_current_ctx(iso_state(this));
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__Exit(_this: *mut RealIsolate) {
    set_current(ptr::null_mut());
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetCurrent() -> *mut RealIsolate {
    current_iso()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetNumberOfDataSlots(_this: *const RealIsolate) -> u32 {
    4
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetData(isolate: *const RealIsolate, slot: u32) -> *mut c_void {
    let st = iso_state(isolate as *mut RealIsolate);
    *st.data_slots.get(slot as usize).unwrap_or(&ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__SetData(isolate: *const RealIsolate, slot: u32, data: *mut c_void) {
    let st = iso_state(isolate as *mut RealIsolate);
    if let Some(s) = st.data_slots.get_mut(slot as usize) {
        *s = data;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__GetCurrentContext(isolate: *mut RealIsolate) -> *const Context {
    let st = iso_state(isolate);
    st.contexts.last().copied().unwrap_or(ptr::null_mut()) as *const Context
}

// ===================================================================
// HandleScope — store [isolate_ptr, saved_handle_depth] in the raw buffer
// ([MaybeUninit<usize>; HANDLE_SCOPE_SIZE]); DESTRUCT unprotects down to it.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__HandleScope__CONSTRUCT(buf: *mut usize, isolate: *mut RealIsolate) {
    set_current(isolate);
    let st = iso_state(isolate);
    refresh_current_ctx(st);
    unsafe {
        *buf.offset(0) = isolate as usize;
        *buf.offset(1) = st.handles.len();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__HandleScope__DESTRUCT(this: *mut usize) {
    unsafe {
        let isolate = *this.offset(0) as *mut RealIsolate;
        let saved_depth = *this.offset(1);
        if isolate.is_null() {
            return;
        }
        let st = iso_state(isolate);
        while st.handles.len() > saved_depth {
            let (ctx, v) = st.handles.pop().unwrap();
            if !ctx.is_null() && !v.is_null() {
                JSValueUnprotect(ctx, v);
            }
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Local__New(isolate: *mut RealIsolate, other: *const Data) -> *const Data {
    let _ = isolate;
    // Clone the handle into the current scope: re-protect and record.
    intern::<Data>(jsval(other))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Undefined(isolate: *mut RealIsolate) -> *const Primitive {
    let st = iso_state(isolate);
    let ctx = st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
    if ctx.is_null() {
        return ptr::null();
    }
    let v = unsafe { JSValueMakeUndefined(ctx) };
    intern_ctx::<Primitive>(ctx, v)
}

// ===================================================================
// Context
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__New(
    isolate: *mut RealIsolate,
    _templ: *const c_void,
    _global_object: *const c_void,
    _microtask_queue: *mut c_void,
) -> *const Context {
    let st = iso_state(isolate);
    let ctx = unsafe { JSGlobalContextCreateInGroup(st.group, ptr::null_mut()) };
    st.owned_contexts.push(ctx);
    ctx as *const Context
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__Enter(this: *const Context) {
    let iso = current_iso();
    if iso.is_null() {
        return;
    }
    let st = iso_state(iso);
    st.contexts.push(ctx_of(this));
    refresh_current_ctx(st);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__Exit(_this: *const Context) {
    let iso = current_iso();
    if iso.is_null() {
        return;
    }
    let st = iso_state(iso);
    st.contexts.pop();
    refresh_current_ctx(st);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Context__Global(this: *const Context) -> *const Object {
    let ctx = ctx_of(this);
    let global = unsafe { JSContextGetGlobalObject(ctx) };
    intern_ctx::<Object>(ctx, global as JSValueRef)
}
