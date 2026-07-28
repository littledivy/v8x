# D6: bump the vendored Hermes, re-run the deno_core boot

Goal of this cycle: D4/D5 booted an actual `deno_core::JsRuntime` on the Hermes
backend up to the first bootstrap script `ext:core/00_primordials.js`, which
threw because the vendored Hermes was an old v0.11.0 build missing 6 JS
intrinsics primordials enumerates (`AggregateError`, `BigInt`, `BigInt64Array`,
`BigUint64Array`, `FinalizationRegistry`, `WeakRef`). The v8 C-ABI shim was
already good enough; the wall was engine version. D6 bumps the vendored Hermes
to a build that has those intrinsics and re-runs the boot to find the next wall.

## TL;DR

- Bumped the vendored Hermes framework twice, landing on Hermes
  **260318099.0.1** (HBC bytecode version **99**), the current `HERMES_VERSION`
  pin on react-native's main branch, published to Maven Central. It is the
  first prebuilt with a macOS-host framework that provides ALL SIX intrinsics
  primordials needs.
- hermesc and the runtime are both HBC version 99, so C5's AOT/HBC path stays
  version-matched (both HBC lib tests pass).
- Re-verify held: all 20 `hermes::` lib tests pass (15 smoke/surface + 5 boot
  probes + 2 HBC, and the older count grew as probes were added). The
  rusty_v8/hermes ratchet went **84 -> 86** (the newer Hermes ships a
  conformant Intl/ICU that made `rv8_test_api::icu_collator` and
  `rv8_test_api::icu_date` pass); no baselined test regressed.
- The deno_core boot now gets PAST the intrinsics wall. primordials.js no longer
  throws on a missing `globalThis[name]`. It now fails one step further, at
  COMPILE time, on a JS syntax feature Hermes does not implement:

```
Compiling JS failed: 285:40: async generators are unsupported
  (ext:core/00_primordials.js)
```

  primordials.js line 285 is `Reflect.getPrototypeOf(async function* () {})`,
  capturing the `%AsyncGenerator%` intrinsic prototype. Hermes supports
  generators, async functions, and `for await`, but not async generator
  functions (`async function*`). This is the exact new wall: a Hermes
  language-feature gap, not a `v8__*` gap and not an intrinsic gap.

## Finding the right prebuilt (the hard part)

The mission asked for the newest facebook/hermes release whose darwin runtime
has the 6 intrinsics. The catch: the JSI-linkable macOS-host runtime framework
is NOT distributed the way the compiler is.

- facebook/hermes GitHub releases ship `hermes-runtime-darwin-vX.tar.gz` (the
  framework) only for **v0.11.0**. v0.12.0 and v0.13.0 ship only
  `hermes-cli-darwin` (hermesc + standalone VM binaries, no linkable dylib).
- The `hermes-engine` npm package stops at 0.11.0.
- The macOS-host framework for newer Hermes lives inside react-native's Hermes
  artifacts on Maven Central, under
  `com/facebook/hermes/hermes-ios/<version>/hermes-ios-<version>-hermes-ios-<debug|release>.tar.gz`.
  Despite the `hermes-ios` name, the tarball's `destroot/Library/Frameworks/`
  contains a real **macosx** host framework (universal arm64+x86_64 dylib),
  the JSI + hermes headers in `destroot/include`, and a matching `hermesc` in
  `destroot/bin`, exactly the layout C2 vendored from the v0.11.0 asset.

Intrinsic coverage measured directly against each build's JSI runtime (not the
CLI VM), with the microtask queue enabled where relevant:

| build | HBC | BigInt | AggregateError | BigInt64/Uint64Array | WeakRef | FinalizationRegistry |
|---|---|---|---|---|---|---|
| v0.11.0 (old vendor) | 84 | no | no | no | no | no |
| v0.12.0 | 89 | yes | yes | yes | no | no |
| v0.13.0 | 96 | yes | yes | yes | no | no |
| RN 0.75.4 | 96 | yes | yes | yes | yes (needs microtask queue) | no |
| 260318099.0.1 | 99 | yes | yes | yes | yes | yes |

Two upstream facts, confirmed against real commits:

1. `WeakRef` exists in Hermes but is inert unless the runtime is built with
   `RuntimeConfig::Builder().withMicrotaskQueue(true)`; without it, `WeakRef` is
   absent from `globalThis`. This is why the default-config probe on RN 0.75.4
   reported `WeakRef` undefined.
2. `FinalizationRegistry` did not exist in Hermes at all until it landed in
   March 2026 (facebook/hermes commit `4346ed50...` "Add JSFinalizationRegistry
   class" + `b2c6c9ae...` "Wire JSFinalizationRegistry to JSLib"). It is newer
   than every GitHub-tagged Hermes release and newer than RN 0.75.4/0.86. So a
   pure bump to any RN-tagged Hermes clears only 5 of the 6; only the
   `260318099.0.1` main-branch build clears all 6.

So the bump went in two steps, both recorded as commits: first to RN 0.75.4
(cleared 4, then 5 with the microtask queue), then to 260318099.0.1 (all 6).

## What changed in the backend

### Vendored artifacts (`vendor/hermes/`)

- `hermes.framework` -> `hermesvm.framework`. The 2026 prebuilt renamed the
  framework and its dylib install name (`@rpath/hermesvm.framework/...`), so the
  link name changed from `hermes` to `hermesvm`.
- `include/` replaced with the 260318099.0.1 JSI + hermes headers.
- `bin/hermesc` replaced with the matching HBC-v99 compiler.
- `HERMES_VERSION` stamp updated.

### build.rs

`build_hermes` now auto-detects `hermesvm.framework` vs `hermes.framework` and
emits the corresponding `-framework` link name, so both the older and newer
prebuilts link without further edits.

### src/hermes/hermes_shim.cpp

Two changes, both required by the newer Hermes:

1. `v8x_hermes_runtime_new` now builds the runtime with an explicit
   `RuntimeConfig`: a named `GCConfig` and `withMicrotaskQueue(true)`.
   - The microtask queue is needed for `WeakRef` (and `FinalizationRegistry`)
     to appear on `globalThis`.
   - The named GCConfig fixes a crash. The RN 0.75.4+ default GC is HadesGC.
     Constructing it with the default (empty) heap name null-dereferences
     inside `HadesGC::HadesGC` (crash-manager custom-data registration) under
     the deno_core process, causing a SIGSEGV in `makeHermesRuntime` before any
     JS runs. It did not crash in the isolated smoke tests, only under the full
     deno binary. Giving the heap a non-empty name avoids the null deref.
2. `v8x_hermes_is_hbc` uses the new root-API path. The 2026 refactor moved
   `isHermesBytecode` from a static method on `HermesRuntime` to a virtual on
   `IHermesRootAPI`, reached via `makeHermesRootAPI()->castInterface(...)`.

The rest of the ~3000-line JSI shim recompiled clean against the 2026 headers
with no source changes, despite ~1100 lines of jsi.h churn. That source
compatibility is the main reason the bump was tractable.

## The boot progression (the D6 result)

Same `hermes_boot` example as D4 (`libs/core/examples/hermes_boot.rs` in the
deno checkout): `JsRuntime::try_new(RuntimeOptions::default())` then
`execute_script("1 + 1")`. Run with `DYLD_FRAMEWORK_PATH=vendor/hermes`.

- With the old v0.11.0 framework (re-confirmed this cycle): boots to
  primordials.js, which THROWS `TypeError: target is not an object` on the first
  missing intrinsic.
- With the RN 0.75.4 framework, default GC config: SIGSEGV in
  `HadesGC::HadesGC` inside `makeHermesRuntime`, before any JS. Fixed by the
  named GCConfig (above).
- With the 260318099.0.1 framework + the shim's microtask/named-GC config: the
  boot gets PAST the intrinsics. primordials.js no longer throws at run time.
  It now fails one step earlier in its own lifecycle, at COMPILE time:

```
Failed to execute ext:core/00_primordials.js
  <- Hermes compiler: 285:40: async generators are unsupported
```

deno_core discards the exception detail at the boot site, so primordials.js was
run through the vendored 260 Hermes runtime directly, under a TryCatch, to
capture the exact message. Isolated syntax probe against the same runtime:

```
generator:        OK
async fn:         OK
async generator:  Compiling JS failed: async generators are unsupported
for-await:        OK
```

## Where the boot stands (the new wall)

An actual `deno_core::JsRuntime::new` runs on the bumped Hermes backend and now:

- gets through v8 platform init, isolate + context creation, string interning,
  the Deno.core namespace (all as in D4);
- gets through the intrinsics enumeration in primordials.js that stopped D4
  (all 6 of `AggregateError`, `BigInt`, `BigInt64Array`, `BigUint64Array`,
  `FinalizationRegistry`, `WeakRef` are present);
- and now stops when the Hermes compiler REJECTS primordials.js at parse time
  because it contains one `async function* () {}` (line 285), a JS syntax
  feature Hermes does not implement.

It still does not run `1 + 1` (primordials + infra + the ext:core/mod.js module
graph must finish first). The wall moved from "missing runtime intrinsics" (D4)
to "unsupported source-language feature" (D6). It is neither a `v8__*` gap nor a
module-resolution gap. It is a hard Hermes compiler limitation.

## Recommended next step

The async-generator gap is a single construct in primordials.js used only to
capture the `%AsyncGenerator%` prototype. Options, roughly in order of leverage:

1. **Source-transform the boot script.** deno_core lets the embedder control the
   bootstrap source; rewriting `Reflect.getPrototypeOf(async function* () {})`
   to a Hermes-compatible way of obtaining the `%AsyncGenerator%` intrinsic (or
   stubbing that one entry when Hermes cannot represent it) would clear this
   wall without an engine change. This is the Hermes analogue of D2's module
   source transform: model a feature Hermes lacks in the layer above it.
2. **Check whether async generators are behind a Hermes flag or a newer build.**
   This cycle's 260318099.0.1 rejects them at compile time with no obvious flag;
   confirm against an even newer main build before assuming it is permanent.
3. If the boot script is made Hermes-compatible, expect the next walls to move
   into primordials' remaining intrinsic captures and then toward the
   ext:core/mod.js + synthetic ext:core/ops module graph (the D2 modeled module
   system), where op registration gets exercised for real.

## Verification (this cycle)

- `cargo test --no-default-features --features hermes,link_hermes --lib hermes:: -- --test-threads=1`
  -> 20 passed, 0 failed (15 smoke/surface, 5 boot probes, 2 HBC).
- `node tests/harness/run.mjs rusty_v8 hermes --check --rescue` -> flagged 2 new
  passes; `--update --rescue` -> baseline 84 -> 86, no regression
  (pass=86 = 84 prior + icu_collator + icu_date).
- hermesc `--version` HBC 99 == runtime `getBytecodeVersion()` 99; both HBC lib
  tests (`hermes_hbc_runs_through_backend`, `hermes_hbc_parse_free_win`) pass,
  confirming compiler/runtime bytecode-version match.

## Constraints honored

- Local branch `hermes-backend-spike` only; no push, no publish.
- Committed often: framework bump (RN 0.75.4), ratchet update, shim GCConfig +
  microtask fix, and the final 260318099.0.1 bump are separate commits so a
  crash cannot lose the work.
- No vendored rusty_v8 test, `report.json`, `history.jsonl`, or `.omc/` file
  touched.
- Deno-checkout edits are OUTSIDE this branch. None were added this cycle: the
  `hermes` facade wiring and `hermes_boot.rs` example already existed from
  D4/D5; the boot was only rebuilt against the bumped framework
  (`DYLD_FRAMEWORK_PATH=vendor/hermes`).
- Disk managed: intermediate tarballs and extractions removed after vendoring;
  the vendored framework is 21 MB (up from 4.5 MB, the newer dylib is larger).

## Files touched (v82jsc, this branch)

- `vendor/hermes/`: framework `hermes.framework` -> `hermesvm.framework`,
  `include/` (JSI + hermes headers), `bin/hermesc`, `HERMES_VERSION`, all
  bumped to Hermes 260318099.0.1 (HBC 99).
- `build.rs`: auto-detect `hermesvm.framework` vs `hermes.framework` link name.
- `src/hermes/hermes_shim.cpp`: named GCConfig + microtask-queue RuntimeConfig
  in `v8x_hermes_runtime_new`; root-API `isHermesBytecode` in `v8x_hermes_is_hbc`.
- `tests/status/baselines/hermes/rusty_v8.txt`: 84 -> 86 (icu_collator, icu_date).
- `docs/hermes-spike/experiments/D6-hermes-bump.md`: this file.

The scratch probes (JSI intrinsic checks, the primordials.js TryCatch run, the
async-generator syntax probe) live in the session scratchpad, not this branch.
