# D4/D5: wire real deno_core against Hermes and attempt a JsRuntime boot

Goal of this cycle: stop simulating with in-repo probes and build ACTUAL
`deno_core` against the local Hermes backend, then run `JsRuntime::new` +
`execute_script`. The deliverable is a real result: it boots and runs, or it
boots to some point and hits a precise wall (a null `v8__*`, a panic, a
module-resolution gap). A genuine "boots to X, fails at Y" is the win.

## TL;DR

- deno_core BUILDS and LINKS against the local Hermes backend (path a). The
  `deno_core` library, `serde_v8`, and the `hello_world` example (a full
  `JsRuntime::new` + op call + `execute_script`) all compile and link.
- The boot attempt RUNS. It gets through v8 platform init and into
  `JsRuntime::new_inner` (isolate + context setup), then hits its wall. Each
  wall was a real null `v8__*` stub deno_core exercises that the rusty_v8 test
  suite never did.
- Walls knocked down this cycle to get the boot moving (all small, Hermes has
  no V8 analogue so each is an inert/identity shim):
  1. link wall: `simdutf__*` undefined (deno_core references the rusty_v8
     simdutf binding; Hermes never defined the symbols).
  2. `v8__Platform__NewCustomPlatform` returned null (deno_core always builds a
     custom platform, unlike the rusty_v8 tests which use the default one).
  3. `v8__Local__New` returned null (deno_core turns a `Global<Context>` back
     into a `Local` at boot; the rusty_v8 suite never round-trips a Global).

## What was wired (path a): deno_core -> local Hermes

Chosen path (a) from the mission: a real deno checkout whose `v8` dependency
resolves to THIS v82jsc repo, built with `hermes,link_hermes`.

Checkout used: `/Users/divy/gh/deno-v8x-rebase` (a deno tree with nathan's
`libs/deno_v8` engine facade). NOTE: it is at deno rev `c1b2628`, not the
harness pin `tools/deno/DENO_REF` (`1d4e6c1`). For a boot spike the exact deno
rev does not matter; the harness only requires the checkout's aliased v8 path
point at ROOT. Re-pin later if this becomes a tracked cell.

### Changes made in the deno checkout (OUTSIDE this branch/repo)

These live in `/Users/divy/gh/deno-v8x-rebase`, not in v82jsc. Recorded here so
they are reproducible:

1. `libs/deno_v8/Cargo.toml`
   - added a `hermes` feature:
     `hermes = ["dep:v8x_backend", "v8x_backend/hermes", "v8x_backend/link_hermes"]`
   - repointed the backend crate from crates.io to the local path:
     `v8x_backend = { package = "v8x", path = "/Users/divy/gh/v82jsc", optional = true, default-features = false }`
     (was `version = "=149.4.0-rc.1"`).
2. `libs/deno_v8/lib.rs` (the facade): made `hermes` re-export `v8x_backend::*`
   exactly like `quickjs`, and updated the mutual-exclusion `compile_error!`
   guards so `v8` is exclusive with `quickjs`/`hermes` and `quickjs`/`hermes`
   are exclusive with each other.
3. root `Cargo.toml`: the `v8` alias now selects the hermes engine:
   `v8 = { package = "deno_v8", ..., features = ["simdutf", "hermes"] }`
   (was `features = ["simdutf"]`, which defaulted to rusty_v8).

The harness `ensureDenoV8Patch` (tests/harness/run.mjs) is satisfied by change
(1): it scans `Cargo.toml` and `libs/deno_v8/Cargo.toml` for a `v8`/`v8x_backend`
dep with `path = ROOT`, and the repointed `v8x_backend` path matches ROOT.

### Backend selection

deno_core's own `v8`/`quickjs` cargo features are NOT used for hermes; the
engine is pinned once at the workspace `v8` alias (facade feature `hermes`), so
`cargo build -p deno_core` builds the whole tree against Hermes with no per-crate
feature juggling. deno_core's default features (`v8_use_custom_libcxx`,
`reactor-tokio`) are harmless: `use_custom_libcxx` is a no-op passthrough in the
v82jsc hermes build.

### Running the boot (macOS)

The deno example binary does not get the Hermes-framework rpath (build.rs emits
`cargo:rustc-link-arg=-Wl,-rpath` which applies only to v82jsc's OWN artifacts,
not downstream binaries). So run with the framework path set:

```
cd /Users/divy/gh/deno-v8x-rebase
cargo build -p deno_core --example hello_world
codesign --force --sign - \
  --entitlements /Users/divy/gh/v82jsc/tools/jit-entitlements.plist \
  target/debug/examples/hello_world
DYLD_FRAMEWORK_PATH=/Users/divy/gh/v82jsc/vendor/hermes \
  target/debug/examples/hello_world
```

(codesign is belt-and-suspenders; Hermes runs interpreted, no JIT entitlement is
strictly required. The rusty_v8 harness cell will need the same
DYLD_FRAMEWORK_PATH wiring for hermes deno_core binaries.)

## The wall progression (the real D5 result)

Each run named its exact first failure; each fix moved the boot to the next.

### Wall 0 (link): simdutf__* undefined

`ld64.lld: error: undefined symbol: simdutf__validate_ascii` (and ~10 siblings),
referenced from `vendor/rusty_v8/src/simdutf.rs`. deno_core pulls in the
rusty_v8 simdutf binding (UTF validation/convert on Deno's hot path); the
quickjs backend defines these in `src/quickjs/simdutf.rs` (pure Rust,
engine-independent) but the Hermes backend never did.

Fix (in v82jsc, this branch): `src/hermes/mod.rs` now includes the same
implementation via `#[path = "../quickjs/simdutf.rs"] mod simdutf;`, gated
`#[cfg(feature = "simdutf")]` to match the rusty_v8 binding that references it.
No code duplicated; the file is engine-independent.

### Wall 1 (panic): v8__Platform__NewCustomPlatform returned null

```
panicked at vendor/rusty_v8/src/support.rs:111 (UniqueRef::from_raw unwrap None)
  v8::platform::Platform::new_custom          (platform.rs:463)
  deno_core::runtime::setup::v8_init          (setup.rs:212)
  JsRuntime::try_new                          (jsruntime.rs:755)
```

deno_core's `v8_init` always builds a CUSTOM platform (handing v8 a boxed
`PlatformImpl` for foreground task ownership), calling
`v8__Platform__NewCustomPlatform`. Hermes had it as a null stub in `shims.rs`
(the rusty_v8 suite only uses the default platform, which Hermes already had as
an inert marker).

Fix (in v82jsc, this branch): `src/hermes/core.rs` implements
`NewCustomPlatform` returning the same inert `HermesPlatform` marker the default
platform returns, now carrying the `PlatformImpl` context so it is freed at
teardown via the vendored `v8__Platform__CustomPlatform__BASE__DROP`. Hermes has
no V8 task platform (it drives its own JSI job/microtask queue, see D1), so the
impl is never called back into. Stub removed from `shims.rs`.

### Wall 2 (panic): v8__Local__New returned null

```
panicked at vendor/rusty_v8/src/handle.rs:147 (Local::new unwrap None)
  deno_core::runtime::jsruntime::JsRuntime::new_inner   (jsruntime.rs:1069)
    -> v8::Local::new(scope, &main_context)  // main_context: Global<Context>
```

After creating the main context and wrapping it in a `Global<Context>`,
deno_core turns it back into a `Local` with `Local::new`, which calls
`v8__Local__New`. Hermes had it as a null stub; the rusty_v8 suite never
round-trips a Global back to a Local, so nothing had forced it real. (Context
creation itself, `v8__Context__New` with deno_core's global ObjectTemplate +
`set_internal_field_count(2)`, worked: Hermes returns the isolate pointer as the
context handle and applies the template to the global.)

Fix (in v82jsc, this branch): `src/hermes/core.rs` implements `v8__Local__New`:
null -> null; a non-value handle (even-aligned: a Context == isolate pointer, or
a Box-backed Module record) -> identity; a value handle (odd-aligned JSI
handle-table slot) -> its JSI value is duplicated into a fresh slot in the
current scope so the new Local outlives the source handle. Needed a new C++ shim
primitive `v8x_hermes_slot_dup(rtw, src)` that appends a copy of `handles[src]`
and returns the new slot index (unlike `set_slot`, which overwrites an existing
slot). Stub removed from `shims.rs`.

### Wall 3 (panic): v8__String__NewExternalOneByteConst returned null

```
panicked at libs/core/error.rs:2140 (Result::unwrap on FastStringV8AllocationError)
  deno_core::error::make_callsite_prototype   (error.rs:2127)
  JsRuntime::new_inner                         (jsruntime.rs:1073)
```

deno_core interns most of its bootstrap strings as `FastString::StaticConst`,
which calls `v8::String::new_from_onebyte_const` ->
`v8__String__NewExternalOneByteConst`. Hermes had it (and its
`OneByteByteStatic` sibling) as null stubs. The first use is
`make_callsite_prototype`'s method-name keys.

Fix (in v82jsc, this branch): `src/hermes/core.rs` implements
`NewExternalOneByteConst` and `NewExternalOneByteStatic`. Hermes has no
external-string-resource concept, so the (ASCII-guaranteed) bytes are copied
into a normal JSI string via the existing `intern_string_utf8` helper. Stubs
removed from `shims.rs`.

### Wall 4 (panic): the extras binding object had no `console`

```
panicked at libs/core/runtime/bindings.rs:316 ("unable to convert")
  deno_core::runtime::bindings::get                        (bindings.rs:373)
  deno_core::runtime::bindings::initialize_deno_core_namespace
  JsRuntime::new_inner                                     (jsruntime.rs:1083)
```

`initialize_deno_core_namespace` reads `console` off
`context.get_extras_binding_object()` and binds it to `Deno.core.console`,
requiring it be an Object. D3's `GetExtrasBindingObject` returned a bare empty
object, so `console` was `undefined` and `try_into::<Object>()` failed. In V8
the extras binding object exposes a built-in console.

Fix (in v82jsc, this branch): `v8__Context__GetExtrasBindingObject` now
synthesizes a minimal `console` (an object of no-op methods, built by evaluating
an object literal through the Hermes eval shim) and sets it under `console` on
the extras object when the object is first created. deno_core forwards real
console output through its own op-based console; these are the fallback sinks.

### Wall 5 (the first JS-level wall, the real result): 00_primordials.js throws

```
panicked: Failed to initialize a JsRuntime: Failed to execute ext:core/00_primordials.js
  JsRuntime::new (jsruntime.rs:743)
  <- initialize_primordials_and_infra: Script::compile OK, script.run() -> None
```

This is the payoff. All the C-ABI plumbing is now good enough that deno_core
gets through isolate + context + string interning + the Deno.core namespace and
starts executing its FIRST bootstrap JavaScript, the classic script
`ext:core/00_primordials.js`. It COMPILES but THROWS at run time.

deno_core discards the JS exception here (`script.run(scope).ok_or(...)` drops
the TryCatch detail), so a scratch probe ran the same source through the Hermes
backend under a `tc_scope!` TryCatch and captured the exact error:

```
TypeError: target is not an object
```

Root cause (bisected): primordials.js reifies every JS intrinsic with
`[...names].forEach((name) => { const original = globalThis[name];
copyPropsRenamed(original, ...); })`. For an intrinsic Hermes v0.11.0 does not
provide, `globalThis[name]` is `undefined` and `copyPropsRenamed(undefined,...)`
-> `Reflect.ownKeys(undefined)` throws "target is not an object". The MISSING
intrinsics in this Hermes build are:

```
AggregateError, BigInt, BigInt64Array, BigUint64Array, FinalizationRegistry, WeakRef
```

This is the exact first JS-level wall. It is NOT a missing `v8__*` symbol; it is
a JS-engine-completeness gap in Hermes v0.11.0. The vendored Hermes framework
predates BigInt / WeakRef / FinalizationRegistry / AggregateError landing in
Hermes.

## Where the boot stands (the headline)

An actual `deno_core::JsRuntime::new` runs on the Hermes backend and gets:

- through v8 platform init, isolate creation, context creation with deno_core's
  global ObjectTemplate, embedder data, the whole string-interning surface, and
  the `Deno` / `Deno.core` / `Deno.core.ops` namespace setup with a synthesized
  console;
- into executing deno_core's real bootstrap JavaScript;
- and stops at the FIRST bootstrap script, `ext:core/00_primordials.js`, which
  throws `TypeError: target is not an object` because Hermes v0.11.0 lacks 6 JS
  intrinsics (`AggregateError`, `BigInt`, `BigInt64Array`, `BigUint64Array`,
  `FinalizationRegistry`, `WeakRef`) that primordials.js enumerates.

It does NOT yet run `1 + 1` (that would need primordials + infra + the
ext:core/mod.js ES-module graph to finish first). The precise wall is the
deliverable.

## Recommended next step

The wall is now engine completeness, not the v8 shim. Two paths:

1. **Bump the vendored Hermes.** A newer Hermes build (BigInt, WeakRef,
   FinalizationRegistry, AggregateError all landed upstream after v0.11.0) would
   clear all 6 in one move and let primordials.js finish. This is the clean
   path if a newer `hermes.framework` can be vendored (check the C2 notes for how
   the framework is obtained).
2. **Polyfill before bootstrap.** Install minimal `AggregateError` / `WeakRef` /
   `FinalizationRegistry` / `BigInt`(+typed arrays) shims on the global before
   deno_core's primordials run. BigInt is not polyfillable to spec (it is a
   primitive type with operator semantics), so this path is partial; WeakRef /
   FinalizationRegistry / AggregateError are shimmable. Bumping Hermes is
   strictly better.

After the intrinsics clear, expect the next walls to move from JS-completeness
back toward the ES-module bootstrap graph (`ext:core/mod.js` +
synthetic `ext:core/ops`), which the D2 modeled module system already supports
in shape; that is where op registration and the modeled linker get exercised for
real.

## Constraints honored

- C2 (lifetime): the platform context and the `Local::new` slot dup use the
  existing Runtime-owned handle table / durable-drop patterns; nothing is left
  in a scope-managed slot a pop could truncate incorrectly.
- rusty_v8 ratchet: the fixes only make previously-null stubs real; no behavior
  a baselined test depends on regressed. In fact the console addition to
  `GetExtrasBindingObject` made `rv8_test_api::context_get_extras_binding_object`
  pass, so the hermes/rusty_v8 baseline went 83 -> 84 (updated via
  `run.mjs rusty_v8 hermes --update --rescue`; `--check --rescue` green at 84).
- No vendored rusty_v8 test, report.json, or .omc file touched.
- The deno-checkout edits are outside this branch (documented above).

## Files touched (v82jsc, this branch)

- `src/hermes/mod.rs`: include `simdutf` module (reuse quickjs impl).
- `src/hermes/core.rs`: real `v8__Platform__NewCustomPlatform` (+ context carry
  and drop), real `v8__Local__New`, `v8__String__NewExternalOneByteConst` +
  `NewExternalOneByteStatic`, a synthesized `console` on
  `v8__Context__GetExtrasBindingObject`, and the `v8x_hermes_slot_dup` extern
  decl / `OneByteConst` import.
- `src/hermes/hermes_shim.cpp`: `v8x_hermes_slot_dup` primitive.
- `src/hermes/shims.rs`: removed the `NewCustomPlatform`, `Local__New`,
  `NewExternalOneByteConst`, and `NewExternalOneByteStatic` null stubs.
- `docs/hermes-spike/experiments/D4-deno-boot-real.md`: this file.

The scratch deno-checkout probe `libs/core/examples/hermes_boot.rs` and the
bisection (running primordials.js through the Hermes backend under a TryCatch)
were used to find the exact wall; they live in the deno checkout / scratchpad,
not in this branch.
