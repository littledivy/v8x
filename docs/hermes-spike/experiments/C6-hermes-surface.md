# C6: widen the Hermes surface (Object/Array/Number/Function) and register the rusty_v8 baseline

**Part 1 result: YES.** Objects, arrays, numbers, integers, booleans, and
calling a JS function value all now work end to end through the v8x Rust
surface on real libhermes. Six new smoke tests drive `v8::Object`,
`v8::Array`, `v8::Number`, `v8::Integer`, `v8::Boolean`, and
`v8::Function::call` (via `crate as v8`, the same vendored rusty_v8 Rust API
the other backends use), and all pass.

**Part 2 result: harness integration works; baseline is honestly 0/16
passing today.** Hermes is registered as a 4th backend in
`tests/harness/config.json` with empty baselines. `node
tests/harness/run.mjs rusty_v8 hermes` runs cleanly end to end (builds each
`rv8_test_*` target individually, same as the other 3 backends) and
`--check` holds against the empty baseline. 14 of 16 test targets LINK (0
passing each, since the surface implemented so far doesn't cover what those
tests exercise); 2 do not link (`rv8_test_api`, `rv8_test_cppgc`), both on
genuinely undeclared ICU symbols plus (`rv8_test_api` only) the TypedArray
constructor family, which C6 did not implement.

`gen_hermes_shims.sh` is fixed: it now preserves hand-added
`#[cfg(not(feature = "link_hermes"))]` gates across regeneration (previously
it silently dropped them, breaking the stub build's link step) and no longer
needs a 14-symbol hand-appended block at the bottom of `shims.rs` (a second,
related bug: it was missing an entire vendored source file and
mis-truncating a class of symbol names).

## Test command and result

```bash
cargo test --no-default-features --features hermes,link_hermes \
  --lib hermes:: -- --nocapture
```

```
test hermes::hello_world::hermes_backend_runs_hello_world ... ok
test hermes::hermes_identity::hermes_identity ... ok
test hermes::hermes_hbc::hermes_hbc_runs_through_backend ... ok
test hermes::hermes_hbc::hermes_hbc_parse_free_win ... ok
test hermes::tests::hermes_smoke_eval_40_plus_2 ... ok
test hermes::tests::hermes_smoke_catches_js_error ... ok
test hermes::hermes_surface::hermes_object_new_get_set ... ok
test hermes::hermes_surface::hermes_array_new_length_indexed_get_set ... ok
test hermes::hermes_surface::hermes_nested_object ... ok
test hermes::hermes_surface::hermes_number_integer_boolean_roundtrip ... ok
test hermes::hermes_surface::hermes_function_call ... ok
test hermes::hermes_surface::hermes_json_stringify_of_native_built_value ... ok

test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 13 filtered out
```

All 6 pre-existing C2/C3/C4/C5 tests stay green; the 6 new `hermes_surface`
tests are the C6 proof. Verified stable under both the default parallel
runner and `--test-threads=1`.

## Part 1: what's real now

### v8__* symbols made real

`src/hermes/hermes_shim.cpp` (new C++/JSI entry points, each wrapped in the
existing C2 catch-all so no C++ exception crosses into Rust) plus
`src/hermes/core.rs` (the v8 C-ABI side, replacing the auto-generated stub):

- **Object**: `v8__Object__New`, `v8__Object__Get`, `v8__Object__Set`,
  `v8__Object__Has`. Property keys are a generic `Value` in the v8 C-ABI
  (not just a `Name`), but JSI's `Object::getProperty`/`setProperty`/
  `hasProperty` are string-/`PropNameID`-keyed only; the key is coerced to a
  JS string via `Value::toString` before the JSI call, matching how v8
  itself coerces non-`Name` property keys.
- **Array**: `v8__Array__New` (`jsi::Array(runtime, length)`),
  `v8__Array__Length` (`Object::isArray` + `Array::size`), and indexed
  access reuses the vendored `v8__Object__GetIndex`/`SetIndex` symbols
  (`Array::getValueAtIndex`/`setValueAtIndex`), since v8's own `Array` type
  has no separate indexed-get/set C-ABI pair. Indexing goes through
  `Object`.
- **Number/Integer/Boolean**: `v8__Number__New`/`Value`
  (`jsi::Value(double)` / `Value::getNumber`), `v8__Integer__New`/
  `NewFromUnsigned`/`Value` (routed through the Number path: Hermes/JSI has
  no separate integer representation, so `Integer` is `Number` under a v8
  API name, same as the numeric tower in real V8's SMI/HeapNumber split
  collapses at the JSI boundary), `v8__Boolean__New`
  (`jsi::Value(bool)`), and `v8__Value__BooleanValue` (`Value::getBool`,
  used for reading a `Boolean` back. The vendored surface has no direct
  `Boolean::value()`, only the JS-truthiness-flavored `Value::BooleanValue`).
- **Function**: `v8__Function__Call`, tractable and implemented.
  `jsi::Function::call`/`callWithThis` (chosen based on whether the receiver
  slot is present) marshal a Rust `argv` array of tagged pointers into a
  `std::vector<jsi::Value>` on the C++ side and call through JSI.
- **Value predicates**: `v8__Value__IsArray`, `IsFunction`, `IsNumber`,
  `IsBoolean`, `IsString` (the last one already existed as a private helper
  from C3's `EscapeSlot__escape`; now also wired to the real symbol name).
  `IsArray`/`IsFunction` route through `Object::isArray`/`isFunction`
  (JSI exposes these only on `Object`, not on a bare `Value`), so the shim
  checks `isObject()` first.
- **`v8__Undefined`/`v8__Null`**: needed as `Function::Call`'s receiver for
  an `undefined` `this`. `jsi::Value::undefined()`/`null()` are static
  factories (no `Runtime` call), just pushed into the handle table.

Every new op follows the same shape C3/C4 established: a v8 `Local` is a
C++ handle-table slot index, and each Rust entry point is a thin wrapper
around one `v8x_hermes_*` shim call.

### JS features that now work through the backend

Objects (create/read/write/has, including nested objects), arrays (create,
length, indexed read/write), numbers/integers/booleans (create + read
back), and calling a JS function value with arguments and getting its
return value. Every new test cross-checks the C++-side-built value with a
real JS script (`JSON.stringify`, `Array.isArray`, `.reduce`) run through
the existing `Script::compile`/`run` path, not just our own read-back.
This proves the writes are real JS heap state indistinguishable from a
JS-authored object.

### What's explicitly NOT done (scope boundary, not an oversight)

TypedArrays (`Int8Array`/`Float64Array`/etc., 11 `v8__*Array__New`
symbols, confirmed still-undefined by the `rv8_test_api` link failure
below), `ArrayBuffer` read/write, `Map`/`Set`, `Promise`, `Proxy`,
`RegExp`, exceptions/`TryCatch` (`v8__TryCatch__CONSTRUCT` is still an
unimplemented stub, and JS errors from `Script::Run` currently just return a
null `Local` with no exception object surfaced), and the inspector/cppgc
subsystems C3's doc already flagged as the `test_api`/`test_cppgc`
link-unlock target. All out of scope for this mission; each is a clear,
scoped follow-up.

## Part 2: the ratchet

### `gen_hermes_shims.sh` fix

Two real, related bugs, both root-caused and fixed (not routed around):

1. **Gate-dropping (the bug C4 flagged).** The generator's stub-vs-real
   exclusion logic (`/tmp/hermes_implemented.txt`) correctly decides
   whether a symbol needs a stub at all, but had no notion of the
   `#[cfg(not(feature = "link_hermes"))]` gate a symbol needs when it is
   real (compiled only under `link_hermes`) while its `unimplemented!()`
   stub must still exist for the plain stub build. Every previous cycle
   that implemented a new real symbol had to hand-add that gate to the
   regenerated file, and a blind re-run would silently drop every existing
   one. Fixed by treating the CURRENTLY gated symbol set in the checked-in
   `shims.rs` as the source of truth: the script reads it before
   overwriting the file, and reapplies the same gate to any of those
   symbols that still need a stub after regeneration. Verified idempotent:
   running the script twice in a row on an unchanged tree produces a
   byte-identical file.
2. **Two symbol-detection bugs, found while verifying the gate fix.** The
   generator's own symbol list was silently WRONG for a class of names:
   - The scan regex `(v8__|v8_inspector__)[A-Za-z0-9_]+` matches a
     substring, so a symbol like
     `std__shared_ptr__v8__Platform__CONVERT__std__unique_ptr` was captured
     starting mid-identifier as `v8__Platform__CONVERT__std__unique_ptr`, a
     different, wrong name that can never satisfy the real symbol at link
     time. This is the actual mechanism behind the old "the vendored
     rusty_v8 scope.rs/platform.rs decls use a form gen_hermes_shims.sh did
     not capture" comment. Fixed with a leading `[A-Za-z0-9_]*` so the whole
     identifier is captured.
   - The file scan was `vendor/rusty_v8/src/*.rs` (one directory only),
     missing `vendor/rusty_v8/src/scope/raw.rs` entirely: the file that
     declares `v8__TryCatch__CONSTRUCT`,
     `v8__AllowJavascriptExecutionScope__CONSTRUCT`, and
     `v8__DisallowJavascriptExecutionScope__CONSTRUCT`. Fixed with a
     recursive `find vendor/rusty_v8/src -name '*.rs'`.

   Together these two bugs are why 14 symbols previously had to be
   hand-appended in a special block at the bottom of `shims.rs` (visible in
   the pre-C6 file, with a comment explaining the generator gap). That block
   is gone: the generator now produces all of them itself, under their
   correct names.

Verified no regression: `cargo build --no-default-features --features
hermes` (pure-Rust stub) and `--features quickjs` both still compile clean
after regenerating `shims.rs`, and `cargo test --features hermes,link_hermes
--lib hermes::` still shows 12/12 passing with the regenerated file.

### Backend registration

`tests/harness/config.json`: added a 4th backend entry,
`id: "hermes"`, `features: "hermes,link_hermes"`, `os: "macos"` (the
vendored `hermes.framework` here is a prebuilt macOS artifact; see
`build.rs`'s `build_hermes`, which panics on any other `target_os`).
Created `tests/status/baselines/hermes/rusty_v8.txt` and
`tests/status/baselines/hermes/deno_core.txt`, both empty (header only, 0
passing), matching the format `run.mjs --update` itself would write.

### rusty_v8 baseline: run and result

```bash
node tests/harness/run.mjs rusty_v8 hermes
```

The harness runs unmodified: it builds each `[[test]] rv8_test_*` target
individually against `--features hermes,link_hermes` (same mechanism as the
other 3 backends), codesigns for JIT (the `os: "macos"` flag triggers the
same ad-hoc entitlements codesign the jsc/sys-jsc backends use; Hermes
doesn't strictly need it since it isn't reusing V8's JIT, but codesigning a
non-JIT binary is harmless, so no special-casing was needed), and reports
pass/fail per test plus which whole targets fail to link.

**Result: 0 passing / 16 total across 14 linking targets; 2 targets
(`rv8_test_api`, `rv8_test_cppgc`) fail to link.**

```
[hermes/rusty_v8] pass=0 fail=16 skip=0 (total 16)
unbuildable targets: rv8_test_api, rv8_test_cppgc
```

`node tests/harness/run.mjs rusty_v8 hermes --check` passes cleanly against
the empty baseline (`OK: ratchet holds (0 baselined)`), confirming the
harness integration itself is correct. A genuinely-empty baseline is not a
harness bug, it is the honest hill-climb starting line.

### Why 0/16, not just "doesn't link"

The 14 linking targets (`rv8_slots`, `rv8_test_api_flags`,
`rv8_test_api_entropy_source`, `rv8_test_simple_external`,
`rv8_test_external_deserialize`, `rv8_test_custom_platform`,
`rv8_test_single_threaded_default_platform`,
`rv8_test_platform_atomics_pump_message_loop`,
`rv8_test_concurrent_isolate_creation_and_disposal`) each fail every test
they contain. These exercise machinery this mission's scope did not touch:
isolate embedder-data "slots" helper wrappers, V8 flags parsing, entropy
source hooks, `External` values, snapshot/deserialize, custom platforms,
and atomics/message-loop plumbing, none of which route through
Object/Array/Number/Function. This is expected, not a regression: the
mission's Object/Array/Number/Function surface was never claimed to unlock
these tests, and the correct move (per the mission's explicit instruction)
is to record the honest 0 rather than chase them tonight.

### Why `rv8_test_api` and `rv8_test_cppgc` don't link

Both fail on genuinely undeclared symbols, confirmed from the raw linker
error, not assumed:

```
"_icu_get_default_locale"
"_icu_set_default_locale"
"_udata_setCommonData_77"
```

(both targets; ICU locale/data symbols the vendored crate references
unconditionally; no backend currently defines them for Hermes) plus, for
`rv8_test_api` only, the 11-member TypedArray constructor family:

```
"_v8__Int8Array__New"    "_v8__Uint8Array__New"    "_v8__Uint8ClampedArray__New"
"_v8__Int16Array__New"   "_v8__Uint16Array__New"
"_v8__Int32Array__New"   "_v8__Uint32Array__New"
"_v8__Float16Array__New" "_v8__Float32Array__New" "_v8__Float64Array__New"
"_v8__BigInt64Array__New" "_v8__BigUint64Array__New"
```

`rv8_test_cppgc` only needs the ICU trio. Its cppgc-specific symbols are
unreferenced by anything the test itself calls at the point it fails to
link, so `ld` doesn't get far enough to report them; the ICU symbols are
the actual first blocker. Per the mission, these are named and not chased
tonight: ICU is a small, self-contained follow-up (define a real or
no-op-consistent `icu_get_default_locale`/`icu_set_default_locale`/
`udata_setCommonData_77` trio, unrelated to JSI); TypedArrays are a
natural next widening of the surface this cycle started, following the
same `Object`/`Array` pattern.

## Regressions

- `cargo build --no-default-features --features hermes` (pure-Rust stub
  backend): compiles clean, unchanged.
- `cargo build --no-default-features --features quickjs` (default backend):
  compiles clean, unchanged.
- All pre-existing C2/C3/C4/C5 hermes tests: still pass (12/12 total with
  the new C6 tests).

## Recommended next step

1. **ICU trio** (`icu_get_default_locale`/`icu_set_default_locale`/
   `udata_setCommonData_77`): smallest, most leveraged unlock. Both
   currently-nonlinking targets need only this to at least link (`test_api`
   would then likely surface hundreds of individual test passes/failures
   instead of 0, since the file itself is large).
2. **TypedArrays** (`v8__*Array__New` family): natural continuation of the
   Object/Array widening this cycle did; would fully unlock `rv8_test_api`
   to link.
3. **`TryCatch`/exceptions**: `Script::Run` currently swallows a thrown JS
   error as a null `Local` with no exception object surfaced. Real
   `TryCatch` support needs a `jsi::JSError` to `v8::Value`
   materialization path (currently the C2 catch-all only prevents the C++
   exception from crossing into Rust; it does not preserve the thrown JS
   value for a caller to inspect). Several `rv8_slots`/platform-family
   tests may also depend on this indirectly.
