//! JSC-backed definitions for the "module" family:
//! Module / ModuleRequest / Script / ScriptCompiler / UnboundScript /
//! UnboundModuleScript / FixedArray / ScriptOrigin.
//!
//! Script compilation/execution is implemented for real via JSEvaluateScript:
//! a `Script`/`UnboundScript` handle simply carries the source-text JSValueRef,
//! and `Run` re-stringifies it and evaluates it in the current context.
//! ES-module-specific functions are inert because the JSC C API exposes no
//! module loader / linker.
#![allow(non_snake_case, unused)]

use std::mem::MaybeUninit;
use std::ptr;

use crate::jsc_sys::*;
use crate::shim_core::{ctx_of, current_ctx, current_iso, intern, intern_ctx, iso_state, jsval};
use crate::{
    Context, Data, FixedArray, Function, Message, Module, ModuleRequest, Object, Primitive,
    RealIsolate, Script, String as V8String, UnboundModuleScript, UnboundScript, Value,
};

use crate::isolate::ModuleImportPhase;
use crate::module::{
    Location, ModuleStatus, ResolveModuleCallback, ResolveSourceCallback,
    StalledTopLevelAwaitMessage, SyntheticModuleEvaluationSteps,
};
use crate::script::ScriptOrigin;
use crate::script_compiler::{CachedData, CompileOptions, NoCacheReason, Source};
use crate::support::{Maybe, MaybeBool, int};

// Extra JSC C functions not declared in jsc_sys.rs.
unsafe extern "C" {
    fn JSValueToObject(
        ctx: JSContextRef,
        value: JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSObjectRef;
    fn JSObjectGetPropertyAtIndex(
        ctx: JSContextRef,
        object: JSObjectRef,
        property_index: u32,
        exception: *mut JSValueRef,
    ) -> JSValueRef;
    fn JSObjectGetProperty(
        ctx: JSContextRef,
        object: JSObjectRef,
        property_name: JSStringRef,
        exception: *mut JSValueRef,
    ) -> JSValueRef;
}

// ===================================================================
// Helpers
// ===================================================================

/// Stringify a JSValueRef and run it as a script in `ctx`. Returns the result
/// JSValueRef (or null on failure).
unsafe fn eval_value_as_script(ctx: JSContextRef, source_val: JSValueRef) -> JSValueRef {
    if ctx.is_null() || source_val.is_null() {
        return ptr::null();
    }
    let mut exc: JSValueRef = ptr::null();
    let src_str = unsafe { JSValueToStringCopy(ctx, source_val, &mut exc) };
    if src_str.is_null() {
        return ptr::null();
    }
    let result = unsafe {
        JSEvaluateScript(
            ctx,
            src_str,
            ptr::null_mut(),
            ptr::null_mut(),
            1,
            &mut exc,
        )
    };
    unsafe { JSStringRelease(src_str) };
    result
}

// ===================================================================
// FixedArray
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__FixedArray__Length(this: *const FixedArray) -> int {
    let ctx = current_ctx();
    let v = jsval(this);
    if ctx.is_null() || v.is_null() {
        return 0;
    }
    unsafe {
        let mut exc: JSValueRef = ptr::null();
        let obj = JSValueToObject(ctx, v, &mut exc);
        if obj.is_null() {
            return 0;
        }
        let name = JSStringCreateWithUTF8CString(c"length".as_ptr());
        let len_val = JSObjectGetProperty(ctx, obj, name, &mut exc);
        JSStringRelease(name);
        if len_val.is_null() {
            return 0;
        }
        JSValueToNumber(ctx, len_val, &mut exc) as int
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__FixedArray__Get(this: *const FixedArray, index: int) -> *const Data {
    let ctx = current_ctx();
    let v = jsval(this);
    if ctx.is_null() || v.is_null() || index < 0 {
        return ptr::null();
    }
    unsafe {
        let mut exc: JSValueRef = ptr::null();
        let obj = JSValueToObject(ctx, v, &mut exc);
        if obj.is_null() {
            return ptr::null();
        }
        let elem = JSObjectGetPropertyAtIndex(ctx, obj, index as u32, &mut exc);
        intern_ctx::<Data>(ctx, elem)
    }
}

// ===================================================================
// Script
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Script__Compile(
    context: *const Context,
    source: *const V8String,
    _origin: *const ScriptOrigin,
) -> *const Script {
    // The compiled Script handle simply carries the source-text JSValueRef.
    // Validate syntax; if it fails, return null so Deno sees a compile error.
    let ctx = ctx_of(context) as JSContextRef;
    let src_val = jsval(source);
    if ctx.is_null() || src_val.is_null() {
        return ptr::null();
    }
    unsafe {
        let mut exc: JSValueRef = ptr::null();
        let src_str = JSValueToStringCopy(ctx, src_val, &mut exc);
        if src_str.is_null() {
            return ptr::null();
        }
        let ok = JSCheckScriptSyntax(ctx, src_str, ptr::null_mut(), 1, &mut exc);
        JSStringRelease(src_str);
        if !ok {
            return ptr::null();
        }
    }
    intern_ctx::<Script>(ctx, src_val)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Script__GetUnboundScript(script: *const Script) -> *const UnboundScript {
    // Carry the same source value through as the unbound script.
    intern::<UnboundScript>(jsval(script))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Script__Run(
    script: *const Script,
    context: *const Context,
) -> *const Value {
    let ctx = ctx_of(context) as JSContextRef;
    let result = unsafe { eval_value_as_script(ctx, jsval(script)) };
    if result.is_null() {
        return ptr::null();
    }
    intern_ctx::<Value>(ctx, result)
}

// ===================================================================
// ScriptOrigin
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptOrigin__CONSTRUCT(
    buf: *mut MaybeUninit<ScriptOrigin>,
    _resource_name: *const Value,
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
    // ScriptOrigin is an opaque byte buffer to us; JSC doesn't consume origin
    // metadata through the C API. Zero-initialize so it is a valid value.
    if !buf.is_null() {
        unsafe {
            ptr::write_bytes(buf as *mut u8, 0u8, size_of::<ScriptOrigin>());
        }
    }
}

// ===================================================================
// ScriptCompiler::Source
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__Source__CONSTRUCT(
    buf: *mut MaybeUninit<Source>,
    source_string: *const V8String,
    _origin: *const ScriptOrigin,
    _cached_data: *mut CachedData,
) {
    if buf.is_null() {
        return;
    }
    unsafe {
        // Zero the whole struct, then stash the source-string handle in the
        // first field (_source_string) so Compile* can recover it.
        ptr::write_bytes(buf as *mut u8, 0u8, size_of::<Source>());
        let first = buf as *mut usize;
        *first = source_string as usize;
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
    // TODO(v82jsc): JSC has no bytecode cache exposed via the C API.
    ptr::null()
}

#[inline]
unsafe fn source_string_of(source: *mut Source) -> JSValueRef {
    if source.is_null() {
        return ptr::null();
    }
    unsafe { *(source as *const usize) as JSValueRef }
}

// ===================================================================
// ScriptCompiler::CachedData
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CachedData__NEW<'a>(
    data: *const u8,
    length: i32,
) -> *mut CachedData<'a> {
    // CachedData layout: { data: *const u8, length: i32, rejected: bool,
    //                      buffer_policy: BufferPolicy, _phantom }.
    // BufferPolicy::BufferNotOwned == 0 (expected by CachedData::new).
    #[repr(C)]
    struct RawCachedData {
        data: *const u8,
        length: i32,
        rejected: bool,
        buffer_policy: i32,
    }
    let boxed = Box::new(RawCachedData {
        data,
        length,
        rejected: false,
        buffer_policy: 0, // BufferNotOwned
    });
    Box::into_raw(boxed) as *mut CachedData<'a>
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CachedData__DELETE<'a>(this: *mut CachedData<'a>) {
    if this.is_null() {
        return;
    }
    #[repr(C)]
    struct RawCachedData {
        data: *const u8,
        length: i32,
        rejected: bool,
        buffer_policy: i32,
    }
    unsafe {
        drop(Box::from_raw(this as *mut RawCachedData));
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
    let ctx = ctx_of(context) as JSContextRef;
    let src_val = unsafe { source_string_of(source) };
    if ctx.is_null() || src_val.is_null() {
        return ptr::null();
    }
    unsafe {
        let mut exc: JSValueRef = ptr::null();
        let src_str = JSValueToStringCopy(ctx, src_val, &mut exc);
        if src_str.is_null() {
            return ptr::null();
        }
        let ok = JSCheckScriptSyntax(ctx, src_str, ptr::null_mut(), 1, &mut exc);
        JSStringRelease(src_str);
        if !ok {
            return ptr::null();
        }
    }
    intern_ctx::<Script>(ctx, src_val)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CompileModule(
    _isolate: *mut RealIsolate,
    _source: *mut Source,
    _options: CompileOptions,
    _no_cache_reason: NoCacheReason,
) -> *const Module {
    // TODO(v82jsc): JSC C API has no ES-module parsing/linking primitives.
    ptr::null()
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
    let ctx = ctx_of(context) as JSContextRef;
    let src_val = unsafe { source_string_of(source) };
    if ctx.is_null() || src_val.is_null() {
        return ptr::null();
    }
    unsafe {
        let mut exc: JSValueRef = ptr::null();
        // Recover the source text.
        let src_str = JSValueToStringCopy(ctx, src_val, &mut exc);
        if src_str.is_null() {
            return ptr::null();
        }
        let max = JSStringGetMaximumUTF8CStringSize(src_str);
        let mut body = vec![0u8; max];
        JSStringGetUTF8CString(src_str, body.as_mut_ptr() as *mut _, max);
        JSStringRelease(src_str);
        let body_str = std::ffi::CStr::from_ptr(body.as_ptr() as *const _)
            .to_string_lossy()
            .into_owned();

        // Collect argument names.
        let mut arg_names: Vec<std::string::String> = Vec::new();
        if !arguments.is_null() {
            for i in 0..arguments_count {
                let a = *arguments.add(i);
                let av = jsval(a);
                if av.is_null() {
                    continue;
                }
                let s = JSValueToStringCopy(ctx, av, &mut exc);
                if s.is_null() {
                    continue;
                }
                let m = JSStringGetMaximumUTF8CStringSize(s);
                let mut nbuf = vec![0u8; m];
                JSStringGetUTF8CString(s, nbuf.as_mut_ptr() as *mut _, m);
                JSStringRelease(s);
                arg_names.push(
                    std::ffi::CStr::from_ptr(nbuf.as_ptr() as *const _)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }

        let wrapped = format!("(function({}) {{\n{}\n}})", arg_names.join(","), body_str);
        let cstr = match std::ffi::CString::new(wrapped) {
            Ok(c) => c,
            Err(_) => return ptr::null(),
        };
        let js_src = JSStringCreateWithUTF8CString(cstr.as_ptr());
        let result = JSEvaluateScript(ctx, js_src, ptr::null_mut(), ptr::null_mut(), 1, &mut exc);
        JSStringRelease(js_src);
        if result.is_null() {
            return ptr::null();
        }
        intern_ctx::<Function>(ctx, result)
    }
}

// ===================================================================
// UnboundScript / UnboundModuleScript
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundScript__CreateCodeCache(
    _script: *const UnboundScript,
) -> *mut CachedData<'static> {
    // TODO(v82jsc): no serializable bytecode cache via JSC C API.
    ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundModuleScript__CreateCodeCache(
    _script: *const UnboundModuleScript,
) -> *mut CachedData<'static> {
    // TODO(v82jsc): no serializable bytecode cache via JSC C API.
    ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundModuleScript__GetSourceMappingURL(
    _script: *const UnboundModuleScript,
) -> *const Value {
    // TODO(v82jsc): no module source-map URL available.
    let ctx = current_ctx();
    if ctx.is_null() {
        return ptr::null();
    }
    let v = unsafe { JSValueMakeUndefined(ctx) };
    intern_ctx::<Value>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundModuleScript__GetSourceURL(
    _script: *const UnboundModuleScript,
) -> *const Value {
    // TODO(v82jsc): no module source URL available.
    let ctx = current_ctx();
    if ctx.is_null() {
        return ptr::null();
    }
    let v = unsafe { JSValueMakeUndefined(ctx) };
    intern_ctx::<Value>(ctx, v)
}

// ===================================================================
// Module (ES modules — inert, JSC C API has no module loader)
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetStatus(_this: *const Module) -> ModuleStatus {
    // TODO(v82jsc): no module support; report Errored so callers don't proceed.
    ModuleStatus::Errored
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetException(_this: *const Module) -> *const Value {
    let ctx = current_ctx();
    if ctx.is_null() {
        return ptr::null();
    }
    let v = unsafe { JSValueMakeUndefined(ctx) };
    intern_ctx::<Value>(ctx, v)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetModuleRequests(_this: *const Module) -> *const FixedArray {
    // TODO(v82jsc): no module requests; return null.
    ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__SourceOffsetToLocation(
    _this: *const Module,
    _offset: int,
    out: *mut Location,
) {
    // TODO(v82jsc): zero location.
    if !out.is_null() {
        unsafe { ptr::write_bytes(out as *mut u8, 0u8, size_of::<Location>()) };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetModuleNamespace(_this: *const Module) -> *const Value {
    // TODO(v82jsc): no module support.
    ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetModuleNamespace2(
    _this: *const Module,
    _phase: ModuleImportPhase,
) -> *const Value {
    // TODO(v82jsc): no module support.
    ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__EvaluateForImportDefer(
    _this: *const Module,
    _context: *const Context,
) -> *const Value {
    // TODO(v82jsc): no module support.
    ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetIdentityHash(_this: *const Module) -> int {
    // A stable-ish identity hash from the pointer value.
    (_this as usize as int) ^ 0x4d4f_44 // "MOD"
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__InstantiateModule(
    _this: *const Module,
    _context: *const Context,
    _cb: ResolveModuleCallback,
    _source_callback: Option<ResolveSourceCallback>,
) -> MaybeBool {
    // TODO(v82jsc): no module linking. Report failure.
    MaybeBool::JustFalse
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__Evaluate(
    _this: *const Module,
    _context: *const Context,
) -> *const Value {
    // TODO(v82jsc): no module evaluation.
    ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__IsGraphAsync(_this: *const Module) -> bool {
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__IsSyntheticModule(_this: *const Module) -> bool {
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__CreateSyntheticModule(
    _isolate: *const RealIsolate,
    _module_name: *const V8String,
    _export_names_len: usize,
    _export_names_raw: *const *const V8String,
    _evaluation_steps: SyntheticModuleEvaluationSteps,
) -> *const Module {
    // TODO(v82jsc): no synthetic module support.
    ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__SetSyntheticModuleExport(
    _this: *const Module,
    _isolate: *const RealIsolate,
    _export_name: *const V8String,
    _export_value: *const Value,
) -> MaybeBool {
    // TODO(v82jsc): no synthetic module support.
    MaybeBool::JustFalse
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetUnboundModuleScript(
    _this: *const Module,
) -> *const UnboundModuleScript {
    // TODO(v82jsc): no module support.
    ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetStalledTopLevelAwaitMessage(
    _this: *const Module,
    _isolate: *const RealIsolate,
    _out_vec: *mut StalledTopLevelAwaitMessage,
    _vec_len: usize,
) -> usize {
    // TODO(v82jsc): no module support; no stalled messages.
    0
}

// ===================================================================
// ModuleRequest (inert)
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__ModuleRequest__GetSpecifier(_this: *const ModuleRequest) -> *const V8String {
    // TODO(v82jsc): no module requests.
    ptr::null()
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
    // TODO(v82jsc): no module requests.
    ptr::null()
}
