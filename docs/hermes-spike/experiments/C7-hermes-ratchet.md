# C7: first rusty_v8 tests pass on Hermes (ratchet hill-climb)

**Result: 10 of 16 rusty_v8 tests now pass on the Hermes backend, up from
0/16.** 14 of the 16 targets link (unchanged from C6); the 10 passes come from
implementing isolate/context slots, embedder data, `External`, a few `Value`
predicates/coercions, the platform constructors, `--use_strict`, and a
non-firing persistent/weak handle path. The baseline
`tests/status/baselines/hermes/rusty_v8.txt` is updated and `--check` holds.

## The 10 passing tests

```
rv8_slots::context_slots
rv8_slots::dropped_context_slots_on_kept_context
rv8_slots::slots_auto_boxing
rv8_slots::slots_general_1
rv8_slots::slots_general_2
rv8_slots::slots_layer1
rv8_slots::slots_layer2
rv8_test_api_flags::set_flags_from_string
rv8_test_simple_external::test_simple_external::test
rv8_test_single_threaded_default_platform::single_threaded_default_platform
```

Run: `node tests/harness/run.mjs rusty_v8 hermes` reports
`pass=10 fail=6 skip=0 (total 16)`. `--check` prints
`OK: ratchet holds (10 baselined)`.

## v8__* machinery implemented (all in src/hermes/core.rs + hermes_shim.cpp)

The slots system is almost pure Rust in the vendored crate (a `HashMap` in an
isolate/context annex), so most of the win was making the surrounding lifecycle
symbols real instead of `unimplemented!()` stubs.

Isolate/lifecycle:
- `v8__Isolate__PerformMicrotaskCheckpoint` (no-op: Hermes drains promise jobs
  inside `evaluateJavaScript` and exposes no separate embedder drain through
  JSI; the two `slots` Deno-pattern tests only call it to prove `Isolate`
  methods are reachable via `Deref`, not to observe queued microtasks).
- `v8__V8__SetFlagsFromString` (accepts and ignores V8 flags, except it honors
  `--use_strict`, see below).
- `v8__Platform__NewUnprotectedDefaultPlatform`,
  `v8__Platform__NewSingleThreadedDefaultPlatform` (both return the same inert
  platform marker as the existing default; Hermes has no V8 platform).

Context slots + embedder data (one context per Hermes isolate, so the fields
live on `IsoState`):
- `v8__Context__GetNumberOfEmbedderDataFields`,
  `v8__Context__GetAlignedPointerFromEmbedderData`,
  `v8__Context__SetAlignedPointerInEmbedderData` (a grow-on-demand
  `Vec<*mut c_void>` on the isolate state; `Context::set_slot` stores its annex
  pointer in field 0).

Persistent/weak handles:
- `v8__Global__New` (a `Global` is the same handle-table slot pointer carried
  unchanged: our table entries already outlive their handle scope, and
  `Global::new` copies the data pointer out, so the value stays reachable for
  the isolate's life, which is what the kept-context test needs).
- `v8__Global__NewWeak` returns the same data pointer as a non-firing weak (JSI
  exposes no embedder weak-callback hook; the value stays strongly reachable
  and the finalizer never runs). `v8__Global__Reset` is a safe no-op. This is a
  conservative over-retention (a leak, never a use-after-free), correct for the
  tests that pass and the honest reason `dropped_context_slots` (which needs a
  real GC reclaim) does not.

External (opaque embedder `void*`):
- `v8__External__New` / `v8__External__Value` model a v8 `External` as a JSI
  `HostObject` (new `ExternalHost` class in the shim) carrying the pointer.
  Each External is a distinct JS object, so two externals compare unequal by
  object identity and the pointer round-trips exactly.
- `v8__Data__EQ` routes to `jsi::Value::strictEquals` (the same identity path
  as `v8__Value__StrictEquals`); the vendored `External` `PartialEq` uses it
  (`use identity`), so `ex1 != ex2` and `ex1 == ex1` hold.

Value predicates/coercions:
- `v8__Value__IsUndefined` (`jsi::Value::isUndefined`), `v8__Value__IsExternal`
  (is the handle an `ExternalHost`), `v8__Value__Uint32Value` (ECMAScript
  ToUint32 of a number, written into the out-param `Maybe<u32>`; used by the
  custom-platform test's `1 + 2 == 3` check, which is also otherwise blocked).

`--use_strict`:
- V8 applies `--use_strict` by making every top-level script strict. Hermes has
  no such flag and runs top-level code non-strict, so `(function(){return
  this})()` returned the global object instead of `undefined`.
  `v8__Script__Compile` now prepends a `"use strict";` directive when the
  process-global `USE_STRICT` flag (set by `SetFlagsFromString`) is on. Each
  rusty_v8 test target is its own process, so this global is scoped to the one
  target that sets it.

All shim entry points keep the C2 catch-all (no C++/`jsi::JSError` unwinds
across `extern "C"`). `tools/gen_hermes_shims.sh` was re-run after each core.rs
change; it detects the newly-implemented symbols and drops their stubs (no
hand-gating needed), and is verified idempotent (byte-identical shims.rs on a
second run).

## The 6 remaining failures and why

- `rv8_test_api_entropy_source::set_entropy_source` — asserts three isolates
  each produce the SAME `Math.random()` because the test installs a fixed
  entropy source. Hermes seeds its own PRNG and never calls a V8
  `SetEntropySource` callback, so the three values differ and the dedup
  assertion fails. Would need Hermes to route its RNG seed through an embedder
  hook, which JSI does not expose. Skipped.
- `rv8_test_custom_platform::custom_platform_foreground_task_ownership` and
  `rv8_test_platform_atomics_pump_message_loop::atomics_pump_message_loop` —
  both drive `Atomics.waitAsync` on a `SharedArrayBuffer` and expect the async
  wait to post a foreground task to the platform (and, for the pump test,
  `%AtomicsNumWaitersForTesting` natives syntax + a real message loop). Hermes
  has no `SharedArrayBuffer`/`Atomics.waitAsync` async-wait machinery wired to
  an embedder platform, and no `%`-prefixed natives syntax. Large subsystem;
  skipped.
- `rv8_test_external_deserialize::external_deserialize` — creates a snapshot
  blob with external references and deserializes it into two isolates. Needs
  the `SnapshotCreator` + `create_blob` + startup-blob-deserialize subsystem
  (none of which Hermes exposes through JSI). Skipped.
- `rv8_slots::dropped_context_slots` — sets a context slot holding a
  `DropMarker`, calls `gc()`, and asserts the marker dropped. Requires a real
  GC weak-callback reclaim of the context (our weak handles never fire, by
  design above). Skipped; the sibling `dropped_context_slots_on_kept_context`
  (which keeps a `Global` and drops the isolate) passes because it does not
  depend on GC timing.

`rv8_test_api` and `rv8_test_cppgc` still do not link (unchanged from C6):
missing ICU trio (`icu_get_default_locale`, `icu_set_default_locale`,
`udata_setCommonData_77`) plus, for `test_api`, the 11 TypedArray constructors.

## Recommended next targets

1. **ICU trio** — smallest, highest-leverage unlock. Both non-linking targets
   need only this to LINK; `test_api` would then surface hundreds of individual
   test outcomes instead of 0. Define a real-or-consistent-noop
   `icu_get_default_locale`/`icu_set_default_locale`/`udata_setCommonData_77`
   trio (unrelated to JSI).
2. **TypedArrays** (`v8__*Array__New` family) — fully unlocks `test_api`'s link,
   following the same `Object`/`Array` handle-slot pattern C6 established.
3. **TryCatch / exception surfacing** — `Script::Run` currently swallows a
   thrown JS error as a null `Local`; materializing the `jsi::JSError` as a
   `v8::Value` would let `TryCatch`-based tests inspect the thrown value.

## No regressions

- Internal hermes smoke tests: 12/12 still pass
  (`cargo test --no-default-features --features hermes,link_hermes --lib
  hermes::`).
- Stub-hermes build (`--features hermes`): compiles clean.
- QuickJS build (`--features quickjs`): compiles clean.
- `tests/status/baselines/hermes/rusty_v8.txt` updated 0 -> 10; `--check` holds.
