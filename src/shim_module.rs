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
    Location, ModuleStatus, ResolveModuleCallback, ResolveModuleCallbackRet,
    ResolveSourceCallback, StalledTopLevelAwaitMessage, SyntheticModuleEvaluationSteps,
    SyntheticModuleEvaluationStepsRet,
};
use crate::script::ScriptOrigin;
use crate::script_compiler::{CachedData, CompileOptions, NoCacheReason, Source};
use crate::support::{Maybe, MaybeBool, int};

// JSC C API functions come from `crate::jsc_sys` (bindgen) via the glob import.
//
// `ModJSClassDefinition` below is a layout-compatible mirror of bindgen's
// `JSClassDefinition` that type-erases the callback fields to `*const c_void`
// (more convenient than spelling out each `Option<extern fn>`). It is passed to
// `JSClassCreate` via a cast.

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
    /// This module's own specifier (resource name), used to register its
    /// namespace into `globalThis.__v8jsc_modules` so dependents resolve it.
    specifier: std::string::String,
    /// Resolved dependency module handles (one per import specifier), filled in
    /// during InstantiateModule via the resolve callback. Evaluated (post-order)
    /// before this module's own body runs, mirroring V8 module linking.
    dependencies: Vec<*const Module>,
    /// True if this source-text module uses top-level `await` (outside any
    /// function). Such modules are evaluated by wrapping the rewritten body in
    /// an `async` function whose returned promise drives module completion, and
    /// `v8__Module__IsGraphAsync` reports them async so deno awaits that promise.
    is_async: bool,
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
        let cls = unsafe { JSClassCreate(&def as *const _ as *const JSClassDefinition) };
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
    // ScriptOrigin is an opaque byte buffer to us; JSC doesn't consume origin
    // metadata through the C API. Zero-initialize, then stash the resource-name
    // handle in the first usize slot so Source::CONSTRUCT can carry it into the
    // Source (used to derive a module's specifier for our linker).
    if !buf.is_null() {
        unsafe {
            ptr::write_bytes(buf as *mut u8, 0u8, size_of::<ScriptOrigin>());
            *(buf as *mut usize) = resource_name as usize;
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
    origin: *const ScriptOrigin,
    _cached_data: *mut CachedData,
) {
    if buf.is_null() {
        return;
    }
    unsafe {
        // Zero the whole struct, then stash the source-string handle in the
        // first field (_source_string) so Compile* can recover it, and the
        // resource-name handle (carried in the ScriptOrigin's first slot) in
        // the second field (_resource_name) so we can derive the specifier.
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

/// Convert a JS string value to a Rust String. Empty on failure/null.
unsafe fn jsstring_to_rust(ctx: JSContextRef, v: JSValueRef) -> std::string::String {
    if ctx.is_null() || v.is_null() {
        return std::string::String::new();
    }
    unsafe {
        let mut exc: JSValueRef = ptr::null();
        let s = JSValueToStringCopy(ctx, v, &mut exc);
        if s.is_null() {
            return std::string::String::new();
        }
        let max = JSStringGetMaximumUTF8CStringSize(s);
        let mut buf = vec![0u8; max];
        let n = JSStringGetUTF8CString(s, buf.as_mut_ptr() as *mut _, max);
        JSStringRelease(s);
        if n == 0 {
            return std::string::String::new();
        }
        buf.truncate(n - 1);
        std::string::String::from_utf8(buf).unwrap_or_default()
    }
}

/// Recover the module specifier (resource name) from a `Source`. The
/// `_resource_name` field is at usize index 1 and holds a `*const Value` that
/// is a JS string (or null). Returns an empty string if unavailable.
unsafe fn resource_name_of(ctx: JSContextRef, source: *mut Source) -> std::string::String {
    if source.is_null() || ctx.is_null() {
        return std::string::String::new();
    }
    let name_val = unsafe { *((source as *const usize).add(1)) } as JSValueRef;
    if name_val.is_null() {
        return std::string::String::new();
    }
    unsafe {
        if JSValueIsUndefined(ctx, name_val) || JSValueIsNull(ctx, name_val) {
            return std::string::String::new();
        }
        let mut exc: JSValueRef = ptr::null();
        let s = JSValueToStringCopy(ctx, name_val, &mut exc);
        if s.is_null() {
            return std::string::String::new();
        }
        let max = JSStringGetMaximumUTF8CStringSize(s);
        let mut buf = vec![0u8; max];
        let n = JSStringGetUTF8CString(s, buf.as_mut_ptr() as *mut _, max);
        JSStringRelease(s);
        if n == 0 {
            return std::string::String::new();
        }
        buf.truncate(n - 1);
        std::string::String::from_utf8(buf).unwrap_or_default()
    }
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
        let raw = Box::from_raw(this as *mut RawCachedData);
        // Free the data buffer too when we own it (BufferOwned == 1).
        if raw.buffer_policy == 1 && !raw.data.is_null() && raw.length > 0 {
            let slice =
                std::slice::from_raw_parts_mut(raw.data as *mut u8, raw.length as usize);
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

    // Recover the resource name (module specifier) from the ScriptOrigin so the
    // module's namespace can be registered for dependents to import.
    let specifier = unsafe { resource_name_of(ctx, source) };

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
        specifier,
        dependencies: Vec::new(),
        is_async: rewrite.is_async,
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
    is_async: bool,
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
/// Collapse statements whose `{ ... }` binding list spans multiple physical
/// lines (`import {`, `export {`) into one logical line. Non-brace statements
/// and plain code pass through unchanged (one logical line each). Blank lines
/// that were consumed inside a join are replaced so total line accounting stays
/// roughly stable, but exact positions are not required for evaluation.
fn join_module_statements(src: &str) -> Vec<std::string::String> {
    let lines: Vec<&str> = src.lines().collect();
    let mut result: Vec<std::string::String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        let starts_brace_stmt = (trimmed.starts_with("import")
            || trimmed.starts_with("export"))
            && trimmed.contains('{')
            && !trimmed.contains('}');
        if starts_brace_stmt {
            // Accumulate until we see the closing `}` (and any following
            // `from "..."` / `;` on that or a later line).
            let mut buf = std::string::String::new();
            buf.push_str(lines[i].trim_end());
            buf.push(' ');
            let mut closed = false;
            i += 1;
            while i < lines.len() {
                let l = lines[i];
                buf.push_str(l.trim());
                buf.push(' ');
                if l.contains('}') {
                    closed = true;
                    // The `from "..."` clause may be on the same line as `}`.
                    // If the line ends the statement (contains `from` or ends
                    // with `;` or just `}`), stop here.
                    i += 1;
                    // Pull in a trailing `from "..."` line if `}` was alone.
                    if !buf.contains(" from ")
                        && !buf.trim_end().ends_with(';')
                        && i < lines.len()
                        && lines[i].trim_start().starts_with("from ")
                    {
                        buf.push_str(lines[i].trim());
                        buf.push(' ');
                        i += 1;
                    }
                    break;
                }
                i += 1;
            }
            let _ = closed;
            result.push(buf.split_whitespace().collect::<Vec<_>>().join(" "));
        } else {
            result.push(lines[i].to_string());
            i += 1;
        }
    }
    result
}

fn rewrite_es_module(src: &str) -> Option<RewrittenModule> {
    let mut out = std::string::String::new();
    // Import bindings are hoisted to the top of the module body to mirror ES
    // module semantics (imported names are live before any other statement
    // runs). Deno's bootstrap uses imported bindings earlier in source order
    // than the import statement itself, so we must emit them first.
    let mut imports_out = std::string::String::new();
    // Export assignments (`__ns[name] = local`) are deferred to the end of the
    // body so multi-line declarations (e.g. `export const X = { ... }` spanning
    // many lines) finish before the namespace is populated, and so forward
    // references between exports resolve.
    let mut exports_out = std::string::String::new();
    let mut export_names: Vec<std::string::String> = Vec::new();
    let mut imports: Vec<std::string::String> = Vec::new();

    // Pre-join multi-line `import { ... } from "..."` / `export { ... }` /
    // `export { ... } from "..."` statements into a single logical line so the
    // per-line rewriter below sees the whole statement at once. Deno's builtin
    // modules format these imports across many lines. Each produced logical line
    // carries an explicit trailing newline count so source positions and any
    // embedded plain code keep their line breaks.
    let logical = join_module_statements(src);

    for raw_line in logical.iter() {
        let raw_line: &str = raw_line.as_str();
        let line = raw_line.trim_start();
        let trimmed = line.trim();

        // `export * from` is not supported.
        if trimmed.starts_with("export *") {
            return None;
        }

        // import ... from "spec";  /  import "spec";
        if trimmed.starts_with("import ") || trimmed == "import" {
            let spec = extract_specifier(trimmed).unwrap_or_default();
            if !spec.is_empty() {
                imports.push(spec.clone());
            }
            let module_expr =
                format!("((globalThis.__v8jsc_modules||{{}})[{:?}]||{{}})", spec);

            // Clause between `import` and `from` (the binding list).
            let clause = if let Some((c, _)) = trimmed["import".len()..].split_once(" from ") {
                c.trim()
            } else {
                ""
            };

            if clause.contains('{') {
                // Possibly `def, { a, b as c }` — split off a leading default.
                let brace_start = clause.find('{').unwrap();
                let head = clause[..brace_start].trim().trim_end_matches(',').trim();
                if !head.is_empty() {
                    // default binding
                    imports_out.push_str(&format!(
                        "const {} = {}.default;\n",
                        head, module_expr
                    ));
                }
                // Named bindings: convert `a as b` -> `a: b` for destructuring.
                let names = between(trimmed, '{', '}').unwrap_or_default();
                let mut destructure: Vec<std::string::String> = Vec::new();
                for part in names.split(',') {
                    let part = part.trim();
                    if part.is_empty() {
                        continue;
                    }
                    if let Some((l, r)) = part.split_once(" as ") {
                        destructure.push(format!("{}: {}", l.trim(), r.trim()));
                    } else {
                        destructure.push(part.to_string());
                    }
                }
                imports_out.push_str(&format!(
                    "const {{ {} }} = {};\n",
                    destructure.join(", "),
                    module_expr
                ));
            } else if clause.starts_with("* as ") {
                // import * as ns from "spec"
                let name = clause["* as ".len()..].trim();
                if !name.is_empty() {
                    imports_out.push_str(&format!("const {} = {};\n", name, module_expr));
                }
            } else if !clause.is_empty() && trimmed.contains(" from ") {
                // import def from "spec"  -> default import
                imports_out.push_str(&format!(
                    "const {} = {}.default;\n",
                    clause, module_expr
                ));
            }
            // bare `import "spec";` -> side effect only, nothing to bind.
            continue;
        }

        // export { a, b as c };   or   export { a, b } from "spec";  (re-export)
        if trimmed.starts_with("export {") || trimmed.starts_with("export{") {
            let inner = between(trimmed, '{', '}').unwrap_or_default();
            // Re-export: bindings come from the named module, not local scope.
            let reexport_spec = if trimmed.contains(" from ") {
                let spec = extract_specifier(trimmed).unwrap_or_default();
                if !spec.is_empty() {
                    imports.push(spec.clone());
                }
                Some(spec)
            } else {
                None
            };
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
                if let Some(spec) = &reexport_spec {
                    exports_out.push_str(&format!(
                        "__ns[{:?}] = ((globalThis.__v8jsc_modules||{{}})[{:?}]||{{}})[{:?}];\n",
                        exported, spec, local
                    ));
                } else {
                    exports_out.push_str(&format!("__ns[{:?}] = {};\n", exported, local));
                }
            }
            continue;
        }

        // export default EXPR;
        if trimmed.starts_with("export default ") {
            let expr = &trimmed["export default ".len()..];
            export_names.push("default".to_string());
            exports_out.push_str(&format!(
                "__ns[\"default\"] = ({});\n",
                expr.trim_end_matches(';')
            ));
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
            // `export const { a, b: c } = expr` — destructuring declaration.
            if after_kw.trim_start().starts_with('{') {
                out.push_str(rest);
                out.push('\n');
                let inner = between(after_kw, '{', '}').unwrap_or_default();
                for part in inner.split(',') {
                    let part = part.trim();
                    if part.is_empty() {
                        continue;
                    }
                    // `a`, `a: b`, `a = default`, `a: b = default` — the bound
                    // local name is what's after `:` (or the bare name), before
                    // any `=`.
                    let local = part
                        .split(':')
                        .next_back()
                        .unwrap_or(part)
                        .split('=')
                        .next()
                        .unwrap_or(part)
                        .trim();
                    let name: std::string::String = local
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
                        .collect();
                    if !name.is_empty() {
                        export_names.push(name.clone());
                        exports_out.push_str(&format!("__ns[{:?}] = {};\n", name, name));
                    }
                }
                continue;
            }
            let name: std::string::String = after_kw
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
                .collect();
            out.push_str(rest);
            out.push('\n');
            if !name.is_empty() {
                export_names.push(name.clone());
                exports_out.push_str(&format!("__ns[{:?}] = {};\n", name, name));
            }
            continue;
        }

        // Plain line: keep as-is.
        out.push_str(raw_line);
        out.push('\n');
    }

    // Prepend hoisted import bindings (ES module semantics) and append deferred
    // export assignments so all declarations are complete first.
    let combined = format!("{}{}{}", imports_out, out, exports_out);

    // `import.meta` is only valid in real modules; our body runs as a plain
    // function. Replace it with a parameter the wrapper provides. This is a
    // textual substitution (acceptable for the trusted builtin sources we run).
    let body = combined.replace("import.meta", "__v8jsc_meta");

    Some(RewrittenModule {
        body,
        export_names,
        imports,
        is_async: has_top_level_await(src),
    })
}

/// Best-effort detection of top-level `await` (or `for await`) in module source
/// — i.e. an `await` keyword that is NOT nested inside any function/arrow body.
/// We scan character-by-character, skipping string/template/comment content and
/// tracking brace depth, and we mark the brace depth at which each `function` /
/// `=>` body opens. An `await` token found while not inside any such body is a
/// top-level await. This is a heuristic (the JSC C API exposes no parser hook),
/// but it is sound for the module shapes deno actually runs: false negatives are
/// the only failure mode and would simply fall back to the old sync behavior.
fn has_top_level_await(src: &str) -> bool {
    let b = src.as_bytes();
    let n = b.len();
    let mut i = 0usize;
    // Stack of brace depths at which a function/arrow body was opened. While the
    // current depth is greater than the top of this stack, we are inside a
    // function body and `await` there is not top-level.
    let mut fn_body_depths: Vec<i32> = Vec::new();
    let mut depth: i32 = 0;
    // Tracks whether a `function`/arrow header was seen and we're waiting for its
    // opening `{` to record the body depth.
    let mut pending_fn_body = false;

    let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';

    while i < n {
        let c = b[i];
        match c {
            // Line comment.
            b'/' if i + 1 < n && b[i + 1] == b'/' => {
                i += 2;
                while i < n && b[i] != b'\n' {
                    i += 1;
                }
            }
            // Block comment.
            b'/' if i + 1 < n && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < n && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            // String literals.
            b'"' | b'\'' => {
                let q = c;
                i += 1;
                while i < n {
                    if b[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if b[i] == q {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            // Template literals — skip content, but recurse into `${ ... }` is
            // overkill; awaits inside template substitutions at top level are
            // exceedingly rare in practice. We conservatively skip the whole
            // template, accepting a possible false negative.
            b'`' => {
                i += 1;
                while i < n {
                    if b[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if b[i] == b'`' {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b'{' => {
                depth += 1;
                if pending_fn_body {
                    fn_body_depths.push(depth);
                    pending_fn_body = false;
                }
                i += 1;
            }
            b'}' => {
                if let Some(&d) = fn_body_depths.last() {
                    if d == depth {
                        fn_body_depths.pop();
                    }
                }
                depth -= 1;
                i += 1;
            }
            // Arrow function: `=>` opens a body. If followed by `{` we'll catch
            // it via pending_fn_body; if it's an expression-bodied arrow
            // (`x => await y` is itself a syntax error unless arrow is async,
            // and an async arrow's await is not top-level), mark pending so the
            // next `{` (if any) is treated as a function body. Expression-bodied
            // async arrows can't contain a *bare* top-level await anyway.
            b'=' if i + 1 < n && b[i + 1] == b'>' => {
                pending_fn_body = true;
                i += 2;
            }
            _ if is_ident(c) => {
                let start = i;
                while i < n && is_ident(b[i]) {
                    i += 1;
                }
                let word = &src[start..i];
                let prev_is_dot = start > 0 && {
                    // Skip back over whitespace to see if preceded by `.`
                    let mut j = start;
                    while j > 0 && (b[j - 1] as char).is_whitespace() {
                        j -= 1;
                    }
                    j > 0 && b[j - 1] == b'.'
                };
                match word {
                    "function" => {
                        pending_fn_body = true;
                    }
                    "await" if !prev_is_dot => {
                        // Top-level only when not inside any function body.
                        if fn_body_depths.is_empty() {
                            return true;
                        }
                    }
                    _ => {}
                }
            }
            _ => {
                i += 1;
            }
        }
    }
    false
}

/// Extract the module specifier: the string literal following `from` (or the
/// first string for a bare `import "x"`). NOT the last string on the line —
/// import attributes (`import x from "./d.json" with { type: "json" }`) add a
/// trailing string (`"json"`) that must not be taken as the specifier.
fn extract_specifier(line: &str) -> Option<std::string::String> {
    let scan = match line.rfind(" from ") {
        Some(p) => &line[p + 6..],
        None => line,
    };
    let bytes = scan.as_bytes();
    let mut open = None;
    let mut q = b'"';
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'"' || b == b'\'' {
            open = Some(i);
            q = b;
            break;
        }
    }
    let open = open?;
    for i in (open + 1)..bytes.len() {
        if bytes[i] == q {
            return Some(scan[open + 1..i].to_string());
        }
    }
    None
}

/// Extract the `type` import attribute from `with { type: "json" }` / legacy
/// `assert { type: "json" }`. Returns None when absent.
fn extract_attr_type(stmt: &str) -> Option<std::string::String> {
    let kw = stmt
        .find(" with ")
        .or_else(|| stmt.find(" with{"))
        .or_else(|| stmt.find(" assert "))
        .or_else(|| stmt.find(" assert{"))?;
    let clause = &stmt[kw..];
    let tpos = clause.find("type")?;
    let after = &clause[tpos + 4..];
    let bytes = after.as_bytes();
    let mut open = None;
    let mut q = b'"';
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'"' || b == b'\'' {
            open = Some(i);
            q = b;
            break;
        }
    }
    let open = open?;
    for i in (open + 1)..bytes.len() {
        if bytes[i] == q {
            return Some(after[open + 1..i].to_string());
        }
    }
    None
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
    // JSC exposes no serializable bytecode cache. deno (`deno run`) requires
    // `create_code_cache()` to return Some, so we hand back a tiny placeholder
    // cache. It is never usable: on consume, our compile path ignores cached
    // data and reports `rejected() == true`, so V8/deno recompiles from source.
    make_placeholder_code_cache()
}

/// Build a minimal owned `CachedData` (1 byte) matching the layout used by
/// `v8__ScriptCompiler__CachedData__NEW`, marked as owned so deno frees it.
fn make_placeholder_code_cache() -> *mut CachedData<'static> {
    #[repr(C)]
    struct RawCachedData {
        data: *const u8,
        length: i32,
        rejected: bool,
        buffer_policy: i32,
    }
    // deno asserts the returned cache is BufferOwned. Allocate a 1-byte owned
    // buffer; our CachedData__DELETE frees owned buffers.
    let v = vec![0u8; 1].into_boxed_slice();
    let len = v.len() as i32;
    let data = Box::into_raw(v) as *const u8;
    let boxed = Box::new(RawCachedData {
        data,
        length: len,
        rejected: false,
        buffer_policy: 1, // BufferOwned
    });
    Box::into_raw(boxed) as *mut CachedData<'static>
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
    // Build a FixedArray of ModuleRequest objects (one per static import) so
    // deno can construct its import graph and pre-resolve specifiers. Each
    // request is a plain JS object `{ specifier, phase }` that the
    // ModuleRequest accessors below read.
    let ctx = module_state(this)
        .map(|m| m.ctx as JSContextRef)
        .unwrap_or_else(current_ctx);
    if ctx.is_null() {
        return ptr::null();
    }
    let specs: Vec<std::string::String> = module_state(this)
        .map(|m| m.import_specifiers.clone())
        .unwrap_or_default();

    let mut elems: Vec<JSValueRef> = Vec::with_capacity(specs.len());
    unsafe {
        for spec in &specs {
            let req = JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut());
            if let Ok(cspec) = std::ffi::CString::new(spec.as_str()) {
                let sval_str = JSStringCreateWithUTF8CString(cspec.as_ptr());
                let sval = JSValueMakeString(ctx, sval_str);
                JSStringRelease(sval_str);
                let key = JSStringCreateWithUTF8CString(c"specifier".as_ptr());
                JSObjectSetProperty(ctx, req, key, sval, 0, ptr::null_mut());
                JSStringRelease(key);
            }
            // Tag so v8__Data__IsModuleRequest can recognize it.
            let mark = JSStringCreateWithUTF8CString(c"__v8jsc_module_request".as_ptr());
            JSObjectSetProperty(
                ctx,
                req,
                mark,
                JSValueMakeBoolean(ctx, true),
                1 << 1, /* DontEnum */
                ptr::null_mut(),
            );
            JSStringRelease(mark);
            elems.push(req as JSValueRef);
        }
        let mut exc: JSValueRef = ptr::null();
        let arr = JSObjectMakeArray(ctx, elems.len(), elems.as_ptr(), &mut exc);
        if arr.is_null() {
            return ptr::null();
        }
        intern_ctx::<FixedArray>(ctx, arr as JSValueRef)
    }
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
    context: *const Context,
    cb: ResolveModuleCallback,
    _source_callback: Option<ResolveSourceCallback>,
) -> MaybeBool {
    // JSC has no module linker, so we walk the import graph ourselves: for each
    // static import specifier, call the resolve callback to obtain the dependency
    // module, recursively instantiate it, and remember it so Evaluate can run
    // dependencies first (post-order), mirroring V8's linking + evaluation.
    let ctx = ctx_of(context) as JSContextRef;
    // Capture the current isolate: re-entrant deno resolve callbacks construct
    // and drop scopes that can clear the current-isolate thread-local, which we
    // must restore before each subsequent callback.
    let iso = current_iso();
    let Some(m) = module_state(this) else {
        return MaybeBool::JustFalse;
    };
    if !matches!(m.status, ModuleStatus::Uninstantiated) {
        return MaybeBool::JustTrue;
    }
    m.status = ModuleStatus::Instantiating;
    let specs = m.import_specifiers.clone();

    let mut deps: Vec<*const Module> = Vec::new();
    for spec in &specs {
        crate::shim_core::restore_current(iso);
        let dep = unsafe { resolve_dependency(context, cb, this, spec) };
        if dep.is_null() {
            // Unresolved import; leave inert (binding will be undefined).
            continue;
        }
        // Recursively instantiate the dependency.
        if let Some(dm) = module_state(dep) {
            if matches!(dm.status, ModuleStatus::Uninstantiated) {
                let _ = v8__Module__InstantiateModule(dep, context, cb, _source_callback);
            }
        }
        deps.push(dep);
    }
    crate::shim_core::restore_current(iso);
    if let Some(m) = module_state(this) {
        m.dependencies = deps;
        m.status = ModuleStatus::Instantiated;
    }
    MaybeBool::JustTrue
}

/// Resolve one import specifier to a dependency module via the host callback.
unsafe fn resolve_dependency(
    context: *const Context,
    cb: ResolveModuleCallback,
    referrer: *const Module,
    spec: &str,
) -> *const Module {
    let ctx = ctx_of(context) as JSContextRef;
    if ctx.is_null() {
        return ptr::null();
    }
    let cspec = match std::ffi::CString::new(spec) {
        Ok(c) => c,
        Err(_) => return ptr::null(),
    };
    let spec_str = unsafe { JSStringCreateWithUTF8CString(cspec.as_ptr()) };
    let spec_val = unsafe { JSValueMakeString(ctx, spec_str) };
    unsafe { JSStringRelease(spec_str) };
    let spec_handle = intern_ctx::<V8String>(ctx, spec_val);
    // Empty import-attributes FixedArray.
    let mut exc: JSValueRef = ptr::null();
    let attrs_arr = unsafe { JSObjectMakeArray(ctx, 0, ptr::null(), &mut exc) };
    let attrs_handle = intern_ctx::<FixedArray>(ctx, attrs_arr as JSValueRef);

    let ret = unsafe {
        let ctx_l = crate::Local::from_raw(context).unwrap();
        let spec_l = crate::Local::from_raw(spec_handle).unwrap();
        let attrs_l = crate::Local::from_raw(attrs_handle).unwrap();
        let ref_l = crate::Local::from_raw(referrer).unwrap();
        cb(ctx_l, spec_l, attrs_l, ref_l)
    };
    // ResolveModuleCallbackRet is a transparent *const Module.
    unsafe { *(&ret as *const ResolveModuleCallbackRet as *const *const Module) }
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
    // Already evaluated? (e.g. shared dependency reached via two paths.)
    if matches!(m.status, ModuleStatus::Evaluated) {
        return make_resolved_promise(ctx);
    }
    m.status = ModuleStatus::Evaluating;

    // Evaluate dependencies first (post-order) so their namespaces are populated
    // and registered before this module's body references them. Collect the
    // evaluation promises of any *async* (top-level-await) dependency so this
    // module's body can await them before reading their exports — mirroring V8's
    // async module graph, where a parent's evaluation waits on async children.
    let iso = current_iso();
    let deps = m.dependencies.clone();
    let mut dep_promises: Vec<JSValueRef> = Vec::new();
    for dep in &deps {
        if dep.is_null() {
            continue;
        }
        let dep_async = v8__Module__IsGraphAsync(*dep);
        if let Some(dm) = module_state(*dep) {
            if matches!(dm.status, ModuleStatus::Evaluated) && !dep_async {
                continue;
            }
        }
        let p = v8__Module__Evaluate(*dep, context);
        if dep_async && !p.is_null() {
            dep_promises.push(jsval(p));
        }
    }
    crate::shim_core::restore_current(iso);

    let Some(m) = module_state(this) else {
        return ptr::null();
    };

    // Register this module's namespace object into `globalThis.__v8jsc_modules`
    // keyed by specifier, so dependents (and self-references) resolve imports.
    // We register the live namespace object up front; the body / eval steps then
    // populate its properties in place.
    if !m.specifier.is_empty() {
        unsafe { register_module_namespace(ctx, &m.specifier, m.namespace) };
    }

    // The rewritten body looks up imports by their *raw* source specifier (e.g.
    // `"./dep.js"`), but each dependency registered itself under its *resolved*
    // specifier (e.g. `file:///abs/dep.js`). Bridge the two by aliasing each raw
    // import specifier to the corresponding dependency's namespace right before
    // running this module's body, so relative imports between user modules
    // resolve. Specifiers and dependencies are kept in the same source order.
    {
        let raw_specs = m.import_specifiers.clone();
        let deps2 = m.dependencies.clone();
        for (raw, dep) in raw_specs.iter().zip(deps2.iter()) {
            if dep.is_null() {
                continue;
            }
            if let Some(dm) = module_state(*dep) {
                if !raw.is_empty() && !dm.namespace.is_null() {
                    unsafe { register_module_namespace(ctx, raw, dm.namespace) };
                }
            }
        }
    }

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
        // The body must run as async if it uses top-level await OR if it depends
        // on an async module whose evaluation promise we must await first.
        let is_async = m.is_async || !dep_promises.is_empty();
        let namespace = m.namespace;
        let meta = unsafe { build_import_meta(context, this) };
        match unsafe {
            eval_module_source(ctx, &src, namespace, meta, is_async, &dep_promises)
        } {
            Ok(promise) => {
                // V8 marks a TLA module Evaluated once its synchronous prologue
                // has run, even though the returned promise is still pending;
                // deno asserts the post-Evaluate status is Evaluated/Errored.
                if let Some(m) = module_state(this) {
                    m.status = ModuleStatus::Evaluated;
                }
                // Async module: hand back the async function's own promise so
                // deno's `mod_evaluate` awaits it (exports are populated when it
                // settles). Sync module: fall through to the resolved promise.
                if !promise.is_null() {
                    crate::shim_exception::track_promise_pub(
                        ctx,
                        promise as JSObjectRef,
                    );
                    return intern_ctx::<Value>(ctx, promise);
                }
            }
            Err(exc) => {
                if let Some(m) = module_state(this) {
                    m.status = ModuleStatus::Errored;
                }
                // V8 semantics: a module that throws during evaluation still
                // returns a Promise — a *rejected* one — rather than null. deno's
                // `mod_evaluate` then drives `on_rejected`, surfacing the real
                // error. Returning null instead makes deno treat the failure as
                // "execution terminated" (the oneshot sender is dropped unsent).
                return make_rejected_promise(ctx, exc);
            }
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
/// `namespace`.
///
/// For a synchronous module (`is_async == false`) the body is wrapped in a plain
/// function and called; `Ok(null)` signals success. For a top-level-await module
/// (`is_async == true`) the body is wrapped in an `async` function: calling it
/// runs the synchronous prologue and returns a promise that settles once the
/// awaited body completes (and exports are assigned). That promise is returned as
/// `Ok(promise)` so the caller can hand it to deno's evaluation driver. A body
/// that throws *synchronously* (before the first await) makes an async function
/// return a rejected promise rather than throw, so the async path never takes the
/// `Err` branch for body errors — deno surfaces them via the rejected promise.
///
/// `Err(exception)` (never null) is only returned when wrapping/compiling or the
/// call itself fails, so callers can build a rejected promise.
unsafe fn eval_module_source(
    ctx: JSContextRef,
    rewritten: &str,
    namespace: JSObjectRef,
    meta: JSValueRef,
    is_async: bool,
    dep_promises: &[JSValueRef],
) -> Result<JSValueRef, JSValueRef> {
    let fail = |ctx: JSContextRef, exc: JSValueRef| -> Result<JSValueRef, JSValueRef> {
        unsafe { report_module_exception(ctx, exc) };
        // Guarantee a non-null exception value for the rejected promise.
        let e = if exc.is_null() {
            make_generic_error(ctx, "module evaluation failed")
        } else {
            exc
        };
        Err(e)
    };
    let kw = if is_async { "async function" } else { "function" };
    // When this module depends on async (TLA) modules, await their evaluation
    // promises (passed as `__deps`) before running the body so their exports are
    // populated first. `__deps` is always a parameter for layout stability but is
    // only awaited on the async path.
    let prologue = if is_async && !dep_promises.is_empty() {
        "await Promise.all(__deps);\n"
    } else {
        ""
    };
    let wrapped = format!(
        "({kw}(__ns, __v8jsc_meta, __deps){{\n{prologue}{}\n}})",
        rewritten
    );
    let cstr = match std::ffi::CString::new(wrapped) {
        Ok(c) => c,
        Err(_) => return fail(ctx, ptr::null()),
    };
    let mut exc: JSValueRef = ptr::null();
    let js = unsafe { JSStringCreateWithUTF8CString(cstr.as_ptr()) };
    let f = unsafe {
        JSEvaluateScript(ctx, js, ptr::null_mut(), ptr::null_mut(), 1, &mut exc)
    };
    unsafe { JSStringRelease(js) };
    if f.is_null() {
        return fail(ctx, exc);
    }
    let fobj = unsafe { JSValueToObject(ctx, f, &mut exc) };
    if fobj.is_null() {
        return fail(ctx, exc);
    }
    let meta = if meta.is_null() {
        unsafe { JSValueMakeUndefined(ctx) }
    } else {
        meta
    };
    let deps_arr = unsafe {
        JSObjectMakeArray(ctx, dep_promises.len(), dep_promises.as_ptr(), &mut exc)
    };
    let deps_val = if deps_arr.is_null() {
        unsafe { JSValueMakeUndefined(ctx) }
    } else {
        deps_arr as JSValueRef
    };
    let args = [namespace as JSValueRef, meta, deps_val];
    let r = unsafe {
        JSObjectCallAsFunction(ctx, fobj, ptr::null_mut(), 3, args.as_ptr(), &mut exc)
    };
    if is_async {
        // The call itself only fails for pathological reasons (the async body's
        // own throws become a rejected promise). `r` is the module-evaluation
        // promise; return it for the caller to drive.
        if r.is_null() || !exc.is_null() {
            return fail(ctx, exc);
        }
        return Ok(r);
    }
    if r.is_null() || !exc.is_null() {
        return fail(ctx, exc);
    }
    Ok(ptr::null())
}

/// Make a generic `Error(message)` JS value in `ctx`.
unsafe fn make_generic_error(ctx: JSContextRef, message: &str) -> JSValueRef {
    let mut exc: JSValueRef = ptr::null();
    let msg = std::ffi::CString::new(message).unwrap_or_default();
    let s = unsafe { JSStringCreateWithUTF8CString(msg.as_ptr()) };
    let arg = unsafe { JSValueMakeString(ctx, s) };
    unsafe { JSStringRelease(s) };
    let args = [arg];
    let e = unsafe { JSObjectMakeError(ctx, 1, args.as_ptr(), &mut exc) };
    if e.is_null() {
        unsafe { JSValueMakeUndefined(ctx) }
    } else {
        e as JSValueRef
    }
}

/// Build a Promise pre-rejected with `exc`, mirroring V8's behavior of returning
/// a rejected promise from `Module::Evaluate` when the body throws.
fn make_rejected_promise(ctx: JSContextRef, exc: JSValueRef) -> *const Value {
    if ctx.is_null() {
        return ptr::null();
    }
    let exc = if exc.is_null() {
        unsafe { JSValueMakeUndefined(ctx) }
    } else {
        exc
    };
    // Use Promise.reject via the global so the promise is a genuine JSC promise
    // that our promise tracking and deno's `.then`/`.catch` handlers observe.
    let mut e: JSValueRef = ptr::null();
    let p = unsafe {
        let global = JSContextGetGlobalObject(ctx);
        let pkey = JSStringCreateWithUTF8CString(c"Promise".as_ptr());
        let promise_ctor = JSObjectGetProperty(ctx, global, pkey, &mut e);
        JSStringRelease(pkey);
        if promise_ctor.is_null() || !JSValueIsObject(ctx, promise_ctor) {
            return intern_ctx::<Value>(ctx, JSValueMakeUndefined(ctx));
        }
        let rkey = JSStringCreateWithUTF8CString(c"reject".as_ptr());
        let reject = JSObjectGetProperty(ctx, promise_ctor as JSObjectRef, rkey, &mut e);
        JSStringRelease(rkey);
        if reject.is_null() || !JSValueIsObject(ctx, reject) {
            return intern_ctx::<Value>(ctx, JSValueMakeUndefined(ctx));
        }
        let args = [exc];
        JSObjectCallAsFunction(
            ctx,
            reject as JSObjectRef,
            promise_ctor as JSObjectRef,
            1,
            args.as_ptr(),
            &mut e,
        )
    };
    if p.is_null() {
        return intern_ctx::<Value>(ctx, unsafe { JSValueMakeUndefined(ctx) });
    }
    crate::shim_exception::track_promise_pub(ctx, p as JSObjectRef);
    intern_ctx::<Value>(ctx, p)
}

/// Register `namespace` into `globalThis.__v8jsc_modules[specifier]` so other
/// modules' rewritten import statements can resolve their bindings.
unsafe fn register_module_namespace(
    ctx: JSContextRef,
    specifier: &str,
    namespace: JSObjectRef,
) {
    if ctx.is_null() || namespace.is_null() || specifier.is_empty() {
        return;
    }
    unsafe {
        let global = JSContextGetGlobalObject(ctx);
        let mut exc: JSValueRef = ptr::null();
        // globalThis.__v8jsc_modules ||= {}
        let reg_key =
            JSStringCreateWithUTF8CString(c"__v8jsc_modules".as_ptr());
        let mut reg = JSObjectGetProperty(ctx, global, reg_key, &mut exc);
        let reg_obj = if reg.is_null() || JSValueIsUndefined(ctx, reg) {
            let o = JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut());
            JSObjectSetProperty(ctx, global, reg_key, o as JSValueRef, 1 << 1, &mut exc);
            o
        } else {
            JSValueToObject(ctx, reg, &mut exc)
        };
        JSStringRelease(reg_key);
        if reg_obj.is_null() {
            return;
        }
        if let Ok(cspec) = std::ffi::CString::new(specifier) {
            let spec_key = JSStringCreateWithUTF8CString(cspec.as_ptr());
            JSObjectSetProperty(
                ctx,
                reg_obj,
                spec_key,
                namespace as JSValueRef,
                0,
                &mut exc,
            );
            JSStringRelease(spec_key);
        }
    }
}

/// Build the `import.meta` object for a source-text module by invoking the
/// host callback registered via SetHostInitializeImportMetaObjectCallback.
unsafe fn build_import_meta(
    context: *const Context,
    module: *const Module,
) -> JSValueRef {
    let ctx = ctx_of(context) as JSContextRef;
    if ctx.is_null() {
        return ptr::null();
    }
    let meta = unsafe { JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut()) };
    let iso = current_iso();
    if iso.is_null() {
        return meta as JSValueRef;
    }
    let cb = iso_state(iso).import_meta_cb;
    if let Some(cb) = cb {
        unsafe {
            let ctx_l = crate::Local::from_raw(context).unwrap();
            let mod_l = crate::Local::from_raw(module).unwrap();
            let meta_l =
                crate::Local::<Object>::from_raw(meta as *const Object).unwrap();
            cb(ctx_l, mod_l, meta_l);
        }
    }
    meta as JSValueRef
}

/// Record a JSC exception from module evaluation as the isolate's pending
/// exception so deno's TryCatch-based error reporting can surface it.
unsafe fn report_module_exception(ctx: JSContextRef, exc: JSValueRef) {
    if exc.is_null() {
        return;
    }
    crate::shim_core::record_pending_exception(ctx, exc);
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__IsGraphAsync(this: *const Module) -> bool {
    // A module's graph is async if the module itself uses top-level await, or
    // any of its (transitive) dependencies do. deno awaits the evaluation
    // promise of an async-graph module rather than requiring synchronous
    // completion (see `mod_evaluate_sync`'s TLA guard).
    fn graph_async(this: *const Module, seen: &mut Vec<*const Module>) -> bool {
        if this.is_null() || seen.contains(&this) {
            return false;
        }
        seen.push(this);
        match module_state(this) {
            Some(m) => {
                if m.is_async {
                    return true;
                }
                let deps = m.dependencies.clone();
                deps.iter().any(|d| graph_async(*d, seen))
            }
            None => false,
        }
    }
    let mut seen = Vec::new();
    graph_async(this, &mut seen)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__IsSyntheticModule(this: *const Module) -> bool {
    module_state(this).is_some()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__Module__CreateSyntheticModule(
    isolate: *const RealIsolate,
    module_name: *const V8String,
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
    let specifier = unsafe { jsstring_to_rust(ctx, jsval(module_name)) };

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
        specifier,
        dependencies: Vec::new(),
        is_async: false,
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
pub extern "C" fn v8__ModuleRequest__GetSpecifier(this: *const ModuleRequest) -> *const V8String {
    // A ModuleRequest is a `{ specifier, ... }` object (see GetModuleRequests).
    let ctx = current_ctx();
    if ctx.is_null() || this.is_null() {
        return ptr::null();
    }
    unsafe {
        let obj = jsval(this) as JSObjectRef;
        let key = JSStringCreateWithUTF8CString(c"specifier".as_ptr());
        let mut exc: JSValueRef = ptr::null();
        let v = JSObjectGetProperty(ctx, obj, key, &mut exc);
        JSStringRelease(key);
        if v.is_null() || JSValueIsUndefined(ctx, v) {
            return ptr::null();
        }
        intern_ctx::<V8String>(ctx, v)
    }
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
    // No import attributes: return an empty JS array (deno iterates it).
    let ctx = current_ctx();
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

// ===================================================================
// ScriptCompiler::CompileUnboundScript / CachedDataVersionTag and
// UnboundScript::BindToCurrentContext / GetSourceMappingURL.
//
// In this backend a Script/UnboundScript handle simply carries the
// source-text JSValueRef (see v8__Script__Compile). So CompileUnboundScript
// syntax-checks and returns the source value as an UnboundScript, and
// BindToCurrentContext hands the same value back as a Script.
// ===================================================================

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CompileUnboundScript(
    isolate: *mut RealIsolate,
    source: *mut Source,
    _options: CompileOptions,
    _no_cache_reason: NoCacheReason,
) -> *const UnboundScript {
    let st = iso_state(isolate);
    let ctx = st.contexts.last().copied().unwrap_or(ptr::null_mut()) as JSContextRef;
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
    intern_ctx::<UnboundScript>(ctx, src_val)
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CachedDataVersionTag() -> u32 {
    // Any stable nonzero tag; our placeholder caches are always rejected, so the
    // exact value only needs to be consistent within a run.
    0x5643_4a53 // "JSCV"
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundScript__BindToCurrentContext(
    script: *const UnboundScript,
) -> *const Script {
    if script.is_null() {
        return ptr::null();
    }
    intern::<Script>(jsval(script))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundScript__GetSourceMappingURL(
    _script: *const UnboundScript,
) -> *const Value {
    let ctx = current_ctx();
    if ctx.is_null() {
        return ptr::null();
    }
    let v = unsafe { JSValueMakeUndefined(ctx) };
    intern_ctx::<Value>(ctx, v)
}
