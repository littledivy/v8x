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
    ctx_of, current_ctx, current_iso, intern, intern_ctx, intern_dup, iso_state, jsval_of,
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
// Bytecode cache (startup acceleration).
//
// QuickJS has no V8-style heap snapshot, so every boot otherwise re-parses all
// ~100 extension JS modules in `module_loader_callback`. We instead persist each
// module's compiled QuickJS bytecode (`JS_WriteObject`) keyed by a content hash,
// and on subsequent boots load it back with `JS_ReadObject` — skipping the parse
// entirely. This is the QuickJS analogue of V8's startup snapshot.
//
// Safety: bytecode is only valid for the exact QuickJS build that wrote it.
// QuickJS embeds its own BC_VERSION byte (JS_ReadObject rejects a mismatch), and
// we additionally fold `BC_MAGIC` into the key so a source/format change misses
// rather than mis-loads. Disable with `V82JSC_NO_BC_CACHE=1`.
// ===================================================================

const JS_WRITE_OBJ_BYTECODE: int = 1 << 0;
const JS_READ_OBJ_BYTECODE: int = 1 << 0;
/// Bump on any change to the vendored QuickJS or the module-eval format.
const BC_MAGIC: u32 = 0x5142_4302; // 'QBC2'

fn bc_cache_dir() -> Option<std::path::PathBuf> {
    use std::sync::OnceLock;
    static DIR: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        if std::env::var_os("V82JSC_NO_BC_CACHE").is_some() {
            return None;
        }
        let base = std::env::var_os("DENO_DIR")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(|h| std::path::PathBuf::from(h).join("Library/Caches"))
            })
            .unwrap_or_else(std::env::temp_dir);
        let dir = base.join("v82jsc_bc");
        std::fs::create_dir_all(&dir).ok()?;
        Some(dir)
    })
    .clone()
}

fn bc_key(source: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    BC_MAGIC.hash(&mut h);
    source.len().hash(&mut h);
    source.hash(&mut h);
    h.finish()
}

fn bc_path(key: u64) -> Option<std::path::PathBuf> {
    Some(bc_cache_dir()?.join(format!("{key:016x}.qbc")))
}

fn bc_load(key: u64) -> Option<Vec<u8>> {
    let p = bc_path(key)?;
    std::fs::read(p).ok().filter(|b| !b.is_empty())
}

fn bc_store(key: u64, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    if let Some(p) = bc_path(key) {
        let tmp = p.with_extension("tmp");
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

/// Serialize a compiled module/script JSValue to bytecode and persist under `key`.
/// Does not consume `obj`.
unsafe fn bc_write(ctx: *mut JSContext, key: u64, obj: JSValue) {
    if bc_cache_dir().is_none() {
        return;
    }
    let mut size: usize = 0;
    let buf = unsafe { JS_WriteObject(ctx, &mut size, obj, JS_WRITE_OBJ_BYTECODE) };
    if !buf.is_null() && size > 0 {
        let slice = unsafe { std::slice::from_raw_parts(buf, size) };
        bc_store(key, slice);
    }
    if !buf.is_null() {
        unsafe { js_free(ctx, buf as *mut std::os::raw::c_void) };
    }
}

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
    /// Static import specifiers parsed from source (for GetModuleRequests),
    /// each paired with its import-attribute `type` (`with { type: "json" }`)
    /// when present.
    import_specifiers: Vec<(std::string::String, Option<std::string::String>)>,
    /// WASM source-phase imports rewritten out of the source: `(map id, raw
    /// specifier)`. InstantiateModule resolves each via the ResolveSourceCallback
    /// and stores the WasmModuleObject in `globalThis.__v82jsc_wasm_src`.
    source_imports: Vec<(u64, std::string::String)>,
    /// True for synthetic modules.
    synthetic: bool,
    /// True if source uses top-level await.
    is_async: bool,
    /// The module's source text and name (URL), kept so `Evaluate` can re-eval
    /// from source when no pre-compiled bytecode is available (the COMPILE_ONLY
    /// step failed because a static import couldn't be resolved yet — its source
    /// gets registered later, before evaluation).
    source_text: std::string::String,
    source_name: std::string::String,
}

thread_local! {
    static MODULE_STATE: RefCell<HashMap<usize, ModuleState>> =
        RefCell::new(HashMap::new());
    /// Module source text keyed by module name (URL). Populated by
    /// `CompileModule` so the QuickJS module loader can resolve static imports
    /// (`import x from "ext:deno_features/flags.js"`) by re-compiling the named
    /// source on demand. V8 never eagerly loads imports during compile; QuickJS
    /// does, via this loader.
    static MODULE_SOURCES_BY_NAME: RefCell<HashMap<std::string::String, std::string::String>> =
        RefCell::new(HashMap::new());
    /// JSModuleDef cache keyed by module name, so repeated imports of the same
    /// name reuse the same def (QuickJS dedupes by name internally too, but this
    /// avoids re-parsing).
    static MODULE_DEF_CACHE: RefCell<HashMap<std::string::String, usize>> =
        RefCell::new(HashMap::new());
    /// Pending synthetic-module exports keyed by JSModuleDef pointer; consumed
    /// by `synthetic_module_init_callback` when QuickJS first imports it.
    static SYNTHETIC_EXPORTS: RefCell<HashMap<usize, Vec<(std::string::String, JSValue)>>> =
        RefCell::new(HashMap::new());
    /// JSModuleDef pointer keyed by Module handle pointer, for synthetic modules
    /// (so SetSyntheticModuleExport can recover the def from the handle).
    static SYNTHETIC_DEFS: RefCell<HashMap<usize, usize>> = RefCell::new(HashMap::new());
    /// For each synthetic-module JSModuleDef: deno's evaluation-steps callback
    /// plus the v8 `Module` handle to pass it. V8 runs these steps at *evaluate*
    /// time (they call `SetSyntheticModuleExport`), but QuickJS can only populate
    /// a CModule's exports inside its init (link-time) callback — which fires
    /// before deno evaluates the module. So we run the steps from inside the init
    /// callback to materialize exports just in time. Keyed by def pointer.
    /// Value: (eval-steps, the module handle's JSValue duped +1). We re-intern a
    /// fresh handle from this JSValue when invoking the steps, because the
    /// original arena slot may have been reclaimed by the time the init callback
    /// fires (it runs much later, during the importing module's evaluation).
    static SYNTHETIC_EVAL_STEPS: RefCell<HashMap<usize, (SyntheticModuleEvaluationSteps<'static>, JSValue)>> =
        RefCell::new(HashMap::new());
    /// Set once any module has been evaluated. QuickJS evaluates statically
    /// imported modules transitively during `JS_EvalFunction`/`JS_Eval(MODULE)`,
    /// so once one module is evaluated, every module reachable from it has also
    /// run. deno_core asserts every registered module reports `Evaluated` after
    /// bootstrap; we honor that by reporting Evaluated for all modules after the
    /// first evaluation (mirrors the reference PR's `AFTER_FIRST_EVAL`).
    static AFTER_FIRST_EVAL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };

    /// Maps a `(referrer_module_name, raw_import_specifier)` pair to deno's
    /// resolved module name. deno resolves every import specifier (e.g.
    /// `npm:express@4` or `./lib/foo`) to a canonical name (the `resource_name`
    /// it later compiles that module under, e.g.
    /// `file:///…/express/4.22.2/index.js`). QuickJS's own module loader, by
    /// contrast, is handed the *raw* specifier from the source text — which won't
    /// match any registered source. We bridge the two by walking deno's
    /// `ResolveModuleCallback` during `InstantiateModule` to learn every
    /// raw→resolved edge, then consult this map in `module_normalize_callback`.
    static RESOLVED_SPECIFIERS: RefCell<HashMap<(std::string::String, std::string::String), std::string::String>> =
        RefCell::new(HashMap::new());
}

thread_local! {
    /// Whether deno registered a `HostInitializeImportMetaObjectCallback`. deno's
    /// callback needs a `Local<Module>` we can't supply for loader-compiled
    /// dependencies, so we don't invoke it directly; we just use its presence as
    /// a signal that `import.meta` should be populated, and fill `url`/`main`
    /// ourselves from the module's resolved name. npm packages run
    /// `createRequire(import.meta.url)` at the top of their entry, so an
    /// unpopulated `import.meta.url` breaks them.
    static IMPORT_META_ENABLED: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// Record that deno wants `import.meta` populated (called from the isolate family).
pub(crate) fn set_import_meta_callback(
    _cb: crate::isolate::HostInitializeImportMetaObjectCallback,
) {
    IMPORT_META_ENABLED.with(|c| c.set(true));
}

thread_local! {
    /// deno's async `HostImportModuleDynamicallyCallback`, stored by
    /// `v8__Isolate__SetHostImportModuleDynamicallyCallback`. Our QuickJS
    /// dynamic-import hook calls it to load new specifiers, then chains the
    /// promise it returns to QuickJS's `import()` promise.
    static DYN_IMPORT_CB: std::cell::Cell<
        Option<crate::isolate::RawHostImportModuleDynamicallyCallback>,
    > = const { std::cell::Cell::new(None) };

    /// Cached `(d,res,rej) => { Promise.resolve(d).then(res,rej); }` helper that
    /// chains deno's import promise onto QuickJS's resolving funcs.
    static DYN_IMPORT_CHAIN: std::cell::Cell<Option<JSValue>> =
        const { std::cell::Cell::new(None) };

    /// deno's ResolveSourceCallback (WASM source-phase). Stashed from the most
    /// recent InstantiateModule so `Module::Evaluate` can resolve the source-phase
    /// imports of an on-demand wrapper module that `build_resolution_map` never
    /// walked.
    static SOURCE_CB: std::cell::Cell<Option<ResolveSourceCallback<'static>>> =
        const { std::cell::Cell::new(None) };

    /// Modules carrying WASM source-phase imports, recorded at CompileModule.
    /// `InstantiateModule` resolves every one through deno's source callback
    /// (regardless of static-graph reachability — many are reached only via
    /// on-demand dynamic-import chains that build_resolution_map never walks).
    static PENDING_SOURCE_IMPORTS: RefCell<
        Vec<(*const Module, Vec<(u64, std::string::String)>)>,
    > = const { RefCell::new(Vec::new()) };
}

/// Store deno's dynamic-import callback + install the QuickJS hook (idempotent).
pub(crate) fn set_dynamic_import_callback(
    cb: crate::isolate::RawHostImportModuleDynamicallyCallback,
) {
    DYN_IMPORT_CB.with(|c| c.set(Some(cb)));
    unsafe { JS_SetDynamicImportHook(dynamic_import_hook) };
}

/// QuickJS dynamic-import hook: bridge `import(specifier)` to deno's async loader.
/// Calls deno's `HostImportModuleDynamicallyCallback` to obtain a promise that
/// resolves to the module namespace, then chains it onto `resolving_funcs`
/// (`[0]`=resolve, `[1]`=reject) so the `import()` promise settles with it.
unsafe extern "C" fn dynamic_import_hook(
    ctx: *mut JSContext,
    basename: JSValue,
    specifier: JSValue,
    _attributes: JSValue,
    resolving_funcs: *const JSValue,
) {
    let resolve = unsafe { *resolving_funcs };
    let reject = unsafe { *resolving_funcs.add(1) };

    if std::env::var_os("QJS_DBG_DYNIMPORT").is_some() {
        let s = unsafe { jsval_to_rust(ctx, specifier) };
        eprintln!("[dynimport] specifier={s:?}");
    }

    let reject_with = |msg: &str| unsafe {
        if let Ok(cm) = CString::new(msg) {
            let mut a = [JS_NewString(ctx, cm.as_ptr())];
            let r = JS_Call(ctx, reject, jsv_undefined(), 1, a.as_mut_ptr());
            JS_FreeValue(ctx, r);
            JS_FreeValue(ctx, a[0]);
        }
    };

    let Some(cb) = DYN_IMPORT_CB.with(|c| c.get()) else {
        reject_with("dynamic import: no host callback");
        return;
    };

    // Mint Local args for deno's callback. host_defined_options = undefined
    // (normal modules; deno reads its "kind" and treats undefined as default).
    let context = intern_ctx(ctx);
    let host_opts = intern::<Data>(jsv_undefined());
    let referrer = intern_dup::<Value>(ctx, basename);
    let spec_handle = intern_dup::<V8String>(ctx, specifier);
    let attrs_handle = intern::<FixedArray>(unsafe { JS_NewArray(ctx) });
    let (Some(ctx_l), Some(ho_l), Some(ref_l), Some(spec_l), Some(attr_l)) = (
        unsafe { crate::Local::from_raw(context) },
        unsafe { crate::Local::from_raw(host_opts) },
        unsafe { crate::Local::from_raw(referrer) },
        unsafe { crate::Local::from_raw(spec_handle) },
        unsafe { crate::Local::from_raw(attrs_handle) },
    ) else {
        reject_with("dynamic import: handle alloc failed");
        return;
    };

    let promise_ptr = unsafe { cb(ctx_l, ho_l, ref_l, spec_l, attr_l) };
    if promise_ptr.is_null() {
        // deno may have left an exception pending; surface it, else generic.
        if unsafe { JS_HasException(ctx) } != 0 {
            let mut a = [unsafe { JS_GetException(ctx) }];
            let r = unsafe { JS_Call(ctx, reject, jsv_undefined(), 1, a.as_mut_ptr()) };
            unsafe { JS_FreeValue(ctx, r) };
            unsafe { JS_FreeValue(ctx, a[0]) };
        } else {
            reject_with("dynamic import: host callback returned null");
        }
        return;
    }
    let d = jsval_of(promise_ptr as *const Value);

    // Chain deno's promise onto our resolving funcs via a cached JS helper.
    let chain = DYN_IMPORT_CHAIN.with(|c| {
        if let Some(cur) = c.get() {
            return cur;
        }
        let src = c"(d,res,rej)=>{Promise.resolve(d).then(res,rej);}";
        let f = unsafe {
            JS_Eval(
                ctx,
                src.as_ptr(),
                src.to_bytes().len(),
                c"<dynimport-chain>".as_ptr(),
                JS_EVAL_TYPE_GLOBAL,
            )
        };
        if f.tag != JS_TAG_EXCEPTION {
            c.set(Some(f));
        }
        f
    });
    if chain.tag == JS_TAG_EXCEPTION {
        reject_with("dynamic import: chain helper failed");
        return;
    }
    let mut args = [d, resolve, reject];
    let r = unsafe { JS_Call(ctx, chain, jsv_undefined(), 3, args.as_mut_ptr()) };
    unsafe { JS_FreeValue(ctx, r) };
}

/// Populate a freshly-compiled module's `import.meta.url`/`main` from its name,
/// before the body runs. No-op if import.meta isn't enabled, the def is null, or
/// the module has no URL-shaped name (e.g. synthetic `ext:`/`node:` builtins,
/// which never read `import.meta`).
unsafe fn populate_import_meta(ctx: *mut JSContext, def: *mut JSModuleDef, name: &str) {
    if def.is_null() || !IMPORT_META_ENABLED.with(|c| c.get()) {
        return;
    }
    // Only real module URLs are useful here; `createRequire` rejects non-URLs.
    if !(name.starts_with("file://")
        || name.starts_with("http://")
        || name.starts_with("https://"))
    {
        return;
    }
    let meta = unsafe { JS_GetImportMeta(ctx, def) };
    if meta.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return;
    }
    if let Ok(curl) = CString::new(name) {
        let url_val = unsafe { JS_NewString(ctx, curl.as_ptr()) };
        unsafe { JS_SetPropertyStr(ctx, meta, c"url".as_ptr(), url_val) };
    }
    unsafe { JS_SetPropertyStr(ctx, meta, c"main".as_ptr(), JS_NewBool(ctx, 0)) };
    // The WASM-ESM wrapper module (named `*.wasm`) instantiates via
    // `new import.meta.WasmInstance(wasmMod, imports)` and keeps a
    // `import.meta.wasmInstances` Map for global imports. V8/deno set these in the
    // import-meta callback; set them directly to `WebAssembly.Instance` + a Map.
    if name.ends_with(".wasm") {
        let global = unsafe { JS_GetGlobalObject(ctx) };
        let wasm = unsafe { JS_GetPropertyStr(ctx, global, c"WebAssembly".as_ptr()) };
        if jsv_is_object(&wasm) {
            let inst = unsafe { JS_GetPropertyStr(ctx, wasm, c"Instance".as_ptr()) };
            unsafe { JS_SetPropertyStr(ctx, meta, c"WasmInstance".as_ptr(), inst) };
        }
        unsafe { JS_FreeValue(ctx, wasm) };
        let src = c"new Map()";
        let map = unsafe {
            JS_Eval(
                ctx,
                src.as_ptr(),
                src.to_bytes().len(),
                c"<wasmmap>".as_ptr(),
                JS_EVAL_TYPE_GLOBAL,
            )
        };
        if map.tag == JS_TAG_EXCEPTION {
            unsafe {
                let e = JS_GetException(ctx);
                JS_FreeValue(ctx, e);
            }
        } else {
            unsafe { JS_SetPropertyStr(ctx, meta, c"wasmInstances".as_ptr(), map) };
        }
        unsafe { JS_FreeValue(ctx, global) };
    }
    unsafe { JS_FreeValue(ctx, meta) };
}

/// Look up deno's resolved name for an import of `spec` made from module `base`.
fn lookup_resolved_specifier(base: &str, spec: &str) -> Option<std::string::String> {
    RESOLVED_SPECIFIERS.with(|t| {
        t.borrow()
            .get(&(base.to_string(), spec.to_string()))
            .cloned()
    })
}

fn record_resolved_specifier(base: &str, spec: &str, resolved: &str) {
    RESOLVED_SPECIFIERS.with(|t| {
        t.borrow_mut()
            .insert((base.to_string(), spec.to_string()), resolved.to_string());
    });
}

/// Referrer-INDEPENDENT lookup: the first recorded resolution of `spec` under any
/// base. Bare import-map / npm specifiers (`preact`, `@foo/bar`) resolve to the
/// same URL regardless of the importing module (the deno.json import map is
/// global), but a module pulled in on-demand never has its own (base, spec) edge
/// recorded by InstantiateModule's static walk. Reusing any recorded edge for the
/// same bare specifier lets such imports resolve. NOT used for relative
/// specifiers (those are genuinely referrer-dependent).
fn lookup_resolved_specifier_any(spec: &str) -> Option<std::string::String> {
    RESOLVED_SPECIFIERS.with(|t| {
        t.borrow()
            .iter()
            .find(|((_, s), _)| s == spec)
            .map(|(_, resolved)| resolved.clone())
    })
}

/// Mark every known module Evaluated and flip the global after-first-eval flag.
/// Called from `Module::Evaluate` because QuickJS evaluates imported modules
/// transitively, so deno's post-evaluate `check_all_modules_evaluated` must see
/// them all as Evaluated.
fn mark_all_modules_evaluated() {
    MODULE_STATE.with(|t| {
        for m in t.borrow_mut().values_mut() {
            m.status = ModuleStatus::Evaluated;
        }
    });
    AFTER_FIRST_EVAL.with(|f| f.set(true));
}

#[allow(dead_code)]
fn after_first_eval() -> bool {
    AFTER_FIRST_EVAL.with(|f| f.get())
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

/// Parse static import/re-export specifiers from module source text.
///
/// deno_core builds its module graph from `GetModuleRequests`, so this must find
/// every `import ... from "spec"`, bare `import "spec"`, and `export ... from
/// "spec"` — including multi-line forms like:
///
/// ```text
/// import {
///   a, b, c,
/// } from "ext:core/ops";
/// ```
///
/// We tokenize at statement boundaries rather than per line. Best-effort but
/// handles the multi-line named-import blocks deno's bootstrap modules use.
fn parse_import_specifiers(
    src: &str,
) -> Vec<(std::string::String, Option<std::string::String>)> {
    let mut out = Vec::new();
    let bytes = src.as_bytes();
    let mut i = 0usize;
    let n = bytes.len();
    while i < n {
        // Skip whitespace.
        while i < n && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }
        // Skip line comments.
        if i + 1 < n && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < n && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Skip block comments.
        if i + 1 < n && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < n && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        // Skip string / template literals WHEREVER they occur. Tools like
        // framework scaffolders embed entire source files (with their own
        // `import ... from "..."` lines) inside backtick template strings; without
        // skipping them the scanner mis-extracts those as real dependencies
        // ("Import \"fresh/runtime\" not a dependency").
        if bytes[i] == b'"' || bytes[i] == b'\'' {
            i = skip_string(bytes, i, bytes[i]);
            continue;
        }
        if bytes[i] == b'`' {
            i = skip_template(bytes, i);
            continue;
        }
        // The `import`/`export` keyword must start a statement: the preceding
        // char must be a boundary (start-of-input, whitespace, `;`, `{`, or `}`),
        // not part of an identifier (`reimport`, `foo.export`). Without this the
        // char-by-char scan would mid-identifier match.
        let at_boundary = i == 0
            || matches!(bytes[i - 1], b' ' | b'\t' | b'\r' | b'\n' | b';' | b'{' | b'}');
        if !at_boundary {
            i += 1;
            continue;
        }
        // Only consider statements that begin a line (column-0-ish): a static
        // import/export is always at the top level. Match `import`/`export`
        // keyword at the current position.
        let rest = &src[i..];
        let is_import = rest.starts_with("import")
            && rest[6..].chars().next().map(|c| c == ' ' || c == '{' || c == '*' || c == '"' || c == '\'' || c == '(').unwrap_or(false);
        // Only `export * ...` and `export { ... }` can be re-exports carrying a
        // module specifier (`export * from "x"`, `export {a} from "x"`). A plain
        // `export function`/`const`/`class`/`default ...` is a declaration with
        // NO specifier — treating it as one makes the statement scanner run across
        // the whole function body and mis-extract a string literal inside it
        // (e.g. `_extensions[".node"]`) as a bogus import.
        let after_export = rest.get(6..).map(|s| s.trim_start()).unwrap_or("");
        let is_export = rest.starts_with("export")
            && (after_export.starts_with('*') || after_export.starts_with('{'));
        if is_import || is_export {
            // Dynamic import `import(` is not a static dependency — skip.
            let dynamic = rest.starts_with("import(")
                || rest.starts_with("import (");
            // Find the end of this statement: the next semicolon or newline that
            // is not inside the import's named block. We scan up to a `;` or, if
            // none on the logical statement, the matching close of a `{...}` plus
            // its `from "..."`. Simplest robust approach: scan until `;` or two
            // consecutive newlines, capturing the last string literal which is
            // the specifier (for `from "spec"` / bare `import "spec"`).
            let mut j = i;
            let mut depth = 0i32;
            let mut end = n;
            while j < n {
                match bytes[j] {
                    b'{' => depth += 1,
                    b'}' => depth -= 1,
                    b';' if depth <= 0 => {
                        end = j;
                        break;
                    }
                    b'\n' if depth <= 0 && j > i => {
                        // Statement may end at newline if it already has a `from`
                        // or is a bare import string on this line.
                        let seg = &src[i..j];
                        if seg.contains(" from ") || seg.contains("\"") || seg.contains("'") {
                            // Heuristic: end at newline only if a specifier is
                            // already present (closed string).
                            if has_balanced_quotes(seg) {
                                end = j;
                                break;
                            }
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            let stmt = &src[i..end.min(n)];
            if !dynamic {
                let has_from = find_from_specifier_start(stmt).is_some();
                // A bare `import "spec"` has the string IMMEDIATELY after the
                // keyword. Requiring that rejects an `import` keyword matched
                // inside a regex literal (e.g. `/Relative import path ".*?"/`),
                // which the scanner can't otherwise distinguish from code.
                let bare_immediate = stmt
                    .strip_prefix("import")
                    .map(|r| {
                        let t = r.trim_start();
                        t.starts_with('"') || t.starts_with('\'')
                    })
                    .unwrap_or(false);
                let bare = is_import && !has_from && bare_immediate;
                // A real bare `import "spec"` is terminated by `;`, end-of-line,
                // EOF, or an import-attributes clause (`with`/`assert`). If the
                // closing quote is instead followed by `.`/`(`/`,`/an identifier
                // char, the `import '...'` text is INSIDE a string or template
                // literal that the scanner desync'd into — e.g. claude-code's
                // `throw Error(\`Failed to import '@aws-sdk/credential-providers'.\
                // You can provide ...\`)`. Reporting that as a real dependency makes
                // deno hard-fail "Could not find package" on a never-imported
                // optional dep. Reject those.
                let bare_ok = bare && bare_import_well_formed(stmt);
                if has_from || bare_ok {
                    if let Some(spec) = extract_specifier(stmt) {
                        if !spec.is_empty() {
                            let ty = extract_attr_type(stmt);
                            if std::env::var_os("QJS_DEBUG_PARSE").is_some() {
                                let snip: std::string::String =
                                    stmt.chars().take(80).collect();
                                eprintln!("[QJS parse] spec={spec:?} type={ty:?} stmt={snip:?}");
                            }
                            out.push((spec, ty));
                        }
                    }
                }
            }
            i = end.min(n) + 1;
            continue;
        }
        // Not an import/export and not a literal — advance one char. (The loop
        // top re-checks for comments/strings/templates at each position, so we
        // never scan into a literal's interior.)
        i += 1;
    }
    out
}

thread_local! {
    static SRC_PHASE_COUNTER: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
}
fn next_src_phase_id() -> u64 {
    SRC_PHASE_COUNTER.with(|c| {
        let v = c.get();
        c.set(v + 1);
        v
    })
}

/// Rewrite WASM source-phase imports (`import source X from "Y"`, the WASM-ESM
/// integration deno's generated wrapper uses) into a plain lookup
/// `const X = globalThis.__v82jsc_wasm_src.get(ID);`, since QuickJS-ng has no
/// source-phase parser. Returns the rewritten source plus the `(ID, specifier)`
/// records; `InstantiateModule` resolves each specifier through deno's
/// ResolveSourceCallback (the WasmModuleObject) and populates the map.
fn rewrite_source_phase(src: &str) -> (std::string::String, Vec<(u64, std::string::String)>) {
    if !src.contains("import source") {
        return (src.to_string(), Vec::new());
    }
    let bytes = src.as_bytes();
    let n = bytes.len();
    let mut out = std::string::String::with_capacity(src.len());
    let mut records = Vec::new();
    let mut i = 0usize;
    let mut last = 0usize;
    while i < n {
        let boundary = i == 0
            || matches!(bytes[i - 1], b'\n' | b'\r' | b';' | b'{' | b'}' | b' ' | b'\t');
        if boundary && src[i..].starts_with("import") {
            if let Some((consumed, binding, spec)) = parse_source_phase_at(&src[i..]) {
                out.push_str(&src[last..i]);
                let id = next_src_phase_id();
                out.push_str(&format!(
                    "const {binding}=globalThis.__v82jsc_wasm_src.get({id});"
                ));
                records.push((id, spec));
                i += consumed;
                last = i;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&src[last..]);
    (out, records)
}

/// Parse `import source <binding> from "<spec>"` at the start of `s`. Returns
/// `(bytes_consumed, binding, specifier)` or None if `s` isn't a source-phase
/// import.
fn parse_source_phase_at(s: &str) -> Option<(usize, std::string::String, std::string::String)> {
    let b = s.as_bytes();
    let n = b.len();
    let skip_ws = |p: usize| {
        let mut p = p;
        while p < n && b[p].is_ascii_whitespace() {
            p += 1;
        }
        p
    };
    if !s.starts_with("import") {
        return None;
    }
    let mut p = 6;
    let p1 = skip_ws(p);
    if p1 == p {
        return None; // need whitespace after `import`
    }
    p = p1;
    if !s[p..].starts_with("source") {
        return None;
    }
    p += 6;
    let p2 = skip_ws(p);
    if p2 == p {
        return None; // `source` must be a standalone keyword
    }
    p = p2;
    let bstart = p;
    while p < n && (b[p].is_ascii_alphanumeric() || b[p] == b'_' || b[p] == b'$') {
        p += 1;
    }
    if p == bstart {
        return None;
    }
    let binding = s[bstart..p].to_string();
    p = skip_ws(p);
    if !s[p..].starts_with("from") {
        return None;
    }
    p += 4;
    p = skip_ws(p);
    if p >= n {
        return None;
    }
    let q = b[p];
    if q != b'"' && q != b'\'' {
        return None;
    }
    p += 1;
    let sstart = p;
    while p < n && b[p] != q {
        p += 1;
    }
    if p >= n {
        return None;
    }
    let spec = s[sstart..p].to_string();
    p += 1;
    if p < n && b[p] == b';' {
        p += 1;
    }
    Some((p, binding, spec))
}

/// Skip a `"`/`'` string literal. `i` points at the opening quote; returns the
/// index just past the closing quote (or end of input).
fn skip_string(bytes: &[u8], i: usize, quote: u8) -> usize {
    let n = bytes.len();
    let mut j = i + 1;
    while j < n {
        match bytes[j] {
            b'\\' => j += 2,
            c if c == quote => return j + 1,
            b'\n' => return j, // unterminated; bail at line end
            _ => j += 1,
        }
    }
    n
}

/// Skip a backtick template literal, including `${ ... }` interpolations (which
/// may themselves contain strings/templates/braces). `i` points at the opening
/// backtick; returns the index just past the closing backtick.
fn skip_template(bytes: &[u8], i: usize) -> usize {
    let n = bytes.len();
    let mut j = i + 1;
    while j < n {
        match bytes[j] {
            b'\\' => j += 2,
            b'`' => return j + 1,
            b'$' if j + 1 < n && bytes[j + 1] == b'{' => {
                // Interpolation: skip to the matching `}`, honoring nested
                // strings/templates/braces.
                let mut depth = 1i32;
                j += 2;
                while j < n && depth > 0 {
                    match bytes[j] {
                        b'{' => {
                            depth += 1;
                            j += 1;
                        }
                        b'}' => {
                            depth -= 1;
                            j += 1;
                        }
                        b'"' | b'\'' => j = skip_string(bytes, j, bytes[j]),
                        b'`' => j = skip_template(bytes, j),
                        b'\\' => j += 2,
                        _ => j += 1,
                    }
                }
            }
            _ => j += 1,
        }
    }
    n
}

/// Validate a candidate bare `import "spec"` statement: the closing quote of the
/// specifier must be followed (after optional whitespace) by a real terminator —
/// `;`, end-of-string, `\n`/`\r`, or an import-attributes keyword (`with`/
/// `assert`). Anything else (`.`, `(`, `,`, `+`, an identifier char, ...) means
/// the `import '...'` text lives inside a string/template literal the scanner
/// landed in by mistake, so it's NOT a real dependency.
fn bare_import_well_formed(stmt: &str) -> bool {
    let b = stmt.as_bytes();
    // Find the opening quote of the bare specifier (right after `import`).
    let after_kw = match stmt.strip_prefix("import") {
        Some(r) => r,
        None => return false,
    };
    let lead_ws = r#"import"#.len() + (after_kw.len() - after_kw.trim_start().len());
    let q = match b.get(lead_ws) {
        Some(&c) if c == b'"' || c == b'\'' => c,
        _ => return false,
    };
    // Find the matching (unescaped) close quote.
    let mut j = lead_ws + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            c if c == q => break,
            b'\n' => return false,
            _ => j += 1,
        }
    }
    if j >= b.len() {
        return false; // unterminated
    }
    // Char after the close quote (skipping whitespace).
    let mut k = j + 1;
    while k < b.len() && (b[k] == b' ' || b[k] == b'\t' || b[k] == b'\r') {
        k += 1;
    }
    match b.get(k) {
        None => true,                                  // EOF
        Some(&c) if c == b';' || c == b'\n' => true,   // terminator
        _ => {
            // Only legal continuation is an import-attributes clause.
            stmt[k..].starts_with("with") || stmt[k..].starts_with("assert")
        }
    }
}

/// Whether `s` has an even number of (unescaped) double or single quotes — used
/// to decide if an import statement's specifier string is closed on a line.
fn has_balanced_quotes(s: &str) -> bool {
    let dq = s.matches('"').count();
    let sq = s.matches('\'').count();
    dq % 2 == 0 && sq % 2 == 0 && (dq + sq) >= 2
}

/// Extract the module specifier from an import/export statement. The specifier
/// is the string literal that follows the `from` keyword (or the first string
/// literal for a bare `import "x"`). Crucially it is NOT the *last* string on the
/// line: import attributes (`import x from "./d.json" with { type: "json" }`)
/// append a trailing string literal (`"json"`) that must not be mistaken for the
/// specifier — doing so makes deno resolve "json" and fail "not a dependency".
fn extract_specifier(line: &str) -> Option<std::string::String> {
    // Scan from just after the `from` keyword's clause (handles minified
    // `import{a}from"x"` with no spaces), or the whole statement for a bare
    // `import"x"`. The first string literal there is the specifier; anything
    // after it (a `with`/`assert` attributes clause) is ignored.
    let scan = match find_from_specifier_start(line) {
        Some(p) => &line[p..],
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

/// Find the byte offset just past a `from` KEYWORD that introduces the module
/// specifier — i.e. a standalone `from` token whose next non-whitespace char is a
/// quote. Handles `} from "x"`, `}from"x"` (minified), `* as ns from "x"`. The
/// `from` inside a named-imports list (`import { from } from "x"`) is rejected
/// because it isn't followed by a quote, so the real clause wins.
fn find_from_specifier_start(stmt: &str) -> Option<usize> {
    let b = stmt.as_bytes();
    let mut result = None;
    let mut idx = 0;
    while let Some(p) = stmt[idx..].find("from") {
        let at = idx + p;
        idx = at + 4;
        let prev_ok = at == 0 || {
            let c = b[at - 1];
            !(c.is_ascii_alphanumeric() || c == b'_' || c == b'$')
        };
        let post_ident = b
            .get(at + 4)
            .map(|&c| c.is_ascii_alphanumeric() || c == b'_' || c == b'$')
            .unwrap_or(false);
        let after = stmt[at + 4..].trim_start();
        let next_quote = after.starts_with('"') || after.starts_with('\'');
        if prev_ok && !post_ident && next_quote {
            result = Some(at + 4);
        }
    }
    result
}

/// Extract the `type` import attribute from a statement's `with { type: "json" }`
/// (or legacy `assert { type: "json" }`) clause. Returns None when absent.
fn extract_attr_type(stmt: &str) -> Option<std::string::String> {
    // Find the attributes clause introduced by a `with`/`assert` keyword that
    // follows the (closed) specifier string.
    let kw = stmt
        .find(" with ")
        .or_else(|| stmt.find(" with{"))
        .or_else(|| stmt.find(" assert "))
        .or_else(|| stmt.find(" assert{"))?;
    let clause = &stmt[kw..];
    // Within the clause, find `type` then the first string literal after it.
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
thread_local! {
    /// Guards against RE-ENTRANT job draining. QuickJS jobs (Promise reactions /
    /// microtasks) are only ever run via this function, so a single outermost
    /// drain loop runs them all. Without the guard, a job that synchronously
    /// triggers another drain (a wasm host-import op → `PerformMicrotaskCheckpoint`
    /// or `Module::Evaluate`) would run the NEXT job nested on the stack — fatal
    /// for the wasm-bindgen-futures single-thread executor, whose `run_all` borrows
    /// a `RefCell` across the poll: re-entering it panics "RefCell already
    /// borrowed" (→ wasm trap), which is what broke @deno/loader's async graph
    /// walk (addEntrypoints) and thus Fresh SSR.
    static JOBS_DRAINING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

unsafe fn drain_jobs(rt: *mut JSRuntime) {
    if rt.is_null() {
        return;
    }
    // Already draining higher on the stack — let that outer loop run the jobs.
    if JOBS_DRAINING.with(|d| d.replace(true)) {
        if std::env::var("V82_WASM_TRACE").is_ok() {
            eprintln!("[jobs] re-entrant drain_jobs BLOCKED");
        }
        return;
    }
    loop {
        let mut pctx: *mut JSContext = ptr::null_mut();
        let r = unsafe { JS_ExecutePendingJob(rt, &mut pctx) };
        if r <= 0 {
            break;
        }
    }
    JOBS_DRAINING.with(|d| d.set(false));
}

/// Register a module's source text under its name (URL) so the module loader
/// can resolve static imports of it.
pub(crate) fn register_module_source(name: &str, source: &str) {
    if name.is_empty() {
        return;
    }
    MODULE_SOURCES_BY_NAME.with(|t| {
        t.borrow_mut()
            .insert(name.to_string(), source.to_string());
    });
}

fn lookup_module_source_by_name(name: &str) -> Option<std::string::String> {
    MODULE_SOURCES_BY_NAME.with(|t| t.borrow().get(name).cloned())
}

/// Module *normalize* function registered via `JS_SetModuleLoaderFunc`. QuickJS
/// calls this before the loader to canonicalize an import specifier: given the
/// importing module's name (`base`) and the raw specifier (`name`) from source,
/// return the canonical module name (js_malloc'd; QuickJS frees it via js_free).
///
/// We consult `RESOLVED_SPECIFIERS` — populated from deno's `ResolveModuleCallback`
/// during `InstantiateModule` — so a `import "npm:express@4"` in `simple.js`
/// normalizes to the `file:///…/express/4.22.2/index.js` name deno registered the
/// source under. On a miss we return the specifier unchanged (matches QuickJS's
/// old default-normalize behavior for the absolute `ext:`/`node:` names, whose
/// specifier already equals the registered name).
pub(crate) unsafe extern "C" fn module_normalize_callback(
    ctx: *mut JSContext,
    module_base_name: *const std::os::raw::c_char,
    module_name: *const std::os::raw::c_char,
    _opaque: *mut std::os::raw::c_void,
) -> *mut std::os::raw::c_char {
    let base = unsafe { std::ffi::CStr::from_ptr(module_base_name) }
        .to_str()
        .unwrap_or("");
    let name = unsafe { std::ffi::CStr::from_ptr(module_name) }
        .to_str()
        .unwrap_or("");
    // 1. Exact (referrer, specifier) edge learned from deno's resolve callback.
    let mut canonical = lookup_resolved_specifier(base, name);
    // 2. Map miss for a *relative* specifier (./x, ../x): this happens for
    //    dynamic `import("./x")` — those edges aren't in the static resolution
    //    map. Resolve the path against the referrer ourselves (what QuickJS's
    //    default normalize did before we overrode it) so the loader can find the
    //    source deno registered under the resolved URL.
    if canonical.is_none() && (name.starts_with("./") || name.starts_with("../")) {
        canonical = Some(resolve_relative_specifier(base, name));
    }
    // 3. Bare builtin specifier (`url`, `path`, `fs/promises`, …) imported WITHOUT
    //    the `node:` prefix. deno resolves these to `node:<name>`, but a module
    //    pulled in on-demand by THIS loader (deep in the graph, never walked by
    //    InstantiateModule) has no recorded resolution edge, so the bare name
    //    misses the map. If deno has registered source under `node:<name>`, use
    //    that — otherwise the bare name has no source and the importing module's
    //    compile fails with a spurious `[uninitialized]`.
    if canonical.is_none()
        && !name.contains(':')
        && !name.starts_with('.')
        && !name.starts_with('/')
    {
        let node_name = format!("node:{name}");
        // node builtins are usually SYNTHETIC modules (CreateSyntheticModule, no
        // source text), so check QuickJS's loaded-module list as well as the
        // source registry.
        let cname = CString::new(node_name.as_str()).ok();
        let loaded = cname
            .as_ref()
            .map(|c| unsafe { v82jsc_has_loaded_module(ctx, c.as_ptr()) != 0 })
            .unwrap_or(false);
        if loaded || lookup_module_source_by_name(&node_name).is_some() {
            canonical = Some(node_name);
        }
    }
    // 4. Bare import-map / npm specifier (`preact`, `@scope/pkg`) reached
    //    on-demand, with no recorded (this-referrer, spec) edge: reuse any
    //    recorded resolution of the same bare specifier (import maps are global).
    if canonical.is_none() && !name.starts_with('.') && !name.starts_with('/') {
        canonical = lookup_resolved_specifier_any(name);
    }
    if std::env::var_os("QJS_DEBUG_MOD").is_some() {
        eprintln!(
            "[QJS normalize] base={base} name={name} -> {}",
            canonical.as_deref().unwrap_or("(identity)")
        );
    }
    match canonical {
        Some(resolved) => match CString::new(resolved) {
            Ok(c) => unsafe { js_strdup(ctx, c.as_ptr()) },
            Err(_) => unsafe { js_strdup(ctx, module_name) },
        },
        None => unsafe { js_strdup(ctx, module_name) },
    }
}

/// Resolve a relative module specifier (`./x`, `../x`) against a referrer URL/path,
/// collapsing `.`/`..` segments — mirrors standard ESM relative resolution so the
/// loader can find the source deno registered under the resolved name.
fn resolve_relative_specifier(base: &str, name: &str) -> std::string::String {
    // Split off any scheme prefix (e.g. `file://`) and keep it intact.
    let (scheme, base_path) = match base.find("://") {
        Some(i) => base.split_at(i + 3),
        None => ("", base),
    };
    // Directory of the referrer = everything up to the last '/'.
    let dir = match base_path.rfind('/') {
        Some(i) => &base_path[..i],
        None => "",
    };
    let mut segments: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in name.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            s => segments.push(s),
        }
    }
    // Preserve a leading '/' for absolute base paths (file:///… → leading slash).
    let joined = segments.join("/");
    if base_path.starts_with('/') {
        format!("{scheme}/{joined}")
    } else {
        format!("{scheme}{joined}")
    }
}

/// Walk the static-import graph from `root`, calling deno's `ResolveModuleCallback`
/// for every `(referrer, specifier)` edge to learn the resolved module name, and
/// record each edge in `RESOLVED_SPECIFIERS`. deno_core invokes our
/// `InstantiateModule` once on the entry module *after* the whole graph is loaded,
/// so every specifier resolves to an already-compiled module here.
unsafe fn build_resolution_map(
    ctx: *mut JSContext,
    context: *const Context,
    cb: ResolveModuleCallback,
    source_cb: Option<ResolveSourceCallback>,
    root: *const Module,
) {
    use std::collections::HashSet;
    let mut visited: HashSet<usize> = HashSet::new();
    let mut stack: Vec<*const Module> = vec![root];
    while let Some(m) = stack.pop() {
        if !visited.insert(handle_key(m)) {
            continue;
        }
        let Some((base, specs, src_imports)) = with_module_state(m, |st| {
            (
                st.source_name.clone(),
                st.import_specifiers.clone(),
                st.source_imports.clone(),
            )
        }) else {
            continue;
        };
        // Resolve WASM source-phase imports (`import source X from "Y"`, rewritten
        // to a `__v82jsc_wasm_src.get(id)` lookup) via deno's ResolveSourceCallback
        // and stash the returned WasmModuleObject under its id.
        if let Some(scb) = source_cb {
            for (id, spec) in &src_imports {
                unsafe { resolve_source_import(ctx, context, scb, m, *id, spec) };
            }
        }
        for (spec, _ty) in specs {
            let Ok(cspec) = CString::new(spec.as_str()) else {
                continue;
            };
            let sval = unsafe { JS_NewString(ctx, cspec.as_ptr()) };
            if sval.tag == JS_TAG_EXCEPTION {
                let exc = unsafe { JS_GetException(ctx) };
                unsafe { JS_FreeValue(ctx, exc) };
                continue;
            }
            let spec_handle = intern::<V8String>(sval);
            let attrs_handle = intern::<FixedArray>(unsafe { JS_NewArray(ctx) });
            if spec_handle.is_null() || attrs_handle.is_null() {
                continue;
            }
            // Local<T> is a transparent NonNull<T>; mint from our handle pointers.
            let (Some(ctx_local), Some(spec_local), Some(attrs_local), Some(ref_local)) = (
                unsafe { crate::Local::from_raw(context) },
                unsafe { crate::Local::from_raw(spec_handle) },
                unsafe { crate::Local::from_raw(attrs_handle) },
                unsafe { crate::Local::from_raw(m) },
            ) else {
                continue;
            };
            let ret = unsafe { cb(ctx_local, spec_local, attrs_local, ref_local) };
            // `ResolveModuleCallbackRet` is a #[repr(C)] newtype over the returned
            // `*const Module`; its field is private to `module.rs`, so recover the
            // pointer by transmute (identical layout).
            let resolved: *const Module = unsafe { std::mem::transmute(ret) };
            if resolved.is_null() {
                continue;
            }
            if let Some(rname) =
                with_module_state(resolved, |st| st.source_name.clone())
            {
                // Synthetic modules (e.g. `ext:core/ops`) carry an empty
                // source_name and register their JSModuleDef under the raw
                // specifier directly — don't remap those (an empty canonical name
                // would make the loader fail). Leave them to identity normalize.
                if !rname.is_empty() && rname != spec {
                    record_resolved_specifier(&base, &spec, &rname);
                }
                stack.push(resolved);
            }
        }
    }
}

/// Resolve one WASM source-phase import: call deno's `ResolveSourceCallback` to
/// get the `WasmModuleObject` for `spec` (relative to module `m`), then store it
/// in `globalThis.__v82jsc_wasm_src` keyed by `id` (the rewritten lookup reads it).
unsafe fn resolve_source_import(
    ctx: *mut JSContext,
    context: *const Context,
    scb: ResolveSourceCallback,
    m: *const Module,
    id: u64,
    spec: &str,
) {
    let Ok(cspec) = CString::new(spec) else {
        return;
    };
    let sval = unsafe { JS_NewString(ctx, cspec.as_ptr()) };
    if sval.tag == JS_TAG_EXCEPTION {
        unsafe {
            let e = JS_GetException(ctx);
            JS_FreeValue(ctx, e);
        }
        return;
    }
    let spec_handle = intern::<V8String>(sval);
    let attrs_handle = intern::<FixedArray>(unsafe { JS_NewArray(ctx) });
    let (Some(ctx_local), Some(spec_local), Some(attrs_local), Some(ref_local)) = (
        unsafe { crate::Local::from_raw(context) },
        unsafe { crate::Local::from_raw(spec_handle) },
        unsafe { crate::Local::from_raw(attrs_handle) },
        unsafe { crate::Local::from_raw(m) },
    ) else {
        return;
    };
    let ret = unsafe { scb(ctx_local, spec_local, attrs_local, ref_local) };
    // `ResolveSourceCallbackRet` is a #[repr(C)] newtype over `*const Object`.
    let obj: *const Object = unsafe { std::mem::transmute(ret) };
    if obj.is_null() {
        if std::env::var_os("QJS_TRACE_MOD").is_some() {
            eprintln!("[src-phase] resolve({spec}) -> null");
        }
        return;
    }
    let obj_val = jsval_of(obj);
    // globalThis.__v82jsc_wasm_src ??= new Map(); .set(id, obj)
    unsafe {
        let global = JS_GetGlobalObject(ctx);
        let mut map = JS_GetPropertyStr(ctx, global, c"__v82jsc_wasm_src".as_ptr());
        if !jsv_is_object(&map) {
            JS_FreeValue(ctx, map);
            let src = c"(globalThis.__v82jsc_wasm_src=new Map())";
            map = JS_Eval(
                ctx,
                src.as_ptr(),
                src.to_bytes().len(),
                c"<wasmsrc>".as_ptr(),
                JS_EVAL_TYPE_GLOBAL,
            );
        }
        let set_fn = JS_GetPropertyStr(ctx, map, c"set".as_ptr());
        let mut args = [JS_NewInt64(ctx, id as i64), JS_DupValue(ctx, obj_val)];
        let r = JS_Call(ctx, set_fn, map, 2, args.as_mut_ptr());
        if r.tag == JS_TAG_EXCEPTION {
            let e = JS_GetException(ctx);
            JS_FreeValue(ctx, e);
        } else {
            JS_FreeValue(ctx, r);
        }
        JS_FreeValue(ctx, args[0]);
        JS_FreeValue(ctx, args[1]);
        JS_FreeValue(ctx, set_fn);
        JS_FreeValue(ctx, map);
        JS_FreeValue(ctx, global);
        if std::env::var_os("QJS_TRACE_MOD").is_some() {
            eprintln!("[src-phase] resolve({spec}) -> stored id={id}");
        }
    }
}

/// Module loader registered via `JS_SetModuleLoaderFunc`. When QuickJS links a
/// module that statically imports `module_name`, it calls this to obtain the
/// dependency's `JSModuleDef`. We re-compile the source we stashed in
/// `CompileModule` (keyed by name) with `COMPILE_ONLY` and return its def.
pub(crate) unsafe extern "C" fn module_loader_callback(
    ctx: *mut JSContext,
    module_name: *const std::os::raw::c_char,
    _opaque: *mut std::os::raw::c_void,
) -> *mut JSModuleDef {
    let name = match unsafe { std::ffi::CStr::from_ptr(module_name) }.to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };
    let Some(source) = lookup_module_source_by_name(name) else {
        if std::env::var_os("QJS_DEBUG_EXC").is_some() {
            eprintln!("[QJS module loader] no source for {name}");
        }
        return ptr::null_mut();
    };
    let Ok(src_c) = CString::new(source.clone()) else {
        return ptr::null_mut();
    };
    let Ok(name_c) = CString::new(name) else {
        return ptr::null_mut();
    };

    // Defensive dedup: if QuickJS already has a module loaded under this exact
    // (normalized) name, return THAT def. Without this, importing the same module
    // from two separate `JS_Eval` roots (e.g. two on-demand `op_lazy_load_esm`
    // graphs each pulling node:stream) would each fall into the bytecode-cache /
    // fresh-compile paths below and `JS_ReadObject`/compile a SECOND def for the
    // same name — re-running its top-level body. For modules with non-idempotent
    // side effects (node:stream's `ObjectDefineProperty(pipeline, customPromisify,
    // …)` with implicit `configurable:false`) the second run throws "property is
    // not configurable". QuickJS resolves each import against the loaded-module
    // list, so handing back the existing def keeps one canonical instance.
    let existing = unsafe { v82jsc_get_loaded_module(ctx, name_c.as_ptr()) };
    if std::env::var_os("QJS_DEBUG_MOD").is_some() && name.contains("stream") {
        eprintln!("[loader] {name} existing_loaded={}", !existing.is_null());
    }
    if !existing.is_null() {
        MODULE_DEF_CACHE.with(|c| {
            c.borrow_mut().insert(name.to_string(), existing as usize);
        });
        return existing;
    }

    // Bytecode cache fast path: load precompiled module bytecode and skip parse.
    let key = bc_key(&source);
    if let Some(bytes) = bc_load(key) {
        if std::env::var_os("QJS_DEBUG_MOD").is_some() && name.contains("stream") {
            eprintln!("[loader] {name} -> BYTECODE path");
        }
        let m = unsafe {
            JS_ReadObject(ctx, bytes.as_ptr(), bytes.len(), JS_READ_OBJ_BYTECODE)
        };
        if m.tag == JS_TAG_MODULE {
            let def = unsafe { m.u.ptr } as *mut JSModuleDef;
            MODULE_DEF_CACHE.with(|c| {
                c.borrow_mut().insert(name.to_string(), def as usize);
            });
            unsafe { populate_import_meta(ctx, def, name) };
            return def;
        }
        // Stale/invalid bytecode (version mismatch, etc.): discard and recompile.
        if m.tag == JS_TAG_EXCEPTION {
            let exc = unsafe { JS_GetException(ctx) };
            unsafe { JS_FreeValue(ctx, exc) };
        } else {
            unsafe { JS_FreeValue(ctx, m) };
        }
    }

    if std::env::var_os("QJS_DEBUG_MOD").is_some() {
        let already = unsafe { v82jsc_has_loaded_module(ctx, name_c.as_ptr()) };
        eprintln!("[loader compile fresh] {name} (already_loaded={already})");
    }
    let result = unsafe {
        JS_Eval(
            ctx,
            src_c.as_ptr(),
            src_c.as_bytes().len(),
            name_c.as_ptr(),
            JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_COMPILE_ONLY,
        )
    };
    if result.tag == JS_TAG_EXCEPTION {
        if std::env::var_os("QJS_DEBUG_EXC").is_some() {
            unsafe {
                let exc = JS_GetException(ctx);
                let mut l = 0usize;
                let s = JS_ToCStringLen(ctx, &mut l, exc);
                if !s.is_null() {
                    let b = std::slice::from_raw_parts(s as *const u8, l);
                    eprintln!(
                        "[QJS module loader] parse failed for {name}: {}",
                        std::string::String::from_utf8_lossy(b)
                    );
                    JS_FreeCString(ctx, s);
                }
                JS_FreeValue(ctx, exc);
            }
        }
        return ptr::null_mut();
    }
    // Persist compiled bytecode for the next boot (does not consume `result`).
    unsafe { bc_write(ctx, key, result) };
    let m = unsafe { result.u.ptr } as *mut JSModuleDef;
    MODULE_DEF_CACHE.with(|c| {
        c.borrow_mut().insert(name.to_string(), m as usize);
    });
    unsafe { populate_import_meta(ctx, m, name) };
    // Ownership of the JSModuleDef transfers to QuickJS; don't free `result`.
    m
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
    let raw_text = unsafe { jsval_to_rust(ctx, src_val) };
    // Rewrite WASM source-phase imports (`import source X from "Y"`) into a map
    // lookup before anything else parses the source (QuickJS can't parse the
    // `import source` syntax).
    let (text, source_imports) = rewrite_source_phase(&raw_text);
    let specifier = unsafe { resource_name_of(ctx, source) };
    let fname = if specifier.is_empty() {
        "<module>".to_string()
    } else {
        specifier.clone()
    };

    let import_specifiers = parse_import_specifiers(&text);
    let is_async = has_top_level_await(&text);

    // Stash the source by name so the module loader can resolve static imports
    // of this module from other modules.
    register_module_source(&fname, &text);
    if std::env::var_os("QJS_DEBUG_MOD").is_some() {
        eprintln!("[QJS CompileModule] {fname} imports={import_specifiers:?}");
    }

    // We deliberately do NOT eagerly compile the module here.
    //
    // QuickJS resolves (and loads, via the module loader) every static import
    // during COMPILE — see quickjs.c `js_resolve_module`, called even with
    // COMPILE_ONLY. deno_core compiles a module *before* loading its
    // dependencies (it reads `GetModuleRequests` off the compiled module to
    // discover them), so a forward-referenced dependency's source isn't yet
    // registered and the compile fails. Worse, eagerly compiling each module
    // registers a `JSModuleDef` in QuickJS's per-runtime loaded-module list
    // keyed by name; mixing those deno-compiled defs (which deno never
    // evaluates — it relies on transitive evaluation) with the fresh defs the
    // loader compiles at evaluation time produces an inconsistent graph and
    // `[uninitialized]` binding errors.
    //
    // Instead we defer ALL compilation to `Module::Evaluate`. By the time deno
    // evaluates the entry-point module, every module's source has been
    // registered (deno calls CompileModule for each before evaluating any), so
    // a single `JS_Eval(MODULE)` of the entry point resolves, links and runs the
    // whole graph consistently via the loader. The textual import list feeds
    // deno's `GetModuleRequests`; `mark_all_modules_evaluated` reconciles the
    // lifecycle table afterward. This mirrors the reference PR's deferral.

    // The Module handle: a fresh object so it has stable pointer identity to key
    // our side table on.
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
    if !source_imports.is_empty() {
        PENDING_SOURCE_IMPORTS.with(|p| {
            p.borrow_mut().push((this, source_imports.clone()));
        });
    }
    record_module_state(
        this,
        ModuleState {
            status: ModuleStatus::Uninstantiated,
            module_def: ptr::null_mut(),
            bytecode: None,
            import_specifiers,
            source_imports,
            synthetic: false,
            is_async,
            source_text: text,
            source_name: fname,
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
        // deno's `op_compile_function` asserts the TryCatch caught an exception
        // when this returns null, so we must leave one pending.
        if !ctx.is_null() {
            unsafe { JS_ThrowTypeError(ctx, c"compile_function: invalid source".as_ptr()) };
        }
        return ptr::null();
    }
    let mut body = unsafe { jsval_to_rust(ctx, jsval_of(src)) };
    // A leading `#!` shebang is valid at the start of a script/module, but here
    // the body is wrapped inside `(function(){ … })`, where a shebang is a syntax
    // error. V8 tolerates it; QuickJS doesn't. Comment it out (preserve the line
    // so stack-trace line numbers stay correct) instead of stripping.
    if body.starts_with("#!") {
        body.replace_range(0..2, "//");
    }

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
        unsafe { JS_ThrowTypeError(ctx, c"compile_function: NUL in source".as_ptr()) };
        return ptr::null();
    };
    let len = csrc.as_bytes().len();
    // Use the real resource name (filename) as the eval name so that
    // `import(...)` *inside* the compiled function resolves relative specifiers
    // against this module's path (QuickJS reads JS_GetScriptOrModuleName at
    // import time). Node's CJS `wrapSafe` compiles module bodies via this op, and
    // those bodies use dynamic `import()` (e.g. Next.js's `bin/next`).
    let name = unsafe { resource_name_of(ctx, source) };
    let name_c = CString::new(if name.is_empty() {
        "<function>".to_string()
    } else {
        name
    })
    .unwrap_or_else(|_| CString::new("<function>").unwrap());
    let result = unsafe {
        JS_Eval(
            ctx,
            csrc.as_ptr(),
            len,
            name_c.as_ptr(),
            JS_EVAL_TYPE_GLOBAL,
        )
    };
    if result.tag == JS_TAG_EXCEPTION {
        // Leave the QuickJS exception PENDING (do not get/free it): deno's
        // `op_compile_function` asserts `tc_scope.has_caught()` when this returns
        // null and then surfaces the exception as the compile error.
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
    make_placeholder_code_cache()
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundModuleScript__CreateCodeCache(
    _script: *const UnboundModuleScript,
) -> *mut CachedData<'static> {
    make_placeholder_code_cache()
}

/// deno's `deno run` requires `create_code_cache()` to return Some; QuickJS has
/// no serializable bytecode here, so return a 1-byte owned placeholder (never
/// consumed — recompiles from source). BufferOwned so deno frees it.
pub(crate) fn make_placeholder_code_cache() -> *mut CachedData<'static> {
    #[repr(C)]
    struct RawCachedData {
        data: *const u8,
        length: i32,
        rejected: bool,
        buffer_policy: i32,
    }
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

/// `ScriptCompiler::CompileUnboundScript` — like `Compile`, but isolate-scoped
/// and yielding an `UnboundScript`. We validate with COMPILE_ONLY and carry the
/// source-text JSValue forward (BindToCurrentContext re-derives a Script).
#[unsafe(no_mangle)]
pub extern "C" fn v8__ScriptCompiler__CompileUnboundScript(
    isolate: *mut RealIsolate,
    source: *mut Source,
    _options: CompileOptions,
    _no_cache_reason: NoCacheReason,
) -> *const UnboundScript {
    if isolate.is_null() {
        return ptr::null();
    }
    let st = crate::qjs::shim_core::iso_state(isolate);
    let ctx = st.contexts.last().copied().unwrap_or(st.ctx);
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
    intern_dup::<UnboundScript>(ctx, src_val)
}

/// `UnboundScript::BindToCurrentContext` — produce a runnable `Script` from the
/// unbound script. The unbound script carries the source text; re-intern it as a
/// Script (which `Script::Run` evaluates).
#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundScript__BindToCurrentContext(
    script: *const UnboundScript,
) -> *const Script {
    if script.is_null() {
        return ptr::null();
    }
    let ctx = current_ctx();
    intern_dup::<Script>(ctx, jsval_of(script))
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__UnboundScript__GetSourceMappingURL(
    _script: *const UnboundScript,
) -> *const Value {
    // TODO(qjs): no script source-map URL available.
    intern::<Value>(jsv_undefined())
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
    let (specs, src_imports) = with_module_state(this, |m| {
        (m.import_specifiers.clone(), m.source_imports.clone())
    })
    .unwrap_or_default();

    // Build an Array of `{ specifier, __v8jsc_module_request: true }` objects so
    // deno can build its import graph and pre-resolve specifiers.
    let arr = unsafe { JS_NewArray(ctx) };
    if arr.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return ptr::null();
    }
    let mut idx = 0u32;
    // Source-phase requests (`import source X from "Y"`, rewritten out of the
    // source) must STILL be reported so deno's ModuleInfo has them — its
    // ResolveSourceCallback asserts the request exists. Mark them phase=kSource.
    for (_id, spec) in src_imports.iter() {
        let req = unsafe { JS_NewObject(ctx) };
        if req.tag == JS_TAG_EXCEPTION {
            unsafe { JS_FreeValue(ctx, req) };
            continue;
        }
        if let Ok(cspec) = CString::new(spec.as_str()) {
            let sval = unsafe { JS_NewString(ctx, cspec.as_ptr()) };
            unsafe { JS_SetPropertyStr(ctx, req, c"specifier".as_ptr(), sval) };
        }
        unsafe {
            JS_SetPropertyStr(ctx, req, c"__src_phase".as_ptr(), JS_NewBool(ctx, 1));
            JS_SetPropertyStr(ctx, req, c"__v8jsc_module_request".as_ptr(), JS_NewBool(ctx, 1));
            JS_SetPropertyUint32(ctx, arr, idx, req);
        }
        idx += 1;
    }
    for (spec, ty) in specs.iter() {
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
        // Carry the import-attribute `type` so GetImportAttributes can report it
        // (deno refuses to load a JSON/text/bytes module without it).
        if let Some(t) = ty {
            if let Ok(ct) = CString::new(t.as_str()) {
                let tval = unsafe { JS_NewString(ctx, ct.as_ptr()) };
                unsafe { JS_SetPropertyStr(ctx, req, c"__attr_type".as_ptr(), tval) };
            }
        }
        unsafe {
            JS_SetPropertyStr(
                ctx,
                req,
                c"__v8jsc_module_request".as_ptr(),
                JS_NewBool(ctx, 1),
            );
            // JS_SetPropertyUint32 consumes `req`.
            JS_SetPropertyUint32(ctx, arr, idx, req);
        }
        idx += 1;
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
    let mut def = with_module_state(this, |m| m.module_def).unwrap_or(ptr::null_mut());
    if def.is_null() {
        // The module was never driven through `Module::Evaluate` so its def isn't
        // set yet. This happens for `createLazyLoader` (`op_lazy_load_esm`)
        // builtins requested *after* the first eval: `mark_all_modules_evaluated`
        // reports them as already `Evaluated`, so deno skips evaluation and asks
        // straight for the namespace (e.g. node:fs's
        // `createLazyLoader("ext:deno_node/internal/fs/utils.mjs")()`, whose
        // `constants` export the fs bundle destructures). Compile+evaluate the
        // stored source on demand so we return the real namespace instead of the
        // empty-object fallback.
        def = unsafe { materialize_module_def(ctx, this) };
    }
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

/// Compile + evaluate a module's stored source on demand, populating its
/// `module_def`, and return that def (or null on failure / no stored source).
/// Mirrors `Module::Evaluate`'s deferred-compile path; used by
/// `GetModuleNamespace` for modules deno never evaluated itself.
unsafe fn materialize_module_def(
    ctx: *mut JSContext,
    this: *const Module,
) -> *mut JSModuleDef {
    let existing = with_module_state(this, |m| m.module_def).unwrap_or(ptr::null_mut());
    if !existing.is_null() {
        return existing;
    }
    let Some((source_text, source_name)) =
        with_module_state(this, |m| (m.source_text.clone(), m.source_name.clone()))
    else {
        return ptr::null_mut();
    };
    if source_text.is_empty() {
        return ptr::null_mut();
    }

    // Reuse a def QuickJS already loaded under this name rather than compiling +
    // evaluating a fresh one. The same module is frequently pulled in BOTH as a
    // static import of some root's graph (where the QuickJS loader compiled and
    // ran it) AND on-demand via `op_lazy_load_esm` → `GetModuleNamespace` (here).
    // Materializing a second def would re-run the module body — fatal for
    // non-idempotent top-level side effects (node:stream's `ObjectDefineProperty(
    // pipeline, customPromisify, …)`, implicit `configurable:false`, throws
    // "property is not configurable" on the second run). The loaded def's
    // namespace is the canonical one.
    if !source_name.is_empty() {
        if let Ok(cn) = CString::new(source_name.clone()) {
            let loaded = unsafe { v82jsc_get_loaded_module(ctx, cn.as_ptr()) };
            if !loaded.is_null() {
                with_module_state(this, |m| m.module_def = loaded);
                MODULE_DEF_CACHE.with(|c| {
                    c.borrow_mut().insert(source_name.clone(), loaded as usize);
                });
                // The loaded def may only be LINKED, not yet evaluated (its
                // `export let/const` bindings still in the temporal dead zone —
                // reading the namespace now yields "X is not initialized"). Drive
                // its body via `JS_EvalFunction` so the namespace is live. BUT only
                // when the def is fully idle: if it's already evaluated there's
                // nothing to do, and if it's currently EVALUATING (reached
                // re-entrantly while an outer `JS_Eval` graph is mid-flight) calling
                // `JS_EvalFunction` again corrupts QuickJS's module-evaluation stack
                // (`js_inner_module_evaluation` walks a half-built requested-modules
                // array → SIGSEGV). In the evaluating case the outer graph will
                // finish it, so just hand back the def.
                let is_ev = unsafe { v82jsc_module_is_evaluated(loaded) };
                let ev_started = unsafe { v82jsc_module_eval_started(loaded) };
                if is_ev == 0 && ev_started == 0 {
                    let mv = make_value(
                        JS_TAG_MODULE,
                        JSValueUnion { ptr: loaded as *mut std::os::raw::c_void },
                    );
                    let mv = unsafe { JS_DupValue(ctx, mv) };
                    let result = unsafe { JS_EvalFunction(ctx, mv) };
                    let iso2 = current_iso();
                    let rt2 = if iso2.is_null() {
                        ptr::null_mut()
                    } else {
                        iso_state(iso2).rt
                    };
                    unsafe { drain_jobs(rt2) };
                    if result.tag == JS_TAG_EXCEPTION {
                        let exc = unsafe { JS_GetException(ctx) };
                        unsafe { JS_FreeValue(ctx, exc) };
                    } else {
                        unsafe { JS_FreeValue(ctx, result) };
                    }
                }
                return loaded;
            }
        }
    }
    let iso = current_iso();
    let rt = if iso.is_null() {
        ptr::null_mut()
    } else {
        iso_state(iso).rt
    };
    let key = bc_key(&source_text);
    let Ok(cname) = CString::new(if source_name.is_empty() {
        "<module>".to_string()
    } else {
        source_name
    }) else {
        return ptr::null_mut();
    };

    // Produce a runnable module value: from bytecode cache, or compile-only.
    let mut module_val: Option<JSValue> = None;
    if let Some(bytes) = bc_load(key) {
        let m =
            unsafe { JS_ReadObject(ctx, bytes.as_ptr(), bytes.len(), JS_READ_OBJ_BYTECODE) };
        if m.tag == JS_TAG_MODULE {
            module_val = Some(m);
        } else if m.tag == JS_TAG_EXCEPTION {
            let exc = unsafe { JS_GetException(ctx) };
            unsafe { JS_FreeValue(ctx, exc) };
        } else {
            unsafe { JS_FreeValue(ctx, m) };
        }
    }
    if module_val.is_none() {
        if let Ok(csrc) = CString::new(source_text.clone()) {
            let c = unsafe {
                JS_Eval(
                    ctx,
                    csrc.as_ptr(),
                    csrc.as_bytes().len(),
                    cname.as_ptr(),
                    JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_COMPILE_ONLY,
                )
            };
            if c.tag == JS_TAG_MODULE {
                unsafe { bc_write(ctx, key, c) };
                module_val = Some(c);
            } else if c.tag == JS_TAG_EXCEPTION {
                let exc = unsafe { JS_GetException(ctx) };
                unsafe { JS_FreeValue(ctx, exc) };
            } else {
                unsafe { JS_FreeValue(ctx, c) };
            }
        }
    }
    let Some(mv) = module_val else {
        return ptr::null_mut();
    };
    // `mv` is a JS_TAG_MODULE value whose payload IS the def; the def outlives the
    // value (it lives in the runtime), so read it before JS_EvalFunction consumes mv.
    let def = unsafe { mv.u.ptr } as *mut JSModuleDef;
    let meta_name = with_module_state(this, |m| {
        m.module_def = def;
        m.status = ModuleStatus::Evaluated;
        m.source_name.clone()
    })
    .unwrap_or_default();
    unsafe { populate_import_meta(ctx, def, &meta_name) };
    let result = unsafe { JS_EvalFunction(ctx, mv) };
    unsafe { drain_jobs(rt) };
    if result.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        if std::env::var_os("QJS_DEBUG_MOD").is_some() {
            let s = unsafe { jsval_to_rust(ctx, exc) };
            eprintln!(
                "[qjs materialize_module {}] exception: {s}",
                cname.to_string_lossy()
            );
        }
        unsafe { JS_FreeValue(ctx, exc) };
    } else {
        unsafe { JS_FreeValue(ctx, result) };
    }
    def
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
    context: *const Context,
    cb: ResolveModuleCallback,
    source_callback: Option<ResolveSourceCallback>,
) -> MaybeBool {
    // QuickJS performs real module linking/resolution at evaluation time via its
    // registered module loader (see JS_SetModuleLoaderFunc / module_loader_*),
    // so we don't need to walk the import graph here to *link*. But QuickJS's
    // loader is handed the raw specifier from source text, while deno registers
    // each module's source under its *resolved* name (e.g. `npm:express@4` →
    // `file:///…/index.js`). We learn that mapping now by walking deno's resolve
    // callback over the graph, so `module_normalize_callback` can canonicalize
    // specifiers at link time. Cheap (hashmap lookups in deno) and safe — on any
    // miss the normalizer falls back to identity (the old behavior).
    if let Some(scb) = source_callback {
        // SAFETY: the fn pointer is 'static; the lifetime param is purely a
        // borrow marker on the Local args, not the function itself.
        SOURCE_CB.with(|c| c.set(Some(unsafe { std::mem::transmute(scb) })));
    }
    let ctx = ctx_of(context);
    if !ctx.is_null() {
        unsafe { build_resolution_map(ctx, context, cb, source_callback, this) };
        // Resolve EVERY pending WASM source-phase import (each module that has
        // any), not just those in this root's static graph — wasm-bindgen loader
        // wrappers are reached only via on-demand dynamic-import chains.
        if let Some(scb) = source_callback {
            let pending: Vec<_> =
                PENDING_SOURCE_IMPORTS.with(|p| p.borrow_mut().drain(..).collect());
            for (referrer, imports) in pending {
                for (id, spec) in &imports {
                    unsafe { resolve_source_import(ctx, context, scb, referrer, *id, spec) };
                }
            }
        }
    }
    // Just advance the lifecycle state so deno's pre-evaluate assertions pass.
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

/// Build a resolved `Promise` (`Promise.resolve(undefined)`) as a `Value` handle.
/// `Module::Evaluate` must return a Promise (deno unwraps it as one); used for the
/// idempotent already-evaluated path.
fn make_resolved_promise(ctx: *mut JSContext) -> *const Value {
    let global = unsafe { JS_GetGlobalObject(ctx) };
    let promise_ctor = unsafe { JS_GetPropertyStr(ctx, global, c"Promise".as_ptr()) };
    let resolve_fn = unsafe { JS_GetPropertyStr(ctx, promise_ctor, c"resolve".as_ptr()) };
    let r = unsafe { JS_Call(ctx, resolve_fn, promise_ctor, 0, ptr::null_mut()) };
    unsafe {
        JS_FreeValue(ctx, resolve_fn);
        JS_FreeValue(ctx, promise_ctor);
        JS_FreeValue(ctx, global);
    }
    if r.tag == JS_TAG_EXCEPTION {
        let exc = unsafe { JS_GetException(ctx) };
        unsafe { JS_FreeValue(ctx, exc) };
        return intern::<Value>(jsv_undefined());
    }
    intern::<Value>(r)
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
    if std::env::var_os("QJS_DEBUG_EVAL").is_some() {
        let nm = with_module_state(this, |m| m.source_name.clone()).unwrap_or_default();
        let md = with_module_state(this, |m| !m.module_def.is_null()).unwrap_or(false);
        eprintln!("[EVAL-ENTRY] this={:?} name={nm:?} has_module_def={md}", this);
    }
    let iso = current_iso();
    let rt = if iso.is_null() {
        ptr::null_mut()
    } else {
        iso_state(iso).rt
    };
    let _t0 = if std::env::var_os("V82JSC_TIMING").is_some() {
        Some(std::time::Instant::now())
    } else {
        None
    };

    // Idempotency guard: if this module already has a `module_def`, its body has
    // already run (via a prior Evaluate or `materialize_module_def`). Re-running
    // would re-execute top-level side effects — and non-idempotent ones fail the
    // second time (e.g. node:stream's `ObjectDefineProperty(pipeline,
    // customPromisify, …)` with implicit `configurable:false` throws "property is
    // not configurable"). deno can call evaluate() on a module that our model
    // already ran transitively as part of another module's graph, so guard here.
    let already_evaluated = with_module_state(this, |m| {
        if !m.module_def.is_null() {
            m.status = ModuleStatus::Evaluated;
            true
        } else {
            false
        }
    })
    .unwrap_or(false);
    if already_evaluated {
        return make_resolved_promise(ctx);
    }

    // Take the bytecode (consumed once) and mark Evaluated. Also grab the
    // source text/name for the deferred-compile fallback.
    let (bytecode, source_text, source_name) = with_module_state(this, |m| {
        m.status = ModuleStatus::Evaluated;
        (
            m.bytecode.take(),
            m.source_text.clone(),
            m.source_name.clone(),
        )
    })
    .unwrap_or((None, std::string::String::new(), std::string::String::new()));
    let source_name_dbg = source_name.clone();

    // NB: WASM source-phase imports (`import source X from "Y.wasm"`) are resolved
    // exclusively during `InstantiateModule` (see `build_resolution_map` + the
    // `PENDING_SOURCE_IMPORTS` drain). They MUST NOT be resolved here: deno's
    // `module_source_callback` reads a `*const ModuleMap` from an isolate slot that
    // deno sets *only* around its `instantiate_module2` call and removes immediately
    // after (modules/map.rs `instantiate_module`). Invoking the source callback at
    // evaluation time — outside that window — makes deno's `get_slot().unwrap()`
    // panic with "called `Option::unwrap()` on a `None` value". deno always calls
    // InstantiateModule before Evaluate (static and dynamic imports alike), and our
    // InstantiateModule drains EVERY pending source import (not just this root's
    // static graph), so the map is already populated by the time we get here.

    // Reuse an already-loaded def. QuickJS's module loader may have compiled this
    // same module (by name) as a static dependency of another graph and run its
    // body there. Compiling a *fresh* def here and re-running it would duplicate
    // top-level side effects (the node:stream `customPromisify` define fails on
    // the second run). Instead reuse QuickJS's cached def: `JS_EvalFunction` on it
    // links+evaluates idempotently (`js_evaluate_module` returns the existing
    // promise when the module is already evaluated — no double-run).
    if !source_name.is_empty() {
        let cached = MODULE_DEF_CACHE.with(|c| c.borrow().get(&source_name).copied());
        if std::env::var_os("QJS_DEBUG_MOD").is_some() && source_name.contains("stream") {
            let (isev, evst) = match cached {
                Some(d) => unsafe {
                    (
                        v82jsc_module_is_evaluated(d as *mut JSModuleDef),
                        v82jsc_module_eval_started(d as *mut JSModuleDef),
                    )
                },
                None => (-1, -1),
            };
            eprintln!(
                "[STREAM-EVAL] {source_name} cached={} is_evaluated={isev} eval_started={evst} bytecode={}",
                cached.is_some(),
                bytecode.is_some()
            );
        }
        // Only reuse a cached def whose body has ALREADY run. A compiled-but-not-
        // yet-evaluated def must instead evaluate through the normal path so it runs
        // in its importer's graph order (reusing it early breaks static-import
        // entries). An already-evaluated def is reused so we don't re-run its body
        // (which would duplicate non-idempotent top-level side effects).
        if let Some(d) = cached {
            let def = d as *mut JSModuleDef;
            // If the cached def is CURRENTLY EVALUATING (mid-graph) — not yet
            // finished — it was reached via another root's JS_Eval (e.g. a Vite
            // chunk pulled in by a dynamic import). deno still calls Evaluate on it
            // as its own root; doing a fresh JS_Eval here would compile a SECOND
            // def for the same name and desync the cyclic live bindings, surfacing
            // as a thrown `[uninitialized]`. Record the in-flight def and return a
            // resolved promise — the owning graph's evaluation finishes it.
            if unsafe { v82jsc_module_is_evaluated(def) } == 0
                && unsafe { v82jsc_module_eval_started(def) } != 0
            {
                with_module_state(this, |m| m.module_def = def);
                return make_resolved_promise(ctx);
            }
            if unsafe { v82jsc_module_is_evaluated(def) } == 0 {
                // Not evaluated yet (only LINKED) — fall through to the normal path.
            } else {
            with_module_state(this, |m| m.module_def = def);
            let mv = make_value(
                JS_TAG_MODULE,
                JSValueUnion {
                    ptr: def as *mut std::os::raw::c_void,
                },
            );
            let mv = unsafe { JS_DupValue(ctx, mv) };
            let result = unsafe { JS_EvalFunction(ctx, mv) };
            unsafe { drain_jobs(rt) };
            if result.tag == JS_TAG_EXCEPTION {
                let exc = unsafe { JS_GetException(ctx) };
                if std::env::var_os("QJS_DEBUG_MOD").is_some() {
                    let s = unsafe { jsval_to_rust(ctx, exc) };
                    eprintln!("[qjs Evaluate reuse {source_name}] exception: {s}");
                }
                unsafe { JS_FreeValue(ctx, exc) };
                return make_resolved_promise(ctx);
            }
            return intern::<Value>(result);
            }
        }
    }

    if std::env::var_os("QJS_DEBUG_MOD").is_some() {
        eprintln!(
            "[QJS Evaluate] {} (precompiled={})",
            source_name,
            bytecode.is_some()
        );
    }
    // For a module whose body is ASYNC (top-level await, or it transitively
    // `await import(...)`s — e.g. @deno/loader's `await import("./rs_lib.js")`),
    // JS_EvalFunction returns a PENDING promise that settles only after the event
    // loop pumps the async work. We must hand THAT promise back to deno so its
    // dynamic-import / `await import()` waits for real completion; returning a
    // freshly-resolved promise makes deno read an empty namespace (undefined
    // exports). Captured here, returned at the end.
    let mut async_promise: Option<JSValue> = None;
    if let Some(bc) = bytecode {
        // Pre-compiled bytecode path. JS_EvalFunction consumes one ref of the
        // bytecode value. We own it (+1), so hand it straight over (no dup).
        let result = unsafe { JS_EvalFunction(ctx, bc) };
        unsafe { drain_jobs(rt) };
        if result.tag == JS_TAG_EXCEPTION {
            let exc = unsafe { JS_GetException(ctx) };
            if !iso.is_null() {
                let s = unsafe { jsval_to_rust(ctx, exc) };
                if !s.is_empty() {
                    eprintln!("[qjs] Module::evaluate exception: {s}");
                }
            }
            unsafe { JS_FreeValue(ctx, exc) };
        } else if result.tag == JS_TAG_OBJECT
            && unsafe { JS_IsPromise(result) } != 0
            && unsafe { JS_PromiseState(ctx, result) } == 0
        {
            // Pending async module: hand the real promise back to deno.
            async_promise = Some(result);
        } else {
            unsafe { JS_FreeValue(ctx, result) };
        }
    } else if !source_text.is_empty() {
        // Deferred path. By now every dependency's source is registered with the
        // loader, so the module compiles/links/runs the whole graph. We make this
        // the PRIMARY startup-cost path bytecode-cacheable: load precompiled
        // bytecode if present (skip parse), else COMPILE_ONLY + persist, then run
        // with JS_EvalFunction. Falls back to a plain JS_Eval(MODULE) if the
        // compile-only step can't produce a module value.
        let key = bc_key(&source_text);
        let cname = CString::new(if source_name.is_empty() {
            "<module>".to_string()
        } else {
            source_name
        })
        .ok();
        if let Some(cname) = cname {
            // Produce a runnable module value: from cache, or freshly compiled.
            let mut module_val: Option<JSValue> = None;
            if let Some(bytes) = bc_load(key) {
                let m = unsafe {
                    JS_ReadObject(ctx, bytes.as_ptr(), bytes.len(), JS_READ_OBJ_BYTECODE)
                };
                if m.tag == JS_TAG_MODULE {
                    module_val = Some(m);
                } else if m.tag == JS_TAG_EXCEPTION {
                    let exc = unsafe { JS_GetException(ctx) };
                    unsafe { JS_FreeValue(ctx, exc) };
                } else {
                    unsafe { JS_FreeValue(ctx, m) };
                }
            }
            if module_val.is_none() {
                if let Ok(csrc) = CString::new(source_text.clone()) {
                    let c = unsafe {
                        JS_Eval(
                            ctx,
                            csrc.as_ptr(),
                            csrc.as_bytes().len(),
                            cname.as_ptr(),
                            JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_COMPILE_ONLY,
                        )
                    };
                    if c.tag == JS_TAG_MODULE {
                        unsafe { bc_write(ctx, key, c) };
                        module_val = Some(c);
                    } else if c.tag == JS_TAG_EXCEPTION {
                        let exc = unsafe { JS_GetException(ctx) };
                        unsafe { JS_FreeValue(ctx, exc) };
                    } else {
                        unsafe { JS_FreeValue(ctx, c) };
                    }
                }
            }
            // Run it. JS_EvalFunction consumes the module value (+1) we own.
            let result = if let Some(mv) = module_val {
                // Record the JSModuleDef so `GetModuleNamespace` (used by dynamic
                // `import()` to read the module's exports) returns the real
                // namespace instead of an empty fallback object. `mv` is a
                // JS_TAG_MODULE value whose payload IS the def.
                let def = unsafe { mv.u.ptr } as *mut JSModuleDef;
                with_module_state(this, |m| m.module_def = def);
                unsafe { populate_import_meta(ctx, def, &source_name_dbg) };
                unsafe { JS_EvalFunction(ctx, mv) }
            } else if let Ok(csrc) = CString::new(source_text.clone()) {
                // Fallback: compile+run from source in one step.
                unsafe {
                    JS_Eval(
                        ctx,
                        csrc.as_ptr(),
                        csrc.as_bytes().len(),
                        cname.as_ptr(),
                        JS_EVAL_TYPE_MODULE,
                    )
                }
            } else {
                jsv_undefined()
            };
            // Capture a synchronous module-eval exception BEFORE draining the job
            // queue: JS_ExecutePendingJob saves/clears `current_exception`, so
            // draining first would erase the real error and leave only the cleared
            // [uninitialized] sentinel.
            if std::env::var_os("QJS_DBG_UNINIT").is_some() {
                let he = unsafe { JS_HasException(ctx) };
                eprintln!("[eval-result] tag={} has_exception={he}", result.tag);
            }
            let sync_exc = if result.tag == JS_TAG_EXCEPTION {
                Some(unsafe { JS_GetException(ctx) })
            } else {
                None
            };
            unsafe { drain_jobs(rt) };
            if let Some(exc) = sync_exc {
                let s = unsafe { jsval_to_rust(ctx, exc) };
                if !s.is_empty() {
                    eprintln!("[qjs] Module::evaluate (deferred) exception: {s} (module={source_name_dbg})");
                    if std::env::var_os("QJS_DEBUG_MOD").is_some() {
                        let stk = unsafe { JS_GetPropertyStr(ctx, exc, c"stack".as_ptr()) };
                        if !jsv_is_undefined(&stk) {
                            let ss = unsafe { jsval_to_rust(ctx, stk) };
                            eprintln!("[qjs] stack:\n{ss}");
                        }
                        unsafe { JS_FreeValue(ctx, stk) };
                    }
                }
                unsafe { JS_FreeValue(ctx, exc) };
            } else {
                if std::env::var_os("QJS_DEBUG_MOD").is_some() {
                    // If the module eval returned a promise, inspect its state —
                    // a rejected top-level promise means the body failed.
                    if result.tag == JS_TAG_OBJECT && unsafe { JS_IsPromise(result) } != 0 {
                        let state = unsafe { JS_PromiseState(ctx, result) };
                        eprintln!("[QJS Evaluate-result] promise state={state}");
                        if state == 2 {
                            let pr = unsafe { JS_PromiseResult(ctx, result) };
                            let s = unsafe { jsval_to_rust(ctx, pr) };
                            eprintln!("[QJS Evaluate-result] rejection: {s}");
                            let stk = unsafe { JS_GetPropertyStr(ctx, pr, c"stack".as_ptr()) };
                            if !jsv_is_undefined(&stk) {
                                let ss = unsafe { jsval_to_rust(ctx, stk) };
                                eprintln!("[QJS Evaluate-result] stack:\n{ss}");
                            }
                            unsafe { JS_FreeValue(ctx, stk) };
                            unsafe { JS_FreeValue(ctx, pr) };
                        }
                    } else {
                        eprintln!("[QJS Evaluate-result] tag={}", result.tag);
                    }
                }
                if result.tag == JS_TAG_OBJECT
                    && unsafe { JS_IsPromise(result) } != 0
                    && unsafe { JS_PromiseState(ctx, result) } == 0
                {
                    // Pending async module — return the real promise to deno.
                    async_promise = Some(result);
                } else if result.tag != JS_TAG_UNDEFINED {
                    unsafe { JS_FreeValue(ctx, result) };
                }
            }
        }
    }

    if let Some(t0) = _t0 {
        eprintln!(
            "[V82JSC_TIMING] Module::Evaluate {} took {:.2} ms",
            source_name_dbg,
            t0.elapsed().as_secs_f64() * 1000.0
        );
        crate::qjs::fam_function::timing::dump();
    }

    // A still-pending async module body: return its real promise so deno's
    // dynamic import / `await import()` waits for the async chain to finish.
    if let Some(p) = async_promise {
        // Do NOT mark all modules evaluated yet — this one is genuinely still
        // running; deno will see it complete when the promise settles.
        return intern::<Value>(p);
    }

    // QuickJS evaluated all statically-imported modules transitively; reflect
    // that so deno's post-evaluate assertion sees every module as Evaluated.
    mark_all_modules_evaluated();

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
    evaluation_steps: SyntheticModuleEvaluationSteps,
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
    if std::env::var_os("QJS_DEBUG_MOD").is_some() {
        let nm = unsafe { jsval_to_rust(ctx, jsval_of(module_name)) };
        eprintln!("[QJS CreateSyntheticModule] {nm} def={def:?} n_export_names={export_names_len}");
    }
    SYNTHETIC_DEFS.with(|t| {
        t.borrow_mut().insert(handle_key(this), def as usize);
    });
    // Stash deno's evaluation steps + this Module handle so the CModule init
    // callback can run them to populate exports at link time (see the
    // SYNTHETIC_EVAL_STEPS doc-comment).
    let steps: SyntheticModuleEvaluationSteps<'static> =
        unsafe { std::mem::transmute(evaluation_steps) };
    // Dup the handle's JSValue so we can re-intern a fresh, valid handle later
    // (the original arena slot will be gone by init-callback time).
    let handle_dup = unsafe { JS_DupValue(ctx, handle_val) };
    SYNTHETIC_EVAL_STEPS.with(|t| {
        t.borrow_mut().insert(def as usize, (steps, handle_dup));
    });
    record_module_state(
        this,
        ModuleState {
            status: ModuleStatus::Instantiated,
            module_def: def,
            bytecode: None,
            import_specifiers: Vec::new(),
            source_imports: Vec::new(),
            synthetic: true,
            is_async: false,
            source_text: std::string::String::new(),
            source_name: std::string::String::new(),
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
    // Run deno's evaluation steps first: they call `SetSyntheticModuleExport`
    // for each export, which stashes (name, value) pairs into SYNTHETIC_EXPORTS
    // keyed by this def. V8 runs these at evaluate time; QuickJS needs the
    // exports now (link time), so we drive them here.
    let steps = SYNTHETIC_EVAL_STEPS.with(|t| t.borrow().get(&(m as usize)).copied());
    if let Some((eval_steps, handle_jsval)) = steps {
        let cur_ctx = current_ctx();
        let ctx_for_call = if cur_ctx.is_null() { ctx } else { cur_ctx };
        // Re-intern fresh handles for the call: the original Context/Module
        // arena slots are long gone. `intern_dup` dups the stored module JSValue
        // into a new slot in the current handle scope so its identity (and thus
        // deno's Global<Module> key) matches what deno recorded.
        let ctx_handle = super::shim_core::intern_ctx(ctx_for_call);
        let mod_handle = super::shim_core::intern_dup::<Module>(ctx_for_call, handle_jsval);
        unsafe {
            if let (Some(ctx_l), Some(mod_l)) = (
                crate::Local::from_raw(ctx_handle),
                crate::Local::from_raw(mod_handle),
            ) {
                let _ = eval_steps(ctx_l, mod_l);
            }
        }
    }

    let exports = SYNTHETIC_EXPORTS.with(|t| t.borrow_mut().remove(&(m as usize)).unwrap_or_default());
    if std::env::var_os("QJS_DEBUG_MOD").is_some() {
        eprintln!("[QJS synthetic init] def={:?} n_exports={}", m, exports.len());
    }
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
    if std::env::var_os("QJS_DEBUG_MOD").is_some() {
        eprintln!("[QJS SetSyntheticExport] def={def:?} name={name}");
    }
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
pub extern "C" fn v8__ModuleRequest__GetPhase(this: *const ModuleRequest) -> ModuleImportPhase {
    let ctx = current_ctx();
    if !ctx.is_null() && !this.is_null() {
        let v = unsafe { JS_GetPropertyStr(ctx, jsval_of(this), c"__src_phase".as_ptr()) };
        let is_src = unsafe { JS_ToBool(ctx, v) } != 0;
        unsafe { JS_FreeValue(ctx, v) };
        if is_src {
            return ModuleImportPhase::kSource;
        }
    }
    ModuleImportPhase::kEvaluation
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ModuleRequest__GetSourceOffset(_this: *const ModuleRequest) -> int {
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn v8__ModuleRequest__GetImportAttributes(
    this: *const ModuleRequest,
) -> *const FixedArray {
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
    // deno expects STATIC-import attributes as flat triples
    // [key, value, sourceOffset, ...]. We carry only the `type` attribute (set
    // on the request object as `__attr_type`).
    if !this.is_null() {
        let req = jsval_of(this);
        let ty = unsafe { JS_GetPropertyStr(ctx, req, c"__attr_type".as_ptr()) };
        if jsv_is_string(&ty) {
            unsafe {
                let key = JS_NewString(ctx, c"type".as_ptr());
                JS_SetPropertyUint32(ctx, arr, 0, key);
                // `ty` is owned (+1); hand it to the array.
                JS_SetPropertyUint32(ctx, arr, 1, ty);
                JS_SetPropertyUint32(ctx, arr, 2, JS_NewInt32(ctx, 0));
            }
        } else {
            unsafe { JS_FreeValue(ctx, ty) };
        }
    }
    intern::<FixedArray>(arr)
}
