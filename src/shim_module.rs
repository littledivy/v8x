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
    SyntheticModuleEvaluationStepsRet,
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
    fn JSObjectSetProperty(
        ctx: JSContextRef,
        object: JSObjectRef,
        propertyName: JSStringRef,
        value: JSValueRef,
        attributes: u32,
        exception: *mut JSValueRef,
    );
    fn JSObjectMake(
        ctx: JSContextRef,
        jsClass: JSClassRef,
        data: *mut std::os::raw::c_void,
    ) -> JSObjectRef;
    fn JSObjectGetPrivate(object: JSObjectRef) -> *mut std::os::raw::c_void;
    fn JSClassCreate(definition: *const ModJSClassDefinition) -> JSClassRef;
    fn JSObjectMakeArray(
        ctx: JSContextRef,
        argumentCount: usize,
        arguments: *const JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSObjectRef;
    fn JSObjectCallAsFunction(
        ctx: JSContextRef,
        object: JSObjectRef,
        thisObject: JSObjectRef,
        argumentCount: usize,
        arguments: *const JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSValueRef;
}

#[repr(C)]
struct ModJSClassDefinition {
    version: std::os::raw::c_int,
    attributes: u32,
    className: *const std::os::raw::c_char,
    parentClass: JSClassRef,
    staticValues: *const std::os::raw::c_void,
    staticFunctions: *const std::os::raw::c_void,
    initialize: *const std::os::raw::c_void,
    finalize: *const std::os::raw::c_void,
    hasProperty: *const std::os::raw::c_void,
    getProperty: *const std::os::raw::c_void,
    setProperty: *const std::os::raw::c_void,
    deleteProperty: *const std::os::raw::c_void,
    getPropertyNames: *const std::os::raw::c_void,
    callAsFunction: *const std::os::raw::c_void,
    callAsConstructor: *const std::os::raw::c_void,
    hasInstance: *const std::os::raw::c_void,
    convertToType: *const std::os::raw::c_void,
}

// ===================================================================
// Synthetic modules
//
// JSC's public C API has no module-object type, so we synthesize one: a Module
// `Local` is a JSObject carrying a boxed `SyntheticModule` as private data. The
// accessors below read that state. This is enough for deno_core's
// `ext:core/ops` virtual module, which is the only synthetic module created
// during bootstrap.
// ===================================================================

struct SyntheticModule {
    ctx: JSGlobalContextRef,
    status: ModuleStatus,
    export_names: Vec<std::string::String>,
    /// For synthetic modules: deno's evaluation steps. None for source-text
    /// modules (which carry `source` instead).
    eval_steps: Option<SyntheticModuleEvaluationSteps<'static>>,
    /// For source-text ES modules: the (rewritten) script body to evaluate.
    source: Option<std::string::String>,
    /// Static import specifiers, in source order (source-text modules).
    import_specifiers: Vec<std::string::String>,
    /// Namespace object holding the exports; protected for the module's life.
    namespace: JSObjectRef,
}

unsafe extern "C" fn mod_finalize(object: JSObjectRef) {
    let p = unsafe { JSObjectGetPrivate(object) } as *mut SyntheticModule;
    if !p.is_null() {
        let m = unsafe { Box::from_raw(p) };
        if !m.namespace.is_null() && !m.ctx.is_null() {
            unsafe { JSValueUnprotect(m.ctx, m.namespace as JSValueRef) };
        }
    }
}

thread_local! {
    static MOD_CLASS: std::cell::Cell<JSClassRef> =
        const { std::cell::Cell::new(ptr::null_mut()) };
}

fn mod_class() -> JSClassRef {
    MOD_CLASS.with(|c| {
        let existing = c.get();
        if !existing.is_null() {
            return existing;
        }
        let def = ModJSClassDefinition {
            version: 0,
            attributes: 0,
            className: c"v8jsc_module".as_ptr(),
            parentClass: ptr::null_mut(),
            staticValues: ptr::null(),
            staticFunctions: ptr::null(),
            initialize: ptr::null(),
            finalize: mod_finalize as *const std::os::raw::c_void,
            hasProperty: ptr::null(),
            getProperty: ptr::null(),
            setProperty: ptr::null(),
            deleteProperty: ptr::null(),
            getPropertyNames: ptr::null(),
            callAsFunction: ptr::null(),
            callAsConstructor: ptr::null(),
            hasInstance: ptr::null(),
            convertToType: ptr::null(),
        };
        let cls = unsafe { JSClassCreate(&def) };
        c.set(cls);
        cls
    })
}

#[inline]
fn module_state<'a>(this: *const Module) -> Option<&'a mut SyntheticModule> {
    if this.is_null() {
        return None;
    }
    let obj = jsval(this) as JSObjectRef;
    let p = unsafe { JSObjectGetPrivate(obj) } as *mut SyntheticModule;
    if p.is_null() {
        None
    } else {
        Some(unsafe { &mut *p })
    }
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
    isolate: *mut RealIsolate,
    source: *mut Source,
    _options: CompileOptions,
    _no_cache_reason: NoCacheReason,
) -> *const Module {
    let st = iso_state(isolate);
    let ctx = st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
    let src_val = unsafe { source_string_of(source) };
    if ctx.is_null() || src_val.is_null() {
        return ptr::null();
    }
    let gctx = unsafe { JSContextGetGlobalContext(ctx) };

    // Recover the source text.
    let text = unsafe {
        let mut exc: JSValueRef = ptr::null();
        let s = JSValueToStringCopy(ctx, src_val, &mut exc);
        if s.is_null() {
            return ptr::null();
        }
        let max = JSStringGetMaximumUTF8CStringSize(s);
        let mut buf = vec![0u8; max];
        let n = JSStringGetUTF8CString(s, buf.as_mut_ptr() as *mut _, max);
        JSStringRelease(s);
        if n == 0 {
            return ptr::null();
        }
        buf.truncate(n - 1);
        match std::string::String::from_utf8(buf) {
            Ok(t) => t,
            Err(_) => return ptr::null(),
        }
    };

    // Rewrite ES-module syntax into a function body that assigns exports onto
    // the `__ns` namespace object. Best-effort; supports the module shapes
    // emitted by deno_core builtins.
    let Some(rewrite) = rewrite_es_module(&text) else {
        // Unsupported module shape; signal a compile failure.
        return ptr::null();
    };

    let namespace = unsafe { JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut()) };
    unsafe { JSValueProtect(gctx, namespace as JSValueRef) };

    let state = Box::new(SyntheticModule {
        ctx: gctx,
        status: ModuleStatus::Uninstantiated,
        export_names: rewrite.export_names,
        eval_steps: None,
        source: Some(rewrite.body),
        import_specifiers: rewrite.imports,
        namespace,
    });
    let obj = unsafe {
        JSObjectMake(ctx, mod_class(), Box::into_raw(state) as *mut _)
    };
    intern_ctx::<Module>(ctx, obj as JSValueRef)
}

struct RewrittenModule {
    body: std::string::String,
    export_names: Vec<std::string::String>,
    imports: Vec<std::string::String>,
}

/// Translate a (small, TLA-free) ES module into a function body that, when run
/// as `(function(__ns){ <body> })(ns)`, evaluates the module and writes each
/// export onto `__ns`. Handles:
///   - `import { a, b } from "spec"` / `import def from "spec"` / `import "spec"`
///   - `export { a, b as c }`
///   - `export const/let/var/function/class NAME ...`
///   - `export default EXPR`
/// Imported bindings resolve through `globalThis.__v8jsc_modules[spec]`.
/// Returns None for shapes we can't safely rewrite (e.g. `export * from`).
fn rewrite_es_module(src: &str) -> Option<RewrittenModule> {
    let mut out = std::string::String::new();
    let mut export_names: Vec<std::string::String> = Vec::new();
    let mut imports: Vec<std::string::String> = Vec::new();

    for raw_line in src.lines() {
        let line = raw_line.trim_start();
        let trimmed = line.trim();

        // `export * from` is not supported.
        if trimmed.starts_with("export *") {
            return None;
        }

        // import ... from "spec";  /  import "spec";
        if trimmed.starts_with("import ") || trimmed == "import" {
            if let Some(spec) = extract_specifier(trimmed) {
                imports.push(spec.clone());
            }
            if trimmed.starts_with("import {") || trimmed.contains("import {") {
                // import { a, b as c } from "spec"
                let names = between(trimmed, '{', '}').unwrap_or_default();
                let spec = extract_specifier(trimmed).unwrap_or_default();
                out.push_str(&format!(
                    "const {{ {} }} = (globalThis.__v8jsc_modules||{{}})[{:?}]||{{}};\n",
                    names, spec
                ));
            } else if trimmed.starts_with("import ") && trimmed.contains(" from ") {
                // import def from "spec"  -> default import
                let mid = &trimmed["import ".len()..];
                let name = mid.split(" from ").next().unwrap_or("").trim();
                let spec = extract_specifier(trimmed).unwrap_or_default();
                if !name.is_empty() && !name.starts_with('{') {
                    out.push_str(&format!(
                        "const {} = ((globalThis.__v8jsc_modules||{{}})[{:?}]||{{}}).default;\n",
                        name, spec
                    ));
                }
            }
            // bare `import "spec";` -> side effect only, nothing to bind.
            continue;
        }

        // export { a, b as c };
        if trimmed.starts_with("export {") || trimmed.starts_with("export{") {
            let inner = between(trimmed, '{', '}').unwrap_or_default();
            for part in inner.split(',') {
                let part = part.trim();
                if part.is_empty() {
                    continue;
                }
                let (local, exported) = if let Some((l, r)) = part.split_once(" as ") {
                    (l.trim().to_string(), r.trim().to_string())
                } else {
                    (part.to_string(), part.to_string())
                };
                export_names.push(exported.clone());
                out.push_str(&format!("__ns[{:?}] = {};\n", exported, local));
            }
            continue;
        }

        // export default EXPR;
        if trimmed.starts_with("export default ") {
            let expr = &trimmed["export default ".len()..];
            export_names.push("default".to_string());
            out.push_str(&format!("__ns[\"default\"] = ({});\n", expr.trim_end_matches(';')));
            continue;
        }

        // export const/let/var NAME = ...   /  export function NAME  / export class NAME
        if trimmed.starts_with("export const ")
            || trimmed.starts_with("export let ")
            || trimmed.starts_with("export var ")
            || trimmed.starts_with("export function ")
            || trimmed.starts_with("export async function ")
            || trimmed.starts_with("export class ")
        {
            let rest = trimmed.strip_prefix("export ").unwrap();
            // Determine the declared name.
            let after_kw = rest
                .trim_start_matches("const ")
                .trim_start_matches("let ")
                .trim_start_matches("var ")
                .trim_start_matches("async function ")
                .trim_start_matches("function ")
                .trim_start_matches("class ");
            let name: std::string::String = after_kw
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
                .collect();
            out.push_str(rest);
            out.push('\n');
            if !name.is_empty() {
                export_names.push(name.clone());
                out.push_str(&format!("__ns[{:?}] = {};\n", name, name));
            }
            continue;
        }

        // Plain line: keep as-is.
        out.push_str(raw_line);
        out.push('\n');
    }

    Some(RewrittenModule {
        body: out,
        export_names,
        imports,
    })
}

/// Extract the string literal in `from "..."` or `import "..."`.
fn extract_specifier(line: &str) -> Option<std::string::String> {
    let bytes = line.as_bytes();
    // Find the last quoted string on the line.
    let mut quote = None;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'"' || b == b'\'' {
            quote = Some((i, b));
        }
    }
    let (start, q) = quote?;
    // Find matching opening quote before `start`.
    let mut open = None;
    for i in (0..start).rev() {
        if bytes[i] == q {
            open = Some(i);
            break;
        }
    }
    let open = open?;
    Some(line[open + 1..start].to_string())
}

/// Return the substring between the first `open` and the matching `close`.
fn between(s: &str, open: char, close: char) -> Option<std::string::String> {
    let a = s.find(open)?;
    let b = s[a + 1..].find(close)? + a + 1;
    Some(s[a + 1..b].to_string())
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
pub extern "C" fn v8__Module__GetStatus(this: *const Module) -> ModuleStatus {
    match module_state(this) {
        Some(m) => match m.status {
            ModuleStatus::Uninstantiated => ModuleStatus::Uninstantiated,
            ModuleStatus::Instantiating => ModuleStatus::Instantiating,
            ModuleStatus::Instantiated => ModuleStatus::Instantiated,
            ModuleStatus::Evaluating => ModuleStatus::Evaluating,
            ModuleStatus::Evaluated => ModuleStatus::Evaluated,
            ModuleStatus::Errored => ModuleStatus::Errored,
        },
        // Unknown / non-synthetic module objects: report Errored so callers
        // don't proceed into unsupported native-module paths.
        None => ModuleStatus::Errored,
    }
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
pub extern "C" fn v8__Module__GetModuleRequests(this: *const Module) -> *const FixedArray {
    // Synthetic modules have no imports: return an empty JS array.
    let ctx = module_state(this)
        .map(|m| m.ctx as JSContextRef)
        .unwrap_or_else(current_ctx);
    if ctx.is_null() {
        return ptr::null();
    }
    let mut exc: JSValueRef = ptr::null();
    let arr = unsafe { JSObjectMakeArray(ctx, 0, ptr::null(), &mut exc) };
    if arr.is_null() {
        return ptr::null();
    }
    intern_ctx::<FixedArray>(ctx, arr as JSValueRef)
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
pub extern "C" fn v8__Module__GetModuleNamespace(this: *const Module) -> *const Value {
    match module_state(this) {
        Some(m) if !m.namespace.is_null() => {
            intern_ctx::<Value>(m.ctx as JSContextRef, m.namespace as JSValueRef)
        }
        _ => ptr::null(),
    }
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
    this: *const Module,
    _context: *const Context,
    _cb: ResolveModuleCallback,
    _source_callback: Option<ResolveSourceCallback>,
) -> MaybeBool {
    // Synthetic modules have no imports, so "linking" only advances status.
    match module_state(this) {
        Some(m) => {
            if matches!(m.status, ModuleStatus::Uninstantiated) {
                m.status = ModuleStatus::Instantiated;
            }
            MaybeBool::JustTrue
        }
        None => MaybeBool::JustFalse,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__Evaluate(
    this: *const Module,
    context: *const Context,
) -> *const Value {
    let ctx = ctx_of(context) as JSContextRef;
    let Some(m) = module_state(this) else {
        return ptr::null();
    };
    m.status = ModuleStatus::Evaluating;

    if let Some(eval_steps) = m.eval_steps {
        // Synthetic module: run deno's evaluation steps, which call
        // SetSyntheticModuleExport per export and return a resolved promise.
        let ret = unsafe {
            let ctx_l = crate::Local::from_raw(context as *const Context).unwrap();
            let mod_l = crate::Local::from_raw(this).unwrap();
            eval_steps(ctx_l, mod_l)
        };
        if let Some(m) = module_state(this) {
            m.status = ModuleStatus::Evaluated;
        }
        let promise = unsafe {
            *(&ret as *const SyntheticModuleEvaluationStepsRet
                as *const *const Value)
        };
        if !promise.is_null() {
            return promise;
        }
        return make_resolved_promise(ctx);
    }

    // Source-text ES module: evaluate the rewritten body, which assigns its
    // exports onto the namespace object.
    if let Some(src) = m.source.clone() {
        let namespace = m.namespace;
        let ok = unsafe { eval_module_source(ctx, &src, namespace) };
        if let Some(m) = module_state(this) {
            m.status = if ok { ModuleStatus::Evaluated } else { ModuleStatus::Errored };
        }
        if !ok {
            return ptr::null();
        }
    } else if let Some(m) = module_state(this) {
        m.status = ModuleStatus::Evaluated;
    }
    make_resolved_promise(ctx)
}

/// Build a Promise resolved with undefined (module evaluation completion).
fn make_resolved_promise(ctx: JSContextRef) -> *const Value {
    if ctx.is_null() {
        return ptr::null();
    }
    let src =
        b"Promise.resolve(undefined)\0";
    let mut exc: JSValueRef = ptr::null();
    let js = unsafe {
        JSStringCreateWithUTF8CString(src.as_ptr() as *const std::os::raw::c_char)
    };
    let p = unsafe {
        JSEvaluateScript(ctx, js, ptr::null_mut(), ptr::null_mut(), 1, &mut exc)
    };
    unsafe { JSStringRelease(js) };
    if p.is_null() {
        let v = unsafe { JSValueMakeUndefined(ctx) };
        return intern_ctx::<Value>(ctx, v);
    }
    // Track so Promise::state()/result() observe it as fulfilled.
    crate::shim_exception::track_promise_pub(ctx, p as JSObjectRef);
    intern_ctx::<Value>(ctx, p)
}

/// Evaluate a rewritten ES-module body in `ctx`, exposing its exports on
/// `namespace`. Returns true on success.
unsafe fn eval_module_source(
    ctx: JSContextRef,
    rewritten: &str,
    namespace: JSObjectRef,
) -> bool {
    let wrapped = format!(
        "(function(__ns){{\n{}\n}})",
        rewritten
    );
    let cstr = match std::ffi::CString::new(wrapped) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let mut exc: JSValueRef = ptr::null();
    let js = unsafe { JSStringCreateWithUTF8CString(cstr.as_ptr()) };
    let f = unsafe {
        JSEvaluateScript(ctx, js, ptr::null_mut(), ptr::null_mut(), 1, &mut exc)
    };
    unsafe { JSStringRelease(js) };
    if f.is_null() {
        return false;
    }
    let fobj = unsafe { JSValueToObject(ctx, f, &mut exc) };
    if fobj.is_null() {
        return false;
    }
    let args = [namespace as JSValueRef];
    let r = unsafe {
        JSObjectCallAsFunction(ctx, fobj, ptr::null_mut(), 1, args.as_ptr(), &mut exc)
    };
    !r.is_null() && exc.is_null()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__IsGraphAsync(_this: *const Module) -> bool {
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__IsSyntheticModule(this: *const Module) -> bool {
    module_state(this).is_some()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__CreateSyntheticModule(
    isolate: *const RealIsolate,
    _module_name: *const V8String,
    export_names_len: usize,
    export_names_raw: *const *const V8String,
    evaluation_steps: SyntheticModuleEvaluationSteps,
) -> *const Module {
    let st = iso_state(isolate as *mut RealIsolate);
    let ctx = st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
    if ctx.is_null() {
        return ptr::null();
    }
    let gctx = unsafe { JSContextGetGlobalContext(ctx) };

    // Collect the export names as Rust strings.
    let mut names: Vec<std::string::String> = Vec::with_capacity(export_names_len);
    if !export_names_raw.is_null() {
        for i in 0..export_names_len {
            let nptr = unsafe { *export_names_raw.add(i) };
            let nv = jsval(nptr);
            if nv.is_null() {
                continue;
            }
            let mut exc: JSValueRef = ptr::null();
            let s = unsafe { JSValueToStringCopy(ctx, nv, &mut exc) };
            if s.is_null() {
                continue;
            }
            let max = unsafe { JSStringGetMaximumUTF8CStringSize(s) };
            let mut buf = vec![0u8; max];
            let n = unsafe {
                JSStringGetUTF8CString(s, buf.as_mut_ptr() as *mut _, max)
            };
            unsafe { JSStringRelease(s) };
            if n > 0 {
                buf.truncate(n - 1);
                if let Ok(name) = std::string::String::from_utf8(buf) {
                    names.push(name);
                }
            }
        }
    }

    // Namespace object that will hold the exports; protect it for the module's
    // lifetime (released in mod_finalize).
    let namespace = unsafe { JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut()) };
    unsafe { JSValueProtect(gctx, namespace as JSValueRef) };

    let steps: SyntheticModuleEvaluationSteps<'static> =
        unsafe { std::mem::transmute(evaluation_steps) };

    let state = Box::new(SyntheticModule {
        ctx: gctx,
        status: ModuleStatus::Uninstantiated,
        export_names: names,
        eval_steps: Some(steps),
        source: None,
        import_specifiers: Vec::new(),
        namespace,
    });
    let obj = unsafe {
        JSObjectMake(ctx, mod_class(), Box::into_raw(state) as *mut _)
    };
    intern_ctx::<Module>(ctx, obj as JSValueRef)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__SetSyntheticModuleExport(
    this: *const Module,
    _isolate: *const RealIsolate,
    export_name: *const V8String,
    export_value: *const Value,
) -> MaybeBool {
    let Some(m) = module_state(this) else {
        return MaybeBool::JustFalse;
    };
    let ctx = m.ctx as JSContextRef;
    let name_v = jsval(export_name);
    let val = jsval(export_value);
    if ctx.is_null() || name_v.is_null() || m.namespace.is_null() {
        return MaybeBool::JustFalse;
    }
    let mut exc: JSValueRef = ptr::null();
    let key = unsafe { JSValueToStringCopy(ctx, name_v, &mut exc) };
    if key.is_null() {
        return MaybeBool::JustFalse;
    }
    unsafe {
        JSObjectSetProperty(ctx, m.namespace, key, val, 0, &mut exc);
        JSStringRelease(key);
    }
    MaybeBool::JustTrue
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__GetUnboundModuleScript(
    this: *const Module,
) -> *const UnboundModuleScript {
    // Carry the module handle through; the UnboundModuleScript accessors we
    // support (source URL / mapping URL) return undefined regardless.
    if this.is_null() {
        return ptr::null();
    }
    intern::<UnboundModuleScript>(jsval(this))
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
