# D0: Deno boot recon on the Hermes backend

Goal of this cycle: not to boot Deno, but to map the exact path from the current
Hermes backend (81 rusty_v8 tests: objects, arrays, functions, callbacks,
exceptions, templates, accessors, identity) to `deno_core::JsRuntime` running a
script, and to find the first walls with a reproducible probe. This is recon.

## TL;DR

- The first wall is **Promises + microtasks + ES modules**, all three of which
  are pure null-pointer link stubs today. A reproducible in-repo probe
  (`cargo test ... hermes_boot_probe`) proves it: the isolate + context +
  classic-script boot passes, and the Promise, microtask, and ES-module probes
  each fail naming the exact stubbed `v8__*`.
- `deno_core` cannot boot from classic scripts alone. Even with no snapshot, its
  own core runtime JS (`ext:core/mod.js`) is loaded as an **ES module**, and
  `ext:core/ops` is a **synthetic ES module**. So the module subsystem is on the
  minimal boot path, not deferred to user code.
- Ordered roadmap to boot: (1) Promises + microtask queue, (2) ES modules
  (source-text instantiate/evaluate + synthetic modules + resolve callback),
  (3) op glue verification (External is already real; needs the module +
  promise plumbing under it), (4) event-loop drive (promise resolution +
  microtask checkpoint already covered by 1+2).

## The measurable target (reproducible)

An in-repo probe in `src/hermes/mod.rs`, module `hermes_boot_probe`, gated
`#[cfg(all(test, feature = "link_hermes"))]`. It walks the boot-critical v8
subsystems in dependency order through the SAME vendored rusty_v8 Rust surface
`deno_core` drives, so each failure names the exact `v8__*` wall.

Run it:

```
cargo test --no-default-features --features hermes,link_hermes \
  --lib hermes_boot_probe -- --test-threads=1 --nocapture
```

Current result (this branch, macOS arm64):

```
boot_baseline_isolate_context_script ... ok
boot_promise_resolver_roundtrip      ... FAILED  (v8__Promise__Resolver__New is a stub)
boot_microtask_enqueue_and_checkpoint ... FAILED (PerformMicrotaskCheckpoint is a no-op)
boot_es_module_instantiate_evaluate  ... FAILED  (v8__ScriptCompiler__CompileModule is a stub)

test result: FAILED. 1 passed; 3 failed
```

`boot_baseline` passing is the concrete "deno_core `JsRuntime::new` +
`execute_script("1 + 1")` engine primitive works". The three failures are the
three walls, in the order they must be knocked down.

## Why option (a) (the deno_core harness cell) was not run

`tests/harness/config.json` already declares a `deno_core` suite for `hermes`
(baseline `tests/status/baselines/hermes/deno_core.txt`, currently 0 passing).
Wiring it for real needs all of:

1. A deno checkout whose aliased `v8` dependency is a **path dep** at this
   repo's root. The available checkout `/Users/divy/gh/deno-v8x-rebase` does
   NOT qualify: its `libs/deno_v8/Cargo.toml` pulls `v8x` from crates.io
   (`package = "v8x", version = "=149.4.0-rc.1"`), not a path, and exposes only a
   `quickjs` feature alias, no `hermes`/`link_hermes`. `run.mjs`'s
   `ensureDenoV8Patch` hard-fails unless a manifest points at `ROOT`.
2. Adding a `hermes` feature alias to `libs/deno_v8` and repointing `v8x_backend`
   at the local path, then a full `cargo nextest` build of `deno_core` and all
   its dependencies.

Disk at recon time was ~5.5 GiB free (budget ~7 GiB); a full `deno_core` build
tree does not fit safely, and the build cannot even link until Promises +
modules exist (walls proven below). So the in-repo probe (option b) is the
correct measurable target for D0: it reuses the already-present vendored
rusty_v8 dependency, needs no new heavy build, and pinpoints the same walls the
deno_core link would hit, sooner. Re-attempt the harness cell after roadmap
steps 1-2 land.

## Refinement: two boot flavors, both blocked before the Promise/module walls

A second, deeper trace (`runtime/setup.rs`, `runtime/bindings.rs`) split the boot
into two flavors:

- The core bootstrap JS `00_primordials.js` / `00_infra.js` is compiled as
  **classic scripts** (`v8::Script::compile`, `bindings.rs:~398`), not modules.
  So the very first bootstrap runs on the same classic-script path
  `boot_baseline` already proves green.
- `ext:core/mod.js` + `ext:core/ops` are the **ES-module** layer on top
  (`init_extension_js`), which is where the module wall lands.

But even the classic-script bootstrap is blocked before reaching the Promise or
module walls, by two smaller stubbed symbols that Phase 3/4 needs:

- `v8__Context__GetExtrasBindingObject` (null stub) — bootstrap reads the
  built-in console off the extras binding object.
- `v8__Function__SetName` (null stub) — each op function is given its name.

Also stubbed but benign (void setters whose null return is unused, registration
only, not invoked by a sync script): `v8__Isolate__SetMicrotasksPolicy`,
`SetCaptureStackTraceForUncaughtExceptions`, `SetPromiseRejectCallback`,
`SetHostImportModuleDynamicallyCallback`,
`SetHostInitializeImportMetaObjectCallback`. These are safe to leave as no-ops
until their subsystems land (promise-reject / dynamic-import / import-meta), so
they are NOT early walls. `v8__V8__SetFlagsFromString` and
`v8__FunctionTemplate__New` are already real.

Net: the roadmap order is unchanged, but step 3 (op glue) grows two tiny
prerequisites (`GetExtrasBindingObject`, `Function__SetName`) that are cheap and
can land first as a warm-up. The Promise + microtask + module walls remain the
substantive work.

## What deno_core actually needs at boot (traced)

Source: `/Users/divy/gh/deno-v8x-rebase/libs/core`.

### (a) Isolate + context

- `JsRuntime::new` selects `InitMode::New` when `options.startup_snapshot` is
  `None` (`runtime/jsruntime.rs:333`). Boot-from-source is supported; a snapshot
  is NOT required. This is good: the Hermes SnapshotCreator stubs are not a boot
  blocker.
- Isolate is created with `external_references` always built
  (`bindings::create_external_references`, `runtime/jsruntime.rs:971,985`).
- Context creation, embedder-data slots, `Context::Global`: **already real** in
  Hermes (`v8__Context__New/Enter/Exit/Global`,
  `SetAlignedPointerInEmbedderData`). Covered by `boot_baseline` passing.

### (b) Microtask queue

- The event loop drives promise settlement via `scope.perform_microtask_
  checkpoint()` at several points (`runtime/jsruntime.rs:2445,2493,2515`).
- Hermes `v8__Isolate__PerformMicrotaskCheckpoint` (`core.rs:3703`) is an **empty
  no-op**; `v8__MicrotaskQueue__*` and `EnqueueMicrotask` are **null stubs**
  (`shims.rs`). Proven by `boot_microtask_enqueue_and_checkpoint` failing: a
  queued microtask never runs.

### (c) Ops / External

- Each op is bound as a `FunctionTemplate` whose `.data()` is a
  `v8::External::new(scope, op_ctx_ptr)` (`runtime/bindings.rs:995+`,
  `let external = v8::External::new(...)`, `.data(external.into())`).
- `v8__External__New` / `v8__External__Value` and `FunctionCallbackInfo::Data`
  are **already real** in Hermes. So op dispatch is plausible once the module +
  promise plumbing that sits above it works. External is NOT the wall; it is a
  green dependency waiting on the layers above.

### (d) Compile + run the core runtime JS

- The core runtime JS is loaded as **ES modules**, not classic scripts.
  `BUILTIN_ES_MODULES` = `[ext:core/mod.js]` (`runtime/jsruntime.rs:417`), plus a
  synthetic `ext:core/ops` module; `init_extension_js` registers `esm` /
  `lazy_esm` / `synthetic_esm` and then instantiates+evaluates the module graph
  (`runtime/jsruntime.rs:1543+`, `jsrealm.rs:591 instantiate_module`).
- So the module subsystem is REQUIRED to boot, not deferred to user code. This
  is the surprising, load-bearing finding: there is no classic-script-only boot
  of deno_core.

### (e) Instantiate / evaluate ES modules

- `ModuleMap::instantiate_module` -> `Module::instantiate_module(scope,
  resolve_callback)` -> `Module::evaluate`, with synthetic modules via
  `Module::create_synthetic_module` + `set_synthetic_module_export`, and a
  `resolve_callback` for `ext:` specifiers.
- Every `v8__Module__*` and `v8__ScriptCompiler__CompileModule` is a **null
  stub** in Hermes. Proven by `boot_es_module_instantiate_evaluate` failing at
  `compile_module`.

### (f) Event loop (Promises / microtasks)

- `poll_event_loop_inner` resolves pending-op promises and runs microtask
  checkpoints (`runtime/jsruntime.rs:2420+`, `resolve_promise_inner:2278`).
  `PromiseResolver` per pending op; promise state read back.
- All `v8__Promise__*` are **null stubs**. `PromiseResolver::new` returns `None`
  (proven by `boot_promise_resolver_roundtrip`); `get_promise` would then panic
  on `.unwrap()`. No promise-reject callback is registered at plain boot, so the
  reject-callback surface is a later concern.

## Prioritized roadmap to `deno_core JsRuntime` runs a script on Hermes

Ordered by dependency. Each step ends at a probe test that flips to green.

### 1. Promises + microtask queue  (difficulty: medium; unblocks event loop)

Hermes JSI does have real JS Promises (the engine runs `Promise`), so these map
onto JSI object/host-function work rather than new engine internals.

- `v8__Promise__Resolver__New` — `new Promise((res, rej) => ...)` capturing the
  two functions; return the resolver handle (model as an object holding the two
  JSI functions, or the promise + resolve/reject closures).
- `v8__Promise__Resolver__GetPromise`, `__Resolve`, `__Reject`.
- `v8__Promise__State`, `v8__Promise__Result` (read `[[PromiseState]]` /
  `[[PromiseResult]]`; JSI has no direct accessor, so drive via a JS helper that
  attaches a `.then`/`.catch` recording callback, same helper-shim pattern C4/C11
  used for identity and internal fields).
- `v8__Promise__Then/Catch/HasHandler/MarkAsHandled`.
- Microtask queue: make `v8__Isolate__PerformMicrotaskCheckpoint` actually drain
  Hermes's job queue (`hermes::vm::Runtime::drainJobs` via JSI, or run a
  JS-level flush), plus `EnqueueMicrotask` (wrap the given `Local<Function>` into
  a queued job) and the `v8__MicrotaskQueue__*` handles deno_core references.
- Green when `boot_promise_resolver_roundtrip` and
  `boot_microtask_enqueue_and_checkpoint` pass.

Honor C2 (values captured for later resolution must outlive the HandleScope via
Runtime-owned `shared_ptr`, same durable-storage pattern C10/C11/C12 use), C4
(promise/resolver identity), C9 (a rejected promise's value must surface through
the existing TryCatch/exception path, not unwind).

### 2. ES modules  (difficulty: high; the real headline)

- `v8__ScriptCompiler__CompileModule` — compile source text as a module.
  Hermes/JSI has no first-class SourceTextModule the way V8 does; the likely
  route is to model a "module" as a compiled function/record plus recorded
  import requests, and synthesize instantiate/evaluate on top. Investigate
  whether Hermes CommonJS/`hermes::hbc` module support or a manual linker is the
  cheaper path.
- `v8__Module__GetModuleRequests` / `ModuleRequest__GetSpecifier` /
  `GetImportAttributes` — expose the module's import list to deno_core's resolve
  callback.
- `v8__Module__InstantiateModule` — walk requests, call the Rust resolve
  callback, link. `v8__Module__Evaluate` — run, returning a promise for TLA.
- `v8__Module__CreateSyntheticModule` + `SetSyntheticModuleExport` +
  `GetModuleNamespace` — `ext:core/ops` is a synthetic module.
- `v8__Module__GetStatus/GetException/GetIdentityHash/ScriptId/
  IsSourceTextModule/IsSyntheticModule`.
- Green when `boot_es_module_instantiate_evaluate` passes, then when
  `ext:core/mod.js` instantiates + evaluates.

This is the highest-risk step: it is the one V8 subsystem with no clean JSI
analogue. Budget a spike to decide the modeling approach before implementing.

### 3. Op glue end to end  (difficulty: low-medium; mostly already there)

- External (`v8__External__New/Value`) and `FunctionCallbackInfo::Data` are
  already real. Verify op registration (`op_ctx_template`) round-trips through
  the Hermes FunctionTemplate path (C10/C11 callbacks + templates already pass),
  and that `create_external_references` is accepted at isolate creation.
- Add a probe that binds one `External`-backed `FunctionTemplate`, installs it
  on the global, calls it from JS, and reads the External back in the callback.
  Likely green with small fixes once 1-2 land.

### 4. deno_core boot itself  (difficulty: integration)

- Re-attempt harness option (a): give the deno checkout a `hermes` feature alias
  + local path dep, build `deno_core`, run `node tests/harness/run.mjs
  deno_core hermes`. First failures will be link (any remaining stub deno_core
  references) then boot (module graph). Iterate.
- End state: `JsRuntime::new` (no snapshot) + `execute_script`, then
  `mod_evaluate` of a trivial user module, runs on Hermes.

## Files touched this cycle

- `src/hermes/mod.rs`: added `mod hermes_boot_probe` (4 boot-recon tests). No
  backend behavior changed; this is a measurement harness only. `boot_baseline`
  passes; the other three are the documented walls and are expected-red until
  roadmap steps 1-2 land.
- `docs/hermes-spike/experiments/D0-deno-boot-recon.md`: this file.

Baselines were NOT touched: the three probe tests are expected failures by
design (they mark walls), so they must not enter any passing baseline.
