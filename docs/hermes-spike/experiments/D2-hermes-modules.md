# D2: ES modules on the Hermes backend

Goal of this cycle: knock down the last Deno-boot wall D0 found, ES modules, so
the `boot_es_module_instantiate_evaluate` probe goes green and the source-module
path deno_core boots from starts to work. This is the high-risk modeling cycle:
JSI/Hermes has no ES-module-record API at all.

## TL;DR

- The target boot probe is GREEN:
  `boot_es_module_instantiate_evaluate` now passes.
  `cargo test --no-default-features --features hermes,link_hermes --lib
  hermes_boot_probe` reports 4 passed / 0 failed. All three earlier walls
  (promise, microtask, module) are down.
- rusty_v8 on hermes went from 81 to 83 passing (of 267). The two new passes are
  both ES-module tests in `test_api.rs`:
  - `script_compiler_source` (compile a module from a `Source` + `ScriptOrigin`)
  - `module_evaluation` (compile, instantiate with a resolve callback that
    compiles the specifier text as a nested module, evaluate the whole graph,
    observe the transitive side effects on the global)
- No regressions: the 15 hermes smoke tests, all 4 boot probes, and every
  previously-baselined rusty_v8 test still pass. `--check --rescue` is green at
  83 baselined.

## The problem

Hermes' `evaluateJavaScript` runs a single classic script. A top-level
`import`/`export` is a syntax error, and JSI exposes no
compile-module / instantiate-with-resolve-callback / evaluate primitives. So the
v8 module semantics are MODELED in Rust on top of the JSI object / eval /
function-call primitives that already exist.

## The modeling approach

A module is compiled to a JS closure of the shape

```text
(function (__imports, __exports) { <rewritten module body> })
```

- Each `import ... from "spec"` is stripped from the body and re-expressed as a
  prologue that reads bindings off `__imports["spec"]`.
- Each `export ...` is rewritten to assign onto `__exports`.

Instantiation walks the module's parsed import requests, invokes the embedder's
resolve callback to get each dependency Module, recursively instantiates it, and
records the dependency's namespace object. Evaluation evaluates each dependency
first (depth-first, matching V8), then builds the `__imports` map from the
dependency namespaces and runs the closure with `(__imports, __exports)`.

### Module handles are Rust records, not JSI slots

A `Module` / `ModuleRequest` / `FixedArray` is a Rust-owned `Box` record whose
raw pointer IS the v8 `Local` handle. These records never enter the JSI handle
table: an even-aligned `Box` pointer reads as the null slot under `slot_of`, and
they never pass through it. They are freed at isolate `Dispose` via a per-isolate
`module_records` drop list (`IsoState`).

JS values a record must hold across HandleScope pops (the exports/namespace
object, and transitively the compiled closure at evaluate time) are parked in
runtime-owned durable pins (`v8x_hermes_pin` / `pin_get` / `unpin`, a new
`std::vector<std::unique_ptr<jsi::Value>>` on the RuntimeWrapper), honoring the
C2 lifetime rule the same way the promise infra (D1) and callback data (C10) do.

### Synthetic modules

A synthetic module (how deno_core's `ext:core/ops` is provided) is an exports
object populated by native evaluation steps. `CreateSyntheticModule` stores the
export-name list and the `SyntheticModuleEvaluationSteps` fn pointer;
instantiate creates the (empty) exports object; evaluate runs the steps, which
call `SetSyntheticModuleExport` to write each named export onto that object.

### Promise from Evaluate

V8 module evaluation returns a Promise. `Evaluate` builds a resolved promise
through the D1 promise infra (`resolver_new` + `resolve(undefined)` +
`get_promise`) and returns it.

## Handled import/export forms

The transform is a focused, line-oriented source-to-source rewrite. The forms
handled (the ones the module rusty_v8 tests and the deno_core boot graph use):

Imports (rewritten to a `const ... = __imports["spec"]...;` prologue):

- `import "spec";` (side-effect only; records the request, emits nothing)
- `import { a, b as c } from "spec";`
- `import defaultName from "spec";`
- `import defaultName, { a } from "spec";`
- `import * as ns from "spec";`

Exports (rewritten to `__exports.name = ...;`):

- `export const x = ...;` / `export let` / `export var`
- `export function f(){}` / `export async function f(){}` / `export class C{}`
  (declaration kept verbatim, then published)
- `export default <expr>;`
- `export { a, b as c };`

Specifier extraction handles both single and double quotes.

## Symbols made real (were null stubs)

- `ScriptCompiler::CompileModule`, `Source::CONSTRUCT/DESTRUCT/GetCachedData`
- `ScriptOrigin::CONSTRUCT/ResourceName/SourceMapUrl/ScriptId` (our own 40-byte
  layout, since we implement CONSTRUCT: only the source string, resource name,
  source-map url, and is-module flag are stored)
- `Module::GetStatus/GetIdentityHash/ScriptId/IsSourceTextModule/
  IsSyntheticModule/IsGraphAsync/GetException/GetModuleRequests/
  GetModuleNamespace(+phase)/InstantiateModule/Evaluate/EvaluateForImportDefer/
  CreateSyntheticModule/SetSyntheticModuleExport/GetUnboundModuleScript/
  GetStalledTopLevelAwaitMessage/SourceOffsetToLocation`
- `ModuleRequest::GetSpecifier/GetPhase/GetSourceOffset/GetImportAttributes`
- `FixedArray::Length/Get`, `Location::GetLineNumber/GetColumnNumber`

New C++ shim primitives: `v8x_hermes_pin`, `v8x_hermes_pin_get`,
`v8x_hermes_unpin` (durable JS-value pins).

## Constraints honored

- C2 (lifetime): module records live in a per-isolate Rust list freed at
  Dispose; the JS values they carry live in runtime-owned pins that are torn
  down before the runtime. Nothing is left in a scope-managed handle slot that a
  scope pop could truncate.
- C4 (identity): a module's namespace/exports object is one pinned JS object, so
  reading the namespace through two Locals yields the same object; module
  identity is the record pointer itself, and `GetIdentityHash` returns a stable
  per-record value.
- C9 (exceptions): a throwing module body or resolve callback surfaces through
  the existing TryCatch/JSError path (the closure eval and the callback both run
  through the C9-aware `v8x_hermes_run` / callback trampoline); Evaluate returns
  null and marks the module Errored rather than unwinding across `extern "C"`.
- D1 (promises): Evaluate returns a real resolved promise built with the D1
  infra.

## How close to the deno_core boot graph

The deno_core boot graph is: a source module `ext:core/mod.js` that imports named
bindings from a synthetic module `ext:core/ops`. D2 now supports exactly that
shape end to end in the modeled linker:

- a source module compiles, instantiates with a resolve callback, and evaluates;
- its dependencies (including a synthetic module) are resolved, instantiated,
  and evaluated first;
- named bindings from a synthetic module are read through `__imports["spec"].name`
  after `SetSyntheticModuleExport` populates that module's namespace.

`module_evaluation` passing is the concrete proof of the source-module +
resolve-callback + transitive-evaluation path. The synthetic path is exercised
by the `CreateSyntheticModule`/`SetSyntheticModuleExport`/eval-steps code but is
not yet covered by a green rusty_v8 test on its own.

## Known gaps (documented, not faked)

- **Re-exports** `export { x } from "spec";` and `export * from "spec";` do NOT
  register a module request. This is why `module_instantiation_failures1` still
  fails at `assert_eq!(2, module_requests.length())` (its second line is
  `export {} from './bar.js';`). deno_core's core modules do not use this form
  at the boot layer, so it is deferred.
- **Source offsets / Location** are stubbed to 0. `SourceOffsetToLocation` and
  `ModuleRequest::GetSourceOffset` return 0, so tests asserting exact
  line/column of an import (again `module_instantiation_failures1`) fail. This
  needs the transform to track byte offsets per request, a larger lift with no
  boot-path payoff.
- **Top-level await** is not modeled: `is_graph_async()` is always false and
  `GetStalledTopLevelAwaitMessage` returns empty, so
  `module_stalled_top_level_await` fails. The boot graph has no TLA.
- **Instantiation-failure status reset**: on a resolve callback throwing, we set
  the module Errored rather than leaving it Uninstantiated as V8 does on a
  linking failure. Tied to the same failing test above.
- The transform is line-oriented, so an import/export split across multiple
  lines, or multiple statements on one line, is not handled. The boot modules
  and the passing tests use one statement per line.

## The remaining walls to actually boot deno_core

With promises (D1) and modules (D2) down, the in-repo boot probe is fully green.
The next steps toward a real deno_core boot are integration, not new v8
subsystems:

1. **D3: op glue end to end.** External + FunctionCallbackInfo::Data are already
   real (D0). Add a probe that binds one External-backed FunctionTemplate,
   installs it, calls it, and reads the External back, plus the two tiny
   bootstrap prerequisites D0 flagged (`Context::GetExtrasBindingObject`,
   `Function::SetName`).
2. **Attempt the deno_core harness cell.** Give the deno checkout a `hermes`
   feature alias + local path dep, build `deno_core`, run
   `node tests/harness/run.mjs deno_core hermes`. First failures will be any
   remaining link stubs, then the real boot module graph. The re-export and
   source-offset gaps above may or may not bite depending on the exact shape of
   the generated `ext:core/mod.js`; if they do, close them then.

## Files touched

- `src/hermes/modules.rs`: new. The whole ES-module model (compile transform,
  instantiate, evaluate, synthetic modules, ModuleRequest / FixedArray / Source
  / ScriptOrigin, Location).
- `src/hermes/core.rs`: `module_records` drop list on `IsoState` (freed at
  Dispose); `slot_ptr`/`slot_of`/`current_rtw`/`read_string_slot` made
  `pub(super)` for the sibling module.
- `src/hermes/hermes_shim.cpp`: durable JS-value pins (`pins` vector +
  `v8x_hermes_pin`/`pin_get`/`unpin`).
- `src/hermes/shims.rs`: removed the ~30 now-real module/ScriptCompiler/
  ScriptOrigin/FixedArray/Location stubs.
- `src/hermes/mod.rs`: `mod modules;` (gated `link_hermes`).
- `tests/status/baselines/hermes/rusty_v8.txt`: 81 to 83 passing.
