//! Foundation for the QuickJS-backed C-ABI shims.
//!
//! This module owns the representation that backs `*mut RealIsolate` and the
//! handle-scope / context / arena machinery. Every other `shim_*` module in the
//! QuickJS backend builds on the helpers exported here.
//!
//! ## The key design difference from JSC
//!
//! In the JSC backend a `Local<T>`'s pointer *is* the `JSValueRef` (a pointer).
//! In QuickJS a `JSValue` is a 16-byte struct (a union + a tag), **not** a
//! pointer, so it cannot itself be a v8 `Local<T>` (which the vendored source
//! treats as `*const T`, a pointer). We therefore use an **arena**: every
//! handle is a heap box holding one `JSValue`; the box's address is the v8
//! handle. Reading the box recovers the `JSValue`.
//!
//! ## Refcount discipline (the #1 correctness risk)
//!
//! Invariant: every arena slot owns **exactly one** QuickJS refcount on its
//! `JSValue`, and frees it **exactly once** when the slot is reclaimed (on
//! handle-scope pop or isolate dispose). Promoting a borrowed `JSValue` into a
//! handle therefore `JS_DupValue`s it; producing a fresh value (`JS_Eval`,
//! `JS_NewObject`, ...) already returns +1, so it is moved into the slot
//! without an extra dup.
//!
//! ## Helper API (used by every other QuickJS `shim_*` module)
//!
//! - `iso_state(p)` — `&mut IsoState` behind a `*mut RealIsolate`.
//! - `current_iso()` — current `*mut RealIsolate` (thread-local).
//! - `current_ctx()` — innermost entered `*mut JSContext` (thread-local).
//! - `intern::<T>(jsval)` — move an owned `JSValue` into a fresh arena slot in
//!   the current handle scope; returns the slot pointer as `*const T`.
//! - `intern_dup::<T>(jsval)` — like `intern` but `JS_DupValue`s first (use
//!   when the `JSValue` is borrowed and you must not consume its refcount).
//! - `jsval_of(ptr)` — read the `JSValue` out of a handle slot pointer.
//! - `ctx_of(c)` — recover the `*mut JSContext` backing a `*const Context`.

#![allow(non_snake_case)]

use super::quickjs_sys::*;
use crate::{Context, Data, Object, Primitive, RealIsolate, String as V8String, Value};
use std::cell::RefCell;
use std::os::raw::c_void;
use std::ptr;

/// The QuickJS-backed state behind a `*mut RealIsolate`.
pub(crate) struct IsoState {
    /// The owning runtime.
    pub rt: *mut JSRuntime,
    /// The single QuickJS context this isolate evaluates in. QuickJS has no
    /// clean notion of a context-as-value distinct from the runtime, so the
    /// `*const Context` v8 handle we hand out is just this pointer reinterpreted.
    pub ctx: *mut JSContext,
    /// Entered-context stack (v8 `Context::Enter`/`Exit`); the last is current.
    /// All entries here are `self.ctx` for now, but we keep the stack so the
    /// enter/exit nesting balances exactly as v8 expects.
    pub contexts: Vec<*mut JSContext>,
    /// The arena: one heap box per live handle, each owning one refcount.
    /// `handles[i]` is `Box::into_raw`'d; reclaimed by `JS_FreeValue` + drop.
    pub handles: Vec<*mut JSValue>,
    /// Embedder data slots (v8's kNumIsolateDataSlots == 4).
    pub data_slots: [*mut c_void; 4],
}

thread_local! {
    static CURRENT_ISO: RefCell<*mut RealIsolate> = const { RefCell::new(ptr::null_mut()) };
    static CURRENT_CTX: RefCell<*mut JSContext> = const { RefCell::new(ptr::null_mut()) };
    // Most-recently-active isolate; used as a fallback when CURRENT_ISO is
    // transiently cleared by a re-entrant scope exit. Never reset on exit.
    static LAST_ISO: RefCell<*mut RealIsolate> = const { RefCell::new(ptr::null_mut()) };
}

#[inline(always)]
pub(crate) fn iso_state<'a>(p: *mut RealIsolate) -> &'a mut IsoState {
    unsafe { &mut *(p as *mut IsoState) }
}

#[inline(always)]
pub(crate) fn current_iso() -> *mut RealIsolate {
    let cur = CURRENT_ISO.with(|c| *c.borrow());
    if !cur.is_null() {
        return cur;
    }
    LAST_ISO.with(|c| *c.borrow())
}

#[inline(always)]
pub(crate) fn current_ctx() -> *mut JSContext {
    CURRENT_CTX.with(|c| *c.borrow())
}

fn set_current(iso: *mut RealIsolate) {
    CURRENT_ISO.with(|c| *c.borrow_mut() = iso);
    if !iso.is_null() {
        LAST_ISO.with(|c| *c.borrow_mut() = iso);
    }
}

fn clear_last_iso(iso: *mut RealIsolate) {
    LAST_ISO.with(|c| {
        if *c.borrow() == iso {
            *c.borrow_mut() = ptr::null_mut();
        }
    });
}

fn refresh_current_ctx(st: &IsoState) {
    let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
    CURRENT_CTX.with(|c| *c.borrow_mut() = ctx);
}

/// Read the `JSValue` stored in a handle slot pointer. Returns `undefined` for
/// a null pointer so callers needn't special-case it.
#[inline(always)]
pub(crate) fn jsval_of<T>(p: *const T) -> JSValue {
    if p.is_null() {
        return jsv_undefined();
    }
    unsafe { *(p as *const JSValue) }
}

/// Private JSValue tag for a v8 `Context` handle. A `*mut JSContext` is **not**
/// a `JSValue` (unlike JSC, where a context ref is a protectable value), so a
/// Context handle cannot be an ordinary arena slot — `jsval_of`/`JS_DupValue`
/// run by `Local::new`/`Global::new` would read/refcount it as garbage. Instead
/// we box a `JSValue` that *encodes* the ctx pointer under this tag. The tag is
/// positive, so QuickJS's `JS_DupValue`/`JS_FreeValue` (which only refcount the
/// negative heap tags) treat it as a plain immediate: dup/free are no-ops and
/// the encoded pointer round-trips losslessly through the handle machinery.
pub(crate) const JS_TAG_V8_CONTEXT: i64 = 0x7632; // 'v2'

/// Encode a `*mut JSContext` as a non-refcounted `JSValue` (see
/// [`JS_TAG_V8_CONTEXT`]).
#[inline(always)]
pub(crate) fn ctx_to_jsval(ctx: *mut JSContext) -> JSValue {
    make_value(JS_TAG_V8_CONTEXT, JSValueUnion { ptr: ctx as *mut c_void })
}

/// Move a context pointer into a fresh arena slot and return it as a v8
/// `Context` handle.
#[inline(always)]
pub(crate) fn intern_ctx(ctx: *mut JSContext) -> *const Context {
    intern::<Context>(ctx_to_jsval(ctx))
}

/// Is `p` a non-JSValue handle (a raw template box pointer) that must be
/// passed through `Local::new`/`Global::new` by identity rather than read and
/// duped as a `JSValue`? Mirrors the JSC backend's `is_non_value_handle`.
#[inline(always)]
pub(crate) fn is_non_value_handle<T>(p: *const T) -> bool {
    !p.is_null() && super::fam_function::is_template_ptr(p as *const c_void)
}

/// Recover the `*mut JSContext` backing a `*const Context` handle.
///
/// Accepts both representations for robustness: an arena slot holding a
/// [`JS_TAG_V8_CONTEXT`]-encoded value (the canonical form), or — as a
/// fallback — a raw `*mut JSContext` reinterpreted directly (internal callers
/// that pass `ctx as *const Context` straight into another shim).
#[inline(always)]
pub(crate) fn ctx_of(c: *const Context) -> *mut JSContext {
    if c.is_null() {
        return ptr::null_mut();
    }
    let v = unsafe { *(c as *const JSValue) };
    if v.tag == JS_TAG_V8_CONTEXT {
        unsafe { v.u.ptr as *mut JSContext }
    } else {
        // Raw `*mut JSContext` passed directly (not an arena slot).
        c as *mut JSContext
    }
}

/// The context to root a fresh handle against, when none was supplied.
#[inline]
fn fallback_ctx(iso: *mut RealIsolate) -> *mut JSContext {
    if iso.is_null() {
        return ptr::null_mut();
    }
    let st = iso_state(iso);
    st.contexts.last().copied().unwrap_or(st.ctx)
}

/// Move an **owned** `JSValue` (refcount already +1 for us) into a fresh arena
/// slot recorded in the current isolate's handle stack. Returns the slot's
/// address as a `*const T` — that pointer *is* the v8 handle. Returns null for
/// a null isolate (after dropping the refcount, to avoid a leak).
#[inline]
pub(crate) fn intern<T>(v: JSValue) -> *const T {
    let iso = current_iso();
    if iso.is_null() {
        // No isolate to record against — free the value so we don't leak.
        let ctx = current_ctx();
        if !ctx.is_null() {
            unsafe { JS_FreeValue(ctx, v) };
        }
        return ptr::null();
    }
    let slot = Box::into_raw(Box::new(v));
    iso_state(iso).handles.push(slot);
    slot as *const T
}

/// Like [`intern`] but for a **borrowed** `JSValue`: `JS_DupValue`s first so the
/// arena slot gets its own refcount and the caller keeps theirs.
#[inline]
pub(crate) fn intern_dup<T>(ctx: *mut JSContext, v: JSValue) -> *const T {
    let ctx = if ctx.is_null() { current_ctx() } else { ctx };
    let ctx = if ctx.is_null() { fallback_ctx(current_iso()) } else { ctx };
    if ctx.is_null() {
        return ptr::null();
    }
    let dup = unsafe { JS_DupValue(ctx, v) };
    intern::<T>(dup)
}

// ===================================================================
// Isolate
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__New(_params: *const c_void) -> *mut RealIsolate {
    let rt = unsafe { JS_NewRuntime() };
    assert!(!rt.is_null(), "JS_NewRuntime failed");
    // QuickJS-ng's default stack is small; bump it so deeper evals don't trip
    // the stack guard. Harmless for the eval-path test.
    unsafe { JS_SetMaxStackSize(rt, 8 * 1024 * 1024) };
    // Install a module loader so QuickJS can resolve static imports
    // (`import x from "ext:..."`) by looking up source stashed in CompileModule.
    unsafe {
        JS_SetModuleLoaderFunc(
            rt,
            if std::env::var_os("QJS_NO_NORM").is_some() {
                None
            } else {
                Some(super::fam_module::module_normalize_callback)
            },
            Some(super::fam_module::module_loader_callback),
            ptr::null_mut(),
        )
    };
    let ctx = unsafe { JS_NewContext(rt) };
    assert!(!ctx.is_null(), "JS_NewContext failed");
    // Install `globalThis.WebAssembly` (WAMR-backed). QuickJS ships no WASM
    // engine; npm code (undici, etc.) needs it.
    if std::env::var_os("QJS_NO_WASM").is_none() {
        super::fam_wasm::install_webassembly(ctx);
    }
    let state = Box::new(IsoState {
        rt,
        ctx,
        contexts: Vec::new(),
        handles: Vec::new(),
        data_slots: [ptr::null_mut(); 4],
    });
    Box::into_raw(state) as *mut RealIsolate
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__CreateParams__CONSTRUCT(
    buf: *mut std::mem::MaybeUninit<crate::isolate_create_params::raw::CreateParams>,
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
pub extern "C" fn v8__Isolate__CreateParams__SIZEOF() -> usize {
    std::mem::size_of::<crate::isolate_create_params::raw::CreateParams>()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__Dispose(this: *mut RealIsolate) {
    if this.is_null() {
        return;
    }
    unsafe {
        let mut st = Box::from_raw(this as *mut IsoState);
        // Free + drop every live arena slot.
        while let Some(slot) = st.handles.pop() {
            let v = *slot;
            JS_FreeValue(st.ctx, v);
            drop(Box::from_raw(slot));
        }
        JS_FreeContext(st.ctx);
        if std::env::var_os("QJS_SKIP_FREE_RT").is_none() {
            JS_FreeRuntime(st.rt);
        }
    }
    set_current(ptr::null_mut());
    clear_last_iso(this);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Isolate__Enter(this: *mut RealIsolate) {
    set_current(this);
    if !this.is_null() {
        refresh_current_ctx(iso_state(this));
    }
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
    if isolate.is_null() {
        return ptr::null();
    }
    let st = iso_state(isolate);
    let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
    intern_ctx(ctx)
}

// ===================================================================
// HandleScope — store [isolate_ptr, saved_handle_depth] in the raw buffer
// ([MaybeUninit<usize>; HANDLE_SCOPE_SIZE]); DESTRUCT frees + drops down to it.
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
            let slot = st.handles.pop().unwrap();
            let v = *slot;
            JS_FreeValue(st.ctx, v);
            drop(Box::from_raw(slot));
        }
    }
}

// ===================================================================
// EscapableHandleScope support.
//
// `v8__EscapeSlot__reserve` runs while the *parent* scope is current (before the
// inner HandleScope is constructed). We push an `undefined` placeholder slot
// onto the isolate handle stack at the parent depth and return its index. The
// inner HandleScope's DESTRUCT only pops slots above its saved depth, so this
// reserved slot survives. On escape we retarget the reserved slot to hold an
// owned (+1) copy of the escaped value and hand back the slot pointer as the
// parent-scoped `Local`.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__EscapeSlot__reserve(isolate: *mut RealIsolate) -> usize {
    if isolate.is_null() {
        return usize::MAX;
    }
    let st = iso_state(isolate);
    let slot = Box::into_raw(Box::new(jsv_undefined()));
    st.handles.push(slot);
    st.handles.len() - 1
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__EscapeSlot__escape(
    isolate: *mut RealIsolate,
    index: usize,
    value: *const Data,
) -> *const Data {
    if isolate.is_null() || index == usize::MAX || value.is_null() {
        return value;
    }
    // Non-value handles (template box pointers) carry no refcount; escape them
    // by identity, exactly like Local::New.
    if is_non_value_handle(value) {
        return value;
    }
    let st = iso_state(isolate);
    let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
    let Some(&slot) = st.handles.get(index) else {
        return value;
    };
    let new_val = unsafe { JS_DupValue(ctx, jsval_of(value)) };
    unsafe {
        let old = *slot;
        *slot = new_val;
        JS_FreeValue(ctx, old);
    }
    slot as *const Data
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Local__New(isolate: *mut RealIsolate, other: *const Data) -> *const Data {
    if other.is_null() {
        return ptr::null();
    }
    // FunctionTemplate / ObjectTemplate handles are raw `Box<…Template>`
    // pointers, NOT arena JSValue slots — reading them as a JSValue and duping
    // would corrupt the template (e.g. yield a null `proto`). Like the JSC
    // backend's `intern`, hand such non-value handles back by identity.
    if is_non_value_handle(other) {
        return other;
    }
    // Clone the handle into the current scope: dup the JSValue into a fresh slot.
    let ctx = if isolate.is_null() {
        current_ctx()
    } else {
        let st = iso_state(isolate);
        st.contexts.last().copied().unwrap_or(st.ctx)
    };
    intern_dup::<Data>(ctx, jsval_of(other))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Undefined(isolate: *mut RealIsolate) -> *const Primitive {
    let _ = isolate;
    intern::<Primitive>(jsv_undefined())
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
    if isolate.is_null() {
        return ptr::null();
    }
    let ctx = iso_state(isolate).ctx;
    install_default_globals(ctx);
    // QuickJS has one context per isolate here; hand back a Context handle that
    // encodes its pointer (see `intern_ctx` / `JS_TAG_V8_CONTEXT`).
    intern_ctx(ctx)
}

/// Install globals that V8 provides out-of-the-box but QuickJS does not. deno's
/// `01_core.js` bootstrap reads `globalThis.console` (V8 installs it via the
/// inspector) and calls `ObjectKeys` on it; a missing `console` throws
/// `Cannot convert undefined or null to object` and aborts bootstrap. We install
/// a plain empty object — `wrapConsole` copies its (zero) keys, leaving deno's
/// own CoreConsole methods intact.
pub(crate) fn install_default_globals(ctx: *mut JSContext) {
    if ctx.is_null() {
        return;
    }
    unsafe {
        let global = JS_GetGlobalObject(ctx);
        // Only install if absent, so we don't clobber a real console later.
        let existing = JS_GetPropertyStr(ctx, global, c"console".as_ptr());
        let absent = jsv_is_undefined(&existing) || existing.tag == JS_TAG_NULL;
        JS_FreeValue(ctx, existing);
        if absent {
            let console = JS_NewObject(ctx);
            // JS_SetPropertyStr consumes the console refcount.
            JS_SetPropertyStr(ctx, global, c"console".as_ptr(), console);
        }
        // QuickJS-ng (this vendored build) ships without Intl. deno's runtime
        // bootstrap (`99_main.js`) touches `Intl` at module top level
        // (`delete Intl.v8BreakIterator`) and various web APIs reference
        // `Intl.*` formatters. Install a minimal stub so the global exists.
        let intl = JS_GetPropertyStr(ctx, global, c"Intl".as_ptr());
        let intl_absent = jsv_is_undefined(&intl) || intl.tag == JS_TAG_NULL;
        JS_FreeValue(ctx, intl);
        if intl_absent {
            install_intl_stub(ctx, global);
        }
        JS_FreeValue(ctx, global);
    }
}

/// Install a minimal `Intl` stub. QuickJS-ng has no Intl in this build; deno's
/// bootstrap and web APIs expect `Intl` plus its common formatter constructors
/// to exist. We evaluate a tiny polyfill that provides pass-through behaviour
/// (formatters echo their input / locale identity), which is enough to let
/// bootstrap proceed. Real ECMA-402 behaviour is out of scope here.
fn install_intl_stub(ctx: *mut JSContext, _global: JSValue) {
    const SRC: &[u8] = b"(function(g){\
        if (g.Intl) return;\
        function id(x){return x;}\
        function DateTimeFormat(l,o){ if(!(this instanceof DateTimeFormat)) return new DateTimeFormat(l,o); this._l=l; this._o=o; }\
        DateTimeFormat.prototype.format=function(d){ return String(new Date(d)); };\
        DateTimeFormat.prototype.formatToParts=function(d){ return [{type:'literal',value:String(new Date(d))}]; };\
        DateTimeFormat.prototype.resolvedOptions=function(){ return {locale:(this._l||'en-US'),timeZone:'UTC'}; };\
        DateTimeFormat.supportedLocalesOf=function(l){ return Array.isArray(l)?l:(l?[l]:[]); };\
        function NumberFormat(l,o){ if(!(this instanceof NumberFormat)) return new NumberFormat(l,o); this._l=l; this._o=o; }\
        NumberFormat.prototype.format=function(n){ return String(n); };\
        NumberFormat.prototype.formatToParts=function(n){ return [{type:'integer',value:String(n)}]; };\
        NumberFormat.prototype.resolvedOptions=function(){ return {locale:(this._l||'en-US')}; };\
        NumberFormat.supportedLocalesOf=function(l){ return Array.isArray(l)?l:(l?[l]:[]); };\
        function Collator(l,o){ if(!(this instanceof Collator)) return new Collator(l,o); this._l=l; }\
        Collator.prototype.compare=function(a,b){ return a<b?-1:(a>b?1:0); };\
        Collator.prototype.resolvedOptions=function(){ return {locale:(this._l||'en-US')}; };\
        Collator.supportedLocalesOf=function(l){ return Array.isArray(l)?l:(l?[l]:[]); };\
        function PluralRules(l,o){ if(!(this instanceof PluralRules)) return new PluralRules(l,o); this._l=l; }\
        PluralRules.prototype.select=function(){ return 'other'; };\
        PluralRules.prototype.resolvedOptions=function(){ return {locale:(this._l||'en-US')}; };\
        PluralRules.supportedLocalesOf=function(l){ return Array.isArray(l)?l:(l?[l]:[]); };\
        function ListFormat(l,o){ if(!(this instanceof ListFormat)) return new ListFormat(l,o); this._l=l; }\
        ListFormat.prototype.format=function(a){ return Array.isArray(a)?a.join(', '):String(a); };\
        ListFormat.prototype.resolvedOptions=function(){ return {locale:(this._l||'en-US')}; };\
        function RelativeTimeFormat(l,o){ if(!(this instanceof RelativeTimeFormat)) return new RelativeTimeFormat(l,o); this._l=l; }\
        RelativeTimeFormat.prototype.format=function(v,u){ return String(v)+' '+String(u); };\
        RelativeTimeFormat.prototype.resolvedOptions=function(){ return {locale:(this._l||'en-US')}; };\
        function Segmenter(l,o){ if(!(this instanceof Segmenter)) return new Segmenter(l,o); this._l=l; }\
        Segmenter.prototype.segment=function(s){ return String(s); };\
        Segmenter.prototype.resolvedOptions=function(){ return {locale:(this._l||'en-US')}; };\
        g.Intl={\
            DateTimeFormat:DateTimeFormat,\
            NumberFormat:NumberFormat,\
            Collator:Collator,\
            PluralRules:PluralRules,\
            ListFormat:ListFormat,\
            RelativeTimeFormat:RelativeTimeFormat,\
            Segmenter:Segmenter,\
            getCanonicalLocales:function(l){ return Array.isArray(l)?l.slice():(l?[l]:[]); },\
        };\
    })(globalThis);\0";
    unsafe {
        let r = JS_Eval(
            ctx,
            SRC.as_ptr() as *const std::os::raw::c_char,
            SRC.len() - 1,
            c"<intl-stub>".as_ptr(),
            JS_EVAL_TYPE_GLOBAL,
        );
        if r.tag == JS_TAG_EXCEPTION {
            let exc = JS_GetException(ctx);
            JS_FreeValue(ctx, exc);
        } else {
            JS_FreeValue(ctx, r);
        }
    }
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
    if ctx.is_null() {
        return ptr::null();
    }
    // JS_GetGlobalObject returns an owned (+1) reference; move it into a slot.
    let g = unsafe { JS_GetGlobalObject(ctx) };
    intern::<Object>(g)
}

// ===================================================================
// Script — Compile / Run via JS_Eval.
//
// v8 splits compile and run; QuickJS's JS_Eval(GLOBAL) does both at once.
// We model Script::Compile by stashing the *source string* in a handle and
// evaluating at Run time. The compiled-script handle is therefore just a
// re-interned copy of the source String; Run reads it back, transcodes to a C
// string, and JS_Eval's it.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Script__Compile(
    context: *const Context,
    source: *const V8String,
    _origin: *const c_void,
) -> *const crate::Script {
    let ctx = ctx_of(context);
    if ctx.is_null() || source.is_null() {
        return ptr::null();
    }
    // Re-intern the source string as the "compiled script" handle (dup, since
    // `source`'s own slot keeps its refcount).
    intern_dup::<crate::Script>(ctx, jsval_of(source))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Script__Run(
    script: *const crate::Script,
    context: *const Context,
) -> *const Value {
    let ctx = ctx_of(context);
    if ctx.is_null() || script.is_null() {
        return ptr::null();
    }
    let src_val = jsval_of(script);
    // Pull the source text out of the stashed String value.
    let mut len: usize = 0;
    let cstr = unsafe { JS_ToCStringLen(ctx, &mut len, src_val) };
    if cstr.is_null() {
        return ptr::null();
    }
    let fname = c"<eval>";
    let result = unsafe { JS_Eval(ctx, cstr, len, fname.as_ptr(), JS_EVAL_TYPE_GLOBAL) };
    unsafe { JS_FreeCString(ctx, cstr) };
    if result.tag == JS_TAG_EXCEPTION {
        if std::env::var_os("QJS_DEBUG_EXC").is_some() {
            unsafe {
                let exc = JS_GetException(ctx);
                let mut l = 0usize;
                let s = JS_ToCStringLen(ctx, &mut l, exc);
                if !s.is_null() {
                    let bytes = std::slice::from_raw_parts(s as *const u8, l);
                    eprintln!("[QJS_DEBUG_EXC] {}", String::from_utf8_lossy(bytes));
                    JS_FreeCString(ctx, s);
                }
                // Print .stack if present.
                let stk = JS_GetPropertyStr(ctx, exc, c"stack".as_ptr());
                if !jsv_is_undefined(&stk) {
                    let mut sl = 0usize;
                    let ss = JS_ToCStringLen(ctx, &mut sl, stk);
                    if !ss.is_null() {
                        let sb = std::slice::from_raw_parts(ss as *const u8, sl);
                        eprintln!("[QJS_DEBUG_STACK]\n{}", String::from_utf8_lossy(sb));
                        JS_FreeCString(ctx, ss);
                    }
                }
                JS_FreeValue(ctx, stk);
                // Re-arm so TryCatch still sees it.
                JS_Throw(ctx, exc);
            }
        }
        // Leave the exception armed in QuickJS's pending slot so a surrounding
        // v8 TryCatch (which reads `JS_HasException`/`JS_GetException`) observes
        // it. Returning a null handle signals failure to the vendored layer.
        return ptr::null();
    }
    // `result` is owned (+1); move it into an arena slot.
    intern::<Value>(result)
}
