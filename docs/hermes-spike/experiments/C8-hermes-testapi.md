# C8: rv8_test_api LINKS on Hermes; the ratchet jumps 10 -> 33

**Result: `rv8_test_api` and `rv8_test_cppgc` now LINK on the Hermes backend, and
the ratchet counts 33 individually-passing rusty_v8 tests, up from 10.** Before
C8 the harness counted 16 whole test files (10 passing); test_api and cppgc did
not link, so their hundreds of individual cases scored 0 and never surfaced.
With the two missing symbol families supplied, both targets link, test_api
enumerates 248 cases, and 23 of them pass green (plus the 10 prior file-level
passes = 33). `--update` rewrote `tests/status/baselines/hermes/rusty_v8.txt`
(37 lines, 33 test names) and `--check` holds deterministically.

## What was blocking the link (two symbol families the generator misses)

`tools/gen_hermes_shims.sh` stubs every symbol whose name matches
`(v8__|v8_inspector__)...`. Two families slipped through that net and went
undefined at link time:

1. **ICU trio** - `icu_get_default_locale`, `icu_set_default_locale`,
   `udata_setCommonData_77`. These have no `v8__` prefix, so the generator never
   emitted stubs. Both non-linking targets needed only this trio to link.
2. **TypedArray constructors** - `v8__Uint8Array__New` and the eleven siblings
   (`Uint8ClampedArray`, `Int8Array`, `Uint16Array`, `Int16Array`,
   `Uint32Array`, `Int32Array`, `Float16Array`, `Float32Array`, `Float64Array`,
   `BigUint64Array`, `BigInt64Array`). The vendored `typed_array!` macro builds
   these names via `paste!` (`[< v8__ $name __New >]`), so the whole symbol name
   never appears as one token the generator's regex can match. test_api needed
   all twelve.

## What was implemented

ICU trio (`src/hermes/misc.rs`, always compiled, pure Rust, no JSI): a
process-global default-locale string (default `en-US`, get/set), and a
header-magic validation of the common-data blob (bytes [2],[3] == 0xDA 0x27 ->
ok, else `U_INVALID_FORMAT_ERROR`), mirroring the QuickJS backend's approach.
`icu_set_common_data_fail` passes as a result.

ArrayBuffer + TypedArrays (`src/hermes/core.rs` + `hermes_shim.cpp`, link_hermes
only): JSI has a real `jsi::ArrayBuffer` (data()/size()) but no C++ factory for a
fresh one, so ArrayBuffer allocation and every typed-array constructor route
through the JS `ArrayBuffer`/`Uint8Array`/etc constructors on the runtime's
global (`getPropertyAsFunction` + `callAsConstructor`). Implemented
`v8__ArrayBuffer__New__with_byte_length`/`ByteLength`/`Data`,
`v8__TypedArray__Length`, and the twelve `v8__<Name>Array__New` via a macro. A
`&'static CStr` per constructor name avoids per-call allocation. `Float16Array`
is not a Hermes global; its constructor returns the null slot rather than
aborting.

Two crash-guard symbols promoted to real bodies in `core.rs`:
- `v8__V8__GetVersion` returns `crate::VERSION_STRING` (a null return would SEGV
  in the vendored `CStr::from_ptr(null)`); `get_version` passes.
- `v8__Context__Get/SetMicrotaskQueue` round-trip a stable per-isolate non-null
  marker pointer (a null return would SEGV in the vendored `&*ptr` deref). No
  real queue is installed, so `microtask_queue` still fails, but gracefully.

## Process-crash landmines neutralized (the important part)

rusty_v8 runs a whole file's hundreds of tests in ONE process. A panic in an
`extern "C"` function cannot unwind and aborts the entire binary, masking every
other pass. The auto-generated stubs used `unimplemented!()`, so the first test
to touch any unimplemented setup symbol (e.g. `v8__V8__SetFatalErrorHandler`,
hit by `add_message_listener`) took the whole binary down.

`tools/gen_hermes_shims.sh` now emits a **null-returning** stub
(`... -> *const c_void { null() }`) instead of `unimplemented!()`. Linking is
name-only (the vendored extern decl's real signature is never type-checked
against the stub), so one null-pointer return satisfies every declared shape:
void-returning setup symbols become true no-ops (the extra x0=null is ignored),
and value-returning ones hand back a NULL handle the many `if this.is_null()`
guards in core.rs short-circuit. This converted ~20 process-aborting stubs into
graceful single-test failures and, together with the two crash-guards above,
left exactly **one** remaining process-crasher.

The generator stays idempotent (byte-identical shims.rs on re-run) and preserves
its gate-reapplication logic. Symbols newly given real `core.rs` bodies
(GetVersion, the microtask pair, the four ArrayBuffer/TypedArray-Length symbols
whose names DO match the regex) get their `#[cfg(not(feature = "link_hermes"))]`
stub in `src/hermes/misc.rs` (hand-written, generator never touches it) so the
stub-only `--features hermes` build still links.

## The one remaining crasher (left for a later cycle)

`rv8_test_api::array_buffer_with_shared_backing_store` aborts: it calls
`ArrayBuffer::get_backing_store()` (stub returns null) then deref's the null
`SharedRef<BackingStore>`, and asserts exact shared_ptr use-counts (2, 3, 4).
This needs the full BackingStore + `std::shared_ptr` refcount subsystem, not a
dummy. The harness recovery cleanly skips just this one test and runs all others
(only 1 skip, fast), so it does not mask the 23 passes. Neutralizing it properly
is the recommended BackingStore work below.

## The 23 test_api passes

19 are engine-independent `crdtp_*` inspector-protocol cases (they exercise
`src/crdtp_shim.rs`, not Hermes). The four that touch our surface:
`cached_data_version_tag`, `get_version`, `icu_set_common_data_fail`,
`inspector_string_view`, `latin1_to_utf8`.

## Why most of the other ~224 do not pass yet (all fail gracefully)

The bulk need multi-feature subsystems still stubbed:
- **TryCatch / exception surfacing** - `try_catch` and every `tc_scope!` test
  need `Script::Run` to surface a thrown `jsi::JSError` as a v8 exception and
  `TryCatch::HasCaught/Exception/Message` to read it. Currently Run swallows the
  error to a null Local. Biggest single cluster.
- **Native Function callbacks** - `Function::new(rust_closure)` +
  `FunctionCallbackArguments`/`ReturnValue`. Needed by `microtask_queue`,
  property-accessor, and many object tests.
- **BackingStore + shared_ptr** - the get_backing_store family above.
- **TypedArray views** - `is_uint8_array()` and the other view predicates
  (stubbed -> false), `byte_length`/`byte_offset`/`copy_contents`/`buffer`.
  `typed_array_constructors` also `.unwrap()`s a `Float16Array` Hermes lacks.
- **Object prototype/creation-context** - `with_prototype_and_properties`,
  `get_creation_context`, `delete`, `has_own_property`, `get_hash`.

## Recommended next target

**TryCatch / exception surfacing.** It is the single largest cluster (every
`tc_scope!`-based test), and the plumbing is contained: `v8x_hermes_run` already
catches the `jsi::JSError` at the C++ boundary; surface its embedded value into
a handle-table slot, store it as the isolate's pending exception, and back
`v8__TryCatch__HasCaught/Exception/Message` with it. After that, native
`Function::new` callbacks (unlocks microtask + accessor clusters), then the
BackingStore subsystem (also neutralizes the last process-crasher).

## No regressions

- Internal hermes smoke tests: 12/12 pass
  (`cargo test --no-default-features --features hermes,link_hermes --lib hermes::`).
- Stub-hermes build (`--features hermes`): links clean (misc.rs supplies the
  gated stubs for every newly-core-implemented symbol).
- QuickJS build (`--features quickjs`): unaffected (only `src/hermes/*` and
  `tools/gen_hermes_shims.sh` changed).
- `gen_hermes_shims.sh` verified byte-identical on a second run.
- Baseline `tests/status/baselines/hermes/rusty_v8.txt` 10 -> 33; `--check`
  holds deterministically.
