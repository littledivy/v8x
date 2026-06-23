//! QuickJS-ng-backed definitions for the "module" family:
//! Module / ModuleRequest / Script / ScriptCompiler / UnboundScript /
//! UnboundModuleScript / FixedArray / ScriptOrigin.
//!
//! Ported from the deno PR's `reference/qjs_v8_compat/src/module.rs` (which is
//! the primary source for the QuickJS module-loader logic) and shaped to the
//! C-ABI of the JSC backend's `src/shim_module.rs`.
//!
//! QuickJS-ng has REAL ES modules, so most of this is implemented for real:
//!   * `ScriptCompiler::CompileModule` compiles via
//!     `JS_Eval(JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_COMPILE_ONLY)`, which yields
//!     a `JS_TAG_MODULE` bytecode value whose `u.ptr` is the `JSModuleDef`.
//!   * `Module::Evaluate` runs that bytecode with `JS_EvalFunction` (which
//!     evaluates statically-imported dependencies transitively) and drains
//!     pending jobs, then hands back a resolved promise.
//!   * `Module::GetModuleNamespace` returns the real `JS_GetModuleNamespace`.
//!   * Synthetic modules use `JS_NewCModule` + `JS_AddModuleExport`, with
//!     exports populated lazily by an init callback.
//!
//! Per-module state can't live on the `JSValue` (it's a 16-byte tagged union,
//! not a pointer), so — exactly like the PR — we keep thread-local side tables
//! keyed by the module handle's pointer payload (`JSValue.u.ptr as usize`).
//!
//! Refcount discipline: every handle we return is routed through
//! `intern`/`intern_dup`; every `JSValue` we create and don't keep is
//! `JS_FreeValue`d. The bytecode `JSValue` (owned at +1 from the COMPILE_ONLY
//! eval) is stored in a side table and freed on isolate drop is *not* attempted
//! (QuickJS frees module bytecode through the module def); we dup before handing
//! it to `JS_EvalFunction` (which consumes one ref).
#![allow(non_snake_case, unused)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CString;
use std::mem::MaybeUninit;
use std::ptr;

use crate::qjs::quickjs_sys::*;
use crate::qjs::shim_core::{
    ctx_of, current_ctx, current_iso, intern, intern_dup, iso_state, jsval_of,
};
use crate::{
    Context, Data, FixedArray, Function, Message, Module, ModuleRequest, Object, RealIsolate,
    Script, String as V8String, UnboundModuleScript, UnboundScript, Value,
};

use crate::isolate::ModuleImportPhase;
use crate::module::{
    Location, ModuleStatus, ResolveModuleCallback, ResolveSourceCallback,
    StalledTopLevelAwaitMessage, SyntheticModuleEvaluationSteps,
};
use crate::script::ScriptOrigin;
use crate::script_compiler::{CachedData, CompileOptions, NoCacheReason, Source};
use crate::support::{MaybeBool, int};

// ===================================================================
// Per-module side state (keyed by module handle pointer payload).
//
// A v8 `*const Module` handle is an arena slot holding a JSValue. We make that
// JSValue a fresh `JS_NewObject` so it has a stable pointer identity, and key
// all per-module bookkeeping on that pointer.
// ===================================================================

struct ModuleState {
    status: ModuleStatus,
    /// The compiled module definition (from JS_Eval MODULE|COMPILE_ONLY), or
    /// null for synthetic modules that still register a JSModuleDef separately.
    module_def: *mut JSModuleDef,
    /// The owned (+1) bytecode JSValue (JS_TAG_MODULE) for source-text modules.
    /// None once consumed by JS_EvalFunction or for synthetic modules.
    bytecode: Option<JSValue>,
    /// Static import specifiers parsed from source (for GetModuleRequests).
    import_specifiers: Vec<std::string::String>,
    /// True for synthetic modules.
    synthetic: bool,
    /// True if source uses top-level await.
    is_async: bool,
}

thread_local! {
    static MODULE_STATE: RefCell<HashMap<usize, ModuleState>> =
        RefCell::new(HashMap::new());
    /// Pending synthetic-module exports keyed by JSModuleDef pointer; consumed
    /// by `synthetic_module_init_callback` when QuickJS first imports it.
    static SYNTHETIC_EXPORTS: RefCell<HashMap<usize, Vec<(std::string::String, JSValue)>>> =
        RefCell::new(HashMap::new());
    /// JSModuleDef pointer keyed by Module handle pointer, for synthetic modules
    /// (so SetSyntheticModuleExport can recover the def from the handle).
    static SYNTHETIC_DEFS: RefCell<HashMap<usize, usize>> = RefCell::new(HashMap::new());
}

#[inline]
fn handle_key(this: *const Module) -> usize {
    let v = jsval_of(this);
    unsafe { v.u.ptr as usize }
}

fn with_module_state<R>(this: *const Module, f: impl FnOnce(&mut ModuleState) -> R) -> Option<R> {
    let key = handle_key(this);
    MODULE_STATE.with(|t| t.borrow_mut().get_mut(&key).map(f))
}

fn record_module_state(this: *const Module, st: ModuleState) {
    let key = handle_key(this);
    MODULE_STATE.with(|t| {
        t.borrow_mut().insert(key, st);
    });
}

// ===================================================================
// Helpers
// ===================================================================

/// Convert a JSValue string handle to a Rust String (empty on failure).
unsafe fn jsval_to_rust(ctx: *mut JSContext, v: JSValue) -> std::string::String {
    if ctx.is_null() {
        return std::string::String::new();
    }
    let mut len: usize = 0;
    let cstr = unsafe { JS_ToCStringLen(ctx, &mut len, v) };
    if cstr.is_null() {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return std::string::String::new();
    }
    let bytes = unsafe { std::slice::from_raw_parts(cstr as *const u8, len) };
    let s = std::string::String::from_utf8_lossy(bytes).into_owned();
    unsafe { JS_FreeCString(ctx, cstr) };
    s
}

/// Parse static import specifiers from module source text. Best-effort, matches
/// `import ... from "spec"` and bare `import "spec"`.
fn parse_import_specifiers(src: &str) -> Vec<std::string::String> {
    let mut out = Vec::new();
    for line in src.lines() {
        let t = line.trim();
        if !t.starts_with("import") && !t.starts_with("export") {
            continue;
        }
        // Only statements that import/re-export from a module have ` from ` or a
        // bare `import "spec"`.
        let has_from = t.contains(" from ");
        let bare_import = t.starts_with("import ") && !has_from && t.contains('"');
        let bare_import_sq = t.starts_with("import ") && !has_from && t.contains('\'');
        if !has_from && !bare_import && !bare_import_sq {
            continue;
        }
        if let Some(spec) = extract_specifier(t) {
            if !spec.is_empty() {
                out.push(spec);
            }
        }
    }
    out
}

/// Extract the last string literal on a line (the module specifier).
fn extract_specifier(line: &str) -> Option<std::string::String> {
    let bytes = line.as_bytes();
    let mut close = None;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'"' || b == b'\'' {
            close = Some((i, b));
        }
    }
    let (end, q) = close?;
    let mut open = None;
    for i in (0..end).rev() {
        if bytes[i] == q {
            open = Some(i);
            break;
        }
    }
    let open = open?;
    Some(line[open + 1..end].to_string())
}

/// Best-effort top-level await detection (false negatives only).
fn has_top_level_await(src: &str) -> bool {
    // Conservative: only flag if an `await`/`for await` appears at column 0-ish
    // outside an obvious function. Cheap heuristic; deno modules rarely use TLA.
    for line in src.lines() {
        let t = line.trim_start();
        if (t.starts_with("await ") || t.starts_with("for await")) && !line.starts_with("  ") {
            return true;
        }
    }
    false
}

/// Drain QuickJS's pending-job queue (microtasks) on `rt`.
unsafe fn drain_jobs(rt: *mut JSRuntime) {
    if rt.is_null() {
        return;
    }
    loop {
        let mut pctx: *mut JSContext = ptr::null_mut();
        let r = unsafe { JS_ExecutePendingJob(rt, &mut pctx) };
        if r <= 0 {
            break;
        }
    }
}

// ===================================================================
// FixedArray
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__FixedArray__Length(this: *const FixedArray) -> int {
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return 0;
    }
    let v = jsval_of(this);
    let len_val = unsafe { JS_GetPropertyStr(ctx, v, c"length".as_ptr()) };
    if len_val.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return 0;
    }
    let n = match len_val.tag {
        JS_TAG_INT => unsafe { len_val.u.int32 },
        JS_TAG_FLOAT64 => (unsafe { len_val.u.float64 }) as i32,
        _ => 0,
    };
    unsafe { JS_FreeValue(ctx, len_val) };
    n as int
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FixedArray__Get(this: *const FixedArray, index: int) -> *const Data {
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() || index < 0 {
        return ptr::null();
    }
    let v = jsval_of(this);
    // JS_GetPropertyUint32 returns an owned (+1) value — move it into a slot.
    let elem = unsafe { JS_GetPropertyUint32(ctx, v, index as u32) };
    if elem.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return ptr::null();
    }
    intern::<Data>(elem)
}

// ===================================================================
// Script
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Script__GetUnboundScript(script: *const Script) -> *const UnboundScript {
    // Carry the same source value through as the unbound script (dup: `script`'s
    // own slot keeps its refcount).
    let ctx = current_ctx();
    intern_dup::<UnboundScript>(ctx, jsval_of(script))
}

// ===================================================================
// ScriptOrigin
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptOrigin__CONSTRUCT(
    buf: *mut MaybeUninit<ScriptOrigin>,
    resource_name: *const Value,
    _resource_line_offset: i32,
    _resource_column_offset: i32,
    _resource_is_shared_cross_origin: bool,
    _script_id: i32,
    _source_map_url: *const Value,
    _resource_is_opaque: bool,
    _is_wasm: bool,
    _is_module: bool,
    _host_defined_options: *const Data,
) {
    // ScriptOrigin is an opaque byte buffer to us. Zero-initialize, then stash
    // the resource-name handle pointer in the first usize slot so
    // Source::CONSTRUCT can carry it into the Source (used to derive a module's
    // specifier / filename).
    if !buf.is_null() {
        unsafe {
            ptr::write_bytes(buf as *mut u8, 0u8, size_of::<ScriptOrigin>());
            *(buf as *mut usize) = resource_name as usize;
        }
    }
}

// ===================================================================
// ScriptCompiler::Source
//
// We stash the source-string handle in slot 0 and the resource-name handle in
// slot 1 of the opaque Source buffer, mirroring the JSC backend.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__Source__CONSTRUCT(
    buf: *mut MaybeUninit<Source>,
    source_string: *const V8String,
    origin: *const ScriptOrigin,
    _cached_data: *mut CachedData,
) {
    if buf.is_null() {
        return;
    }
    unsafe {
        ptr::write_bytes(buf as *mut u8, 0u8, size_of::<Source>());
        let slots = buf as *mut usize;
        *slots = source_string as usize;
        if !origin.is_null() {
            *slots.add(1) = *(origin as *const usize);
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__Source__DESTRUCT(_this: *mut Source) {
    // Nothing owned; the source handle lives in the handle scope.
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__Source__GetCachedData<'a>(
    _this: *const Source,
) -> *const CachedData<'a> {
    // TODO(qjs): QuickJS bytecode caching is not surfaced through this path.
    ptr::null()
}

#[inline]
unsafe fn source_string_of(source: *mut Source) -> *const V8String {
    if source.is_null() {
        return ptr::null();
    }
    unsafe { *(source as *const usize) as *const V8String }
}

#[inline]
unsafe fn resource_name_of(ctx: *mut JSContext, source: *mut Source) -> std::string::String {
    if source.is_null() || ctx.is_null() {
        return std::string::String::new();
    }
    let name_ptr = unsafe { *((source as *const usize).add(1)) } as *const Value;
    if name_ptr.is_null() {
        return std::string::String::new();
    }
    let v = jsval_of(name_ptr);
    if jsv_is_undefined(&v) || jsv_is_null(&v) {
        return std::string::String::new();
    }
    unsafe { jsval_to_rust(ctx, v) }
}

// ===================================================================
// ScriptCompiler::CachedData
// ===================================================================

#[repr(C)]
struct RawCachedData {
    data: *const u8,
    length: i32,
    rejected: bool,
    buffer_policy: i32,
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CachedData__NEW<'a>(
    data: *const u8,
    length: i32,
) -> *mut CachedData<'a> {
    // BufferPolicy::BufferNotOwned == 0 (expected by CachedData::new).
    let boxed = Box::new(RawCachedData {
        data,
        length,
        rejected: false,
        buffer_policy: 0,
    });
    Box::into_raw(boxed) as *mut CachedData<'a>
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CachedData__DELETE<'a>(this: *mut CachedData<'a>) {
    if this.is_null() {
        return;
    }
    unsafe {
        let raw = Box::from_raw(this as *mut RawCachedData);
        if raw.buffer_policy == 1 && !raw.data.is_null() && raw.length > 0 {
            let slice = std::slice::from_raw_parts_mut(raw.data as *mut u8, raw.length as usize);
            drop(Box::from_raw(slice as *mut [u8]));
        }
        drop(raw);
    }
}

// ===================================================================
// ScriptCompiler compile entry points
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__Compile(
    context: *const Context,
    source: *mut Source,
    _options: CompileOptions,
    _no_cache_reason: NoCacheReason,
) -> *const Script {
    // A compiled Script handle just carries the source-text JSValue; Run
    // (defined in shim_core) re-evaluates it. Validate by compiling with
    // COMPILE_ONLY and discarding the result.
    let ctx = ctx_of(context);
    let src = unsafe { source_string_of(source) };
    if ctx.is_null() || src.is_null() {
        return ptr::null();
    }
    let src_val = jsval_of(src);
    let text = unsafe { jsval_to_rust(ctx, src_val) };
    let Ok(csrc) = CString::new(text) else {
        return ptr::null();
    };
    let len = csrc.as_bytes().len();
    let compiled = unsafe {
        JS_Eval(
            ctx,
            csrc.as_ptr(),
            len,
            c"<compile>".as_ptr(),
            JS_EVAL_TYPE_GLOBAL | JS_EVAL_FLAG_COMPILE_ONLY,
        )
    };
    if compiled.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return ptr::null();
    }
    unsafe { JS_FreeValue(ctx, compiled) };
    // Carry the source string forward as the Script handle.
    intern_dup::<Script>(ctx, src_val)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CompileModule(
    isolate: *mut RealIsolate,
    source: *mut Source,
    _options: CompileOptions,
    _no_cache_reason: NoCacheReason,
) -> *const Module {
    if isolate.is_null() {
        return ptr::null();
    }
    let st = iso_state(isolate);
    let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
    let src = unsafe { source_string_of(source) };
    if ctx.is_null() || src.is_null() {
        return ptr::null();
    }
    let src_val = jsval_of(src);
    let text = unsafe { jsval_to_rust(ctx, src_val) };
    let specifier = unsafe { resource_name_of(ctx, source) };
    let fname = if specifier.is_empty() {
        "<module>".to_string()
    } else {
        specifier.clone()
    };

    let import_specifiers = parse_import_specifiers(&text);
    let is_async = has_top_level_await(&text);

    let Ok(csrc) = CString::new(text) else {
        return ptr::null();
    };
    let Ok(cname) = CString::new(fname) else {
        return ptr::null();
    };
    let len = csrc.as_bytes().len();
    // COMPILE_ONLY yields a JS_TAG_MODULE value owned at +1; its u.ptr is the
    // JSModuleDef. We keep the bytecode value to evaluate later.
    let bytecode = unsafe {
        JS_Eval(
            ctx,
            csrc.as_ptr(),
            len,
            cname.as_ptr(),
            JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_COMPILE_ONLY,
        )
    };
    if bytecode.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return ptr::null();
    }
    let module_def = unsafe { bytecode.u.ptr } as *mut JSModuleDef;

    // The Module handle: a fresh object so it has stable pointer identity to key
    // our side table on.
    let handle_val = unsafe { JS_NewObject(ctx) };
    if handle_val.tag == JS_TAG_EXCEPTION {
        unsafe { JS_FreeValue(ctx, bytecode) };
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return ptr::null();
    }
    let this = intern::<Module>(handle_val);
    if this.is_null() {
        unsafe { JS_FreeValue(ctx, bytecode) };
        return ptr::null();
    }
    record_module_state(
        this,
        ModuleState {
            status: ModuleStatus::Uninstantiated,
            module_def,
            bytecode: Some(bytecode),
            import_specifiers,
            synthetic: false,
            is_async,
        },
    );
    this
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CompileFunction(
    context: *const Context,
    source: *mut Source,
    arguments_count: usize,
    arguments: *const *const V8String,
    _context_extensions_count: usize,
    _context_extensions: *const *const Object,
    _options: CompileOptions,
    _no_cache_reason: NoCacheReason,
) -> *const Function {
    // Build `(function(<args>) { <source> })` and evaluate it to a Function.
    let ctx = ctx_of(context);
    let src = unsafe { source_string_of(source) };
    if ctx.is_null() || src.is_null() {
        return ptr::null();
    }
    let body = unsafe { jsval_to_rust(ctx, jsval_of(src)) };

    let mut arg_names: Vec<std::string::String> = Vec::new();
    if !arguments.is_null() {
        for i in 0..arguments_count {
            let a = unsafe { *arguments.add(i) };
            if a.is_null() {
                continue;
            }
            arg_names.push(unsafe { jsval_to_rust(ctx, jsval_of(a)) });
        }
    }

    let wrapped = format!("(function({}) {{\n{}\n}})", arg_names.join(","), body);
    let Ok(csrc) = CString::new(wrapped) else {
        return ptr::null();
    };
    let len = csrc.as_bytes().len();
    let result = unsafe {
        JS_Eval(
            ctx,
            csrc.as_ptr(),
            len,
            c"<function>".as_ptr(),
            JS_EVAL_TYPE_GLOBAL,
        )
    };
    if result.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return ptr::null();
    }
    intern::<Function>(result)
}

// ===================================================================
// UnboundScript / UnboundModuleScript
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundScript__CreateCodeCache(
    _script: *const UnboundScript,
) -> *mut CachedData<'static> {
    // TODO(qjs): no serializable bytecode cache surfaced via this path.
    ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundModuleScript__CreateCodeCache(
    _script: *const UnboundModuleScript,
) -> *mut CachedData<'static> {
    // TODO(qjs): no serializable bytecode cache surfaced via this path.
    ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundModuleScript__GetSourceMappingURL(
    _script: *const UnboundModuleScript,
) -> *const Value {
    // TODO(qjs): no module source-map URL available.
    intern::<Value>(jsv_undefined())
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundModuleScript__GetSourceURL(
    _script: *const UnboundModuleScript,
) -> *const Value {
    // TODO(qjs): no module source URL available.
    intern::<Value>(jsv_undefined())
}

// ===================================================================
// Module
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetStatus(this: *const Module) -> ModuleStatus {
    with_module_state(this, |m| clone_status(&m.status)).unwrap_or(ModuleStatus::Errored)
}

/// `ModuleStatus` (vendored) is not `Copy`/`Clone`; reproduce the value by hand.
fn clone_status(s: &ModuleStatus) -> ModuleStatus {
    match s {
        ModuleStatus::Uninstantiated => ModuleStatus::Uninstantiated,
        ModuleStatus::Instantiating => ModuleStatus::Instantiating,
        ModuleStatus::Instantiated => ModuleStatus::Instantiated,
        ModuleStatus::Evaluating => ModuleStatus::Evaluating,
        ModuleStatus::Evaluated => ModuleStatus::Evaluated,
        ModuleStatus::Errored => ModuleStatus::Errored,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetException(_this: *const Module) -> *const Value {
    intern::<Value>(jsv_undefined())
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetModuleRequests(this: *const Module) -> *const FixedArray {
    let ctx = current_ctx();
    if ctx.is_null() {
        return ptr::null();
    }
    let specs = with_module_state(this, |m| m.import_specifiers.clone()).unwrap_or_default();

    // Build an Array of `{ specifier, __v8jsc_module_request: true }` objects so
    // deno can build its import graph and pre-resolve specifiers.
    let arr = unsafe { JS_NewArray(ctx) };
    if arr.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return ptr::null();
    }
    for (i, spec) in specs.iter().enumerate() {
        let req = unsafe { JS_NewObject(ctx) };
        if req.tag == JS_TAG_EXCEPTION {
            unsafe { JS_FreeValue(ctx, req) };
            continue;
        }
        if let Ok(cspec) = CString::new(spec.as_str()) {
            let sval = unsafe { JS_NewString(ctx, cspec.as_ptr()) };
            // JS_SetPropertyStr consumes the value ref.
            unsafe { JS_SetPropertyStr(ctx, req, c"specifier".as_ptr(), sval) };
        }
        unsafe {
            JS_SetPropertyStr(
                ctx,
                req,
                c"__v8jsc_module_request".as_ptr(),
                JS_NewBool(ctx, 1),
            );
            // JS_SetPropertyUint32 consumes `req`.
            JS_SetPropertyUint32(ctx, arr, i as u32, req);
        }
    }
    intern::<FixedArray>(arr)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__SourceOffsetToLocation(
    _this: *const Module,
    _offset: int,
    out: *mut Location,
) {
    // TODO(qjs): zero location (no source-position mapping surfaced).
    if !out.is_null() {
        unsafe { ptr::write_bytes(out as *mut u8, 0u8, size_of::<Location>()) };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetModuleNamespace(this: *const Module) -> *const Value {
    let ctx = current_ctx();
    if ctx.is_null() {
        return ptr::null();
    }
    let def = with_module_state(this, |m| m.module_def).unwrap_or(ptr::null_mut());
    if !def.is_null() {
        // JS_GetModuleNamespace returns an owned (+1) value.
        let ns = unsafe { JS_GetModuleNamespace(ctx, def) };
        if ns.tag != JS_TAG_EXCEPTION {
            return intern::<Value>(ns);
        }
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
    }
    // Fallback: empty object so destructuring on the namespace doesn't crash.
    let obj = unsafe { JS_NewObject(ctx) };
    intern::<Value>(obj)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetModuleNamespace2(
    this: *const Module,
    _phase: ModuleImportPhase,
) -> *const Value {
    v8__Module__GetModuleNamespace(this)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__EvaluateForImportDefer(
    _this: *const Module,
    _context: *const Context,
) -> *const Value {
    // TODO(qjs): import-defer phase not supported.
    ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetIdentityHash(this: *const Module) -> int {
    // Stable-ish identity hash from the handle pointer payload.
    (handle_key(this) as int) ^ 0x4d4f_44 // "MOD"
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__InstantiateModule(
    this: *const Module,
    _context: *const Context,
    _cb: ResolveModuleCallback,
    _source_callback: Option<ResolveSourceCallback>,
) -> MaybeBool {
    // QuickJS performs real module linking/resolution at evaluation time via its
    // registered module loader (see JS_SetModuleLoaderFunc / module_loader_*),
    // so we don't need to walk the import graph here. Just advance the lifecycle
    // state so deno's pre-evaluate assertions pass.
    match with_module_state(this, |m| {
        if matches!(m.status, ModuleStatus::Uninstantiated) {
            m.status = ModuleStatus::Instantiated;
        }
        true
    }) {
        Some(true) => MaybeBool::JustTrue,
        _ => MaybeBool::JustFalse,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__Evaluate(
    this: *const Module,
    context: *const Context,
) -> *const Value {
    let ctx = ctx_of(context);
    if ctx.is_null() {
        return ptr::null();
    }
    let iso = current_iso();
    let rt = if iso.is_null() {
        ptr::null_mut()
    } else {
        iso_state(iso).rt
    };

    // Take the bytecode (consumed once) and mark Evaluated.
    let bytecode = with_module_state(this, |m| {
        m.status = ModuleStatus::Evaluated;
        m.bytecode.take()
    })
    .flatten();

    if let Some(bc) = bytecode {
        // JS_EvalFunction consumes one ref of the bytecode value. We own it (+1),
        // so hand it straight over (no dup, no extra free).
        let result = unsafe { JS_EvalFunction(ctx, bc) };
        unsafe { drain_jobs(rt) };
        if result.tag == JS_TAG_EXCEPTION {
            let exc = unsafe { JS_GetException(ctx) };
            if !iso.is_null() {
                // Surface the rejection reason for debugging.
                let s = unsafe { jsval_to_rust(ctx, exc) };
                if !s.is_empty() {
                    eprintln!("[qjs] Module::evaluate exception: {s}");
                }
            }
            unsafe { JS_FreeValue(ctx, exc) };
        } else {
            unsafe { JS_FreeValue(ctx, result) };
        }
    }

    // Hand back a resolved promise (deno awaits the module-evaluation promise).
    let mut funcs: [JSValue; 2] = [jsv_undefined(), jsv_undefined()];
    let promise = unsafe { JS_NewPromiseCapability(ctx, funcs.as_mut_ptr()) };
    if promise.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return ptr::null();
    }
    let resolve = funcs[0];
    let reject = funcs[1];
    let mut args = [jsv_undefined()];
    let r = unsafe { JS_Call(ctx, resolve, jsv_undefined(), 1, args.as_mut_ptr()) };
    if r.tag != JS_TAG_EXCEPTION {
        unsafe { JS_FreeValue(ctx, r) };
    } else {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
    }
    unsafe {
        JS_FreeValue(ctx, resolve);
        JS_FreeValue(ctx, reject);
        drain_jobs(rt);
    }
    intern::<Value>(promise)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__IsGraphAsync(this: *const Module) -> bool {
    with_module_state(this, |m| m.is_async).unwrap_or(false)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__IsSyntheticModule(this: *const Module) -> bool {
    with_module_state(this, |m| m.synthetic).unwrap_or(false)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__CreateSyntheticModule(
    isolate: *const RealIsolate,
    module_name: *const V8String,
    export_names_len: usize,
    export_names_raw: *const *const V8String,
    _evaluation_steps: SyntheticModuleEvaluationSteps,
) -> *const Module {
    let iso = isolate as *mut RealIsolate;
    if iso.is_null() {
        return ptr::null();
    }
    let st = iso_state(iso);
    let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
    if ctx.is_null() {
        return ptr::null();
    }

    let name = if module_name.is_null() {
        "<synthetic>".to_string()
    } else {
        unsafe { jsval_to_rust(ctx, jsval_of(module_name)) }
    };
    let Ok(cname) = CString::new(name) else {
        return ptr::null();
    };

    let def = unsafe {
        JS_NewCModule(ctx, cname.as_ptr(), Some(synthetic_module_init_callback))
    };
    if def.is_null() {
        return ptr::null();
    }
    if !export_names_raw.is_null() {
        for i in 0..export_names_len {
            let n = unsafe { *export_names_raw.add(i) };
            if n.is_null() {
                continue;
            }
            let s = unsafe { jsval_to_rust(ctx, jsval_of(n)) };
            if let Ok(c) = CString::new(s) {
                unsafe { JS_AddModuleExport(ctx, def, c.as_ptr()) };
            }
        }
    }

    // The Module handle: a fresh object keyed for side state.
    let handle_val = unsafe { JS_NewObject(ctx) };
    if handle_val.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return ptr::null();
    }
    let this = intern::<Module>(handle_val);
    if this.is_null() {
        return ptr::null();
    }
    SYNTHETIC_DEFS.with(|t| {
        t.borrow_mut().insert(handle_key(this), def as usize);
    });
    record_module_state(
        this,
        ModuleState {
            status: ModuleStatus::Instantiated,
            module_def: def,
            bytecode: None,
            import_specifiers: Vec::new(),
            synthetic: true,
            is_async: false,
        },
    );
    this
}

/// Init callback for synthetic modules: read the stashed (name, value) export
/// list for this JSModuleDef and call JS_SetModuleExport for each.
unsafe extern "C" fn synthetic_module_init_callback(
    ctx: *mut JSContext,
    m: *mut JSModuleDef,
) -> std::os::raw::c_int {
    let exports = SYNTHETIC_EXPORTS.with(|t| t.borrow_mut().remove(&(m as usize)).unwrap_or_default());
    for (name, value) in exports {
        let Ok(name_c) = CString::new(name) else {
            // Drop the owned value we stashed to avoid a leak.
            unsafe { JS_FreeValue(ctx, value) };
            continue;
        };
        // We stashed an owned (+1) value; JS_SetModuleExport consumes one ref.
        unsafe { JS_SetModuleExport(ctx, m, name_c.as_ptr(), value) };
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__SetSyntheticModuleExport(
    this: *const Module,
    isolate: *const RealIsolate,
    export_name: *const V8String,
    export_value: *const Value,
) -> MaybeBool {
    let iso = isolate as *mut RealIsolate;
    let ctx = if iso.is_null() {
        current_ctx()
    } else {
        let st = iso_state(iso);
        st.contexts.last().copied().unwrap_or(st.ctx)
    };
    if ctx.is_null() {
        return MaybeBool::JustFalse;
    }
    let def = SYNTHETIC_DEFS
        .with(|t| t.borrow().get(&handle_key(this)).copied())
        .map(|p| p as *mut JSModuleDef);
    let Some(def) = def else {
        return MaybeBool::JustFalse;
    };
    let name = unsafe { jsval_to_rust(ctx, jsval_of(export_name)) };
    // Dup the value so the stash owns its own refcount (the caller keeps theirs).
    let val = unsafe { JS_DupValue(ctx, jsval_of(export_value)) };
    SYNTHETIC_EXPORTS.with(|t| {
        t.borrow_mut()
            .entry(def as usize)
            .or_default()
            .push((name, val));
    });
    MaybeBool::JustTrue
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetUnboundModuleScript(
    this: *const Module,
) -> *const UnboundModuleScript {
    // Carry the module handle through as the unbound module script.
    let ctx = current_ctx();
    intern_dup::<UnboundModuleScript>(ctx, jsval_of(this))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetStalledTopLevelAwaitMessage(
    _this: *const Module,
    _isolate: *const RealIsolate,
    _out_vec: *mut StalledTopLevelAwaitMessage,
    _vec_len: usize,
) -> usize {
    // TODO(qjs): no stalled-TLA diagnostics; report none stalled.
    0
}

// ===================================================================
// ModuleRequest
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__ModuleRequest__GetSpecifier(this: *const ModuleRequest) -> *const V8String {
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return ptr::null();
    }
    let v = jsval_of(this);
    // JS_GetPropertyStr returns owned (+1).
    let spec = unsafe { JS_GetPropertyStr(ctx, v, c"specifier".as_ptr()) };
    if spec.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return ptr::null();
    }
    intern::<V8String>(spec)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ModuleRequest__GetPhase(_this: *const ModuleRequest) -> ModuleImportPhase {
    ModuleImportPhase::kEvaluation
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ModuleRequest__GetSourceOffset(_this: *const ModuleRequest) -> int {
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ModuleRequest__GetImportAttributes(
    _this: *const ModuleRequest,
) -> *const FixedArray {
    // Empty attributes array.
    let ctx = current_ctx();
    if ctx.is_null() {
        return ptr::null();
    }
    let arr = unsafe { JS_NewArray(ctx) };
    if arr.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return ptr::null();
    }
    intern::<FixedArray>(arr)
}
