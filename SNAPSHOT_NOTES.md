# Startup / snapshot plan (v8x JSC + QuickJS)

## Problem
Built `--features hmr` → **no V8 startup snapshot**. Every boot re-parses + re-compiles
all extension JS. Measured startup: V8 (snapshot) ~10 ms; QuickJS ~160 ms; JSC ~180 ms.
The ~150 ms delta is parse/compile of ext JS, not engine speed.

V8's snapshot serializes the whole initialized heap. We can't replicate heap
serialization through JSC/QuickJS public APIs. But we CAN remove the parse/compile
cost with **per-module bytecode caching** — the same hooks deno_core already drives
for V8's code cache.

## Hook points (already exist as placeholders, commit cf1dcb7)
deno_core compiles each ext module via `ScriptCompiler::CompileUnboundScript` and,
when a `code_cache` callback is set, calls `UnboundScript::CreateCodeCache` to get
bytes to persist (DENO_DIR), then feeds them back as `CachedData` on the next boot.

Shim fns to make real:
- JSC: `src/shim_module.rs`
  - `v8__ScriptCompiler__CompileUnboundScript` — if Source has cached_data, load bytecode instead of parsing.
  - `v8__UnboundScript__CreateCodeCache` — currently returns 1-byte placeholder; must emit real bytecode.
  - CachedData NEW/DELETE/GetCachedData round-trip (already structurally there).
- QuickJS: `src/qjs/fam_module.rs` (same fn names).

## QuickJS: DOABLE via public API ✅
Bytecode cache fully supported:
```
// compile-only -> bytecode object
JSValue fn = JS_Eval(ctx, src, len, name, JS_EVAL_FLAG_COMPILE_ONLY | type);
// serialize
uint8_t* buf = JS_WriteObject(ctx, &size, fn, JS_WRITE_OBJ_BYTECODE);   // -> CachedData
// later boot: deserialize + run
JSValue fn = JS_ReadObject(ctx, buf, size, JS_READ_OBJ_BYTECODE);
JS_EvalFunction(ctx, fn);   // runs it
```
Map: CreateCodeCache = JS_WriteObject(bytecode); CompileUnboundScript(cached_data)
= JS_ReadObject; Script::Run of a cached script = JS_EvalFunction.
Expected: QuickJS startup 160 ms → ~15–25 ms (skip parse; interpreter, no JIT warmup).
Constants already in quickjs.h: JS_EVAL_FLAG_COMPILE_ONLY (1<<5), JS_WRITE_OBJ_BYTECODE
(1<<0), JS_READ_OBJ_BYTECODE (1<<0). JS_WriteObject/ReadObject/EvalFunction exported.

## JSC: HARDER ⚠️
Public C API exposes only `JSScriptCreateFromString` / `JSScriptEvaluate` /
`JSScriptRetain/Release` — **no C bytecode-cache writer**. The Obj-C `JSScript`
has a `cachePath` (JSC writes/reads a bytecode cache file itself), but the C
`JSScriptCreateFromString` takes no cache path.
Options, best→worst:
1. **Obj-C JSScript with cachePath** — call `+[JSScript scriptOfType:withSource:andSourceURL:andBytecodeCache:inVirtualMachine:error:]`
   via objc_msgSend, giving a cache-file URL. JSC compiles once, caches bytecode to
   that file, memory-maps it on subsequent boots. This is what Apple intends for AOT.
   Needs: a JSVirtualMachine* (we have the VM via JSContextGetGroup), objc runtime calls.
2. **JSC::CodeCache C++ internals** — like the `_ZN3JSC2VM15drainMicrotasksEv` trick;
   fragile, version-specific.
3. Punt: JSC keeps re-parsing (180 ms) until (1) lands. JIT entitlement already gives
   the compute/HTTP wins; startup is the one axis still V8's.

→ Do QuickJS bytecode cache first (clean, big win). Then attempt JSC option (1)
   (Obj-C JSScript + cachePath) as a follow-up.

## Heap-image idea (mentioned to Ryan, separate/invasive)
Replace snapshot *deserialize* with a memcpy'd heap image + manual external-pointer
fixup (~0.9 ms saving on V8). Not portable to JSC/QuickJS; out of scope here.

## Order of work
1. (agents) finish symbol tail so real apps run — prerequisite to measuring real startup.
2. QuickJS bytecode cache in fam_module.rs → re-measure startup.
3. JSC Obj-C JSScript+cachePath → re-measure.
