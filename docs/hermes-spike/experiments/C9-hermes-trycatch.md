# C9: TryCatch / exception surfacing on Hermes, ratchet jumps 33 -> 58

**Result: TryCatch works.** A thrown JS error (`throw new Error(...)`, a
non-Error primitive throw, an embedder `ThrowException`) is now caught at the
JSI boundary, its value is inspectable from Rust via `TryCatch::exception()`,
a synthesized `TryCatch::message()` reproduces the "Uncaught <ctor>: <msg>"
text V8 tests expect, and `rethrow()`/`reset()` follow V8's documented quirks.
The Hermes rusty_v8 baseline moves from 33 to **58 passing tests**
(`tests/status/baselines/hermes/rusty_v8.txt`), and `--check --rescue` holds
deterministically across two independent re-runs.

Both changes landed together: the new TryCatch/exception surface, and a
pre-existing `Isolate::Enter`/`Exit` bug (dormant since C3) that TryCatch's own
test coverage was the first thing to actually exercise.

## What "TryCatch works" means concretely

```
rv8_test_api::try_catch                  - throw/catch, has_caught/exception/
                                            message/stack_trace, no-throw case,
                                            nested rethrow+reset quirk
rv8_test_api::try_catch_caught_lifetime  - exception()/message() values outlive
                                            the TryCatch scope but stay valid
                                            inside the enclosing HandleScope
rv8_test_api::throw_exception            - Isolate::ThrowException + strict-
                                            equals identity of the caught value
```

All three pass. 22 more tests also newly pass in this baseline update (see
"The poisoned-lock discovery" below) - they are not TryCatch-dependent
themselves, but were being hidden by a shared-mutex artifact this cycle also
diagnosed and worked around.

## Design implemented

**Per-runtime TryCatch stack (C++ side, `hermes_shim.cpp`).** `RuntimeWrapper`
gained `std::vector<TryCatchFrame> tc_stack`, mirroring v8's real semantics:
`TryCatch` is a stack-discipline scope, and only the INNERMOST live frame
observes a new exception. Each `TryCatchFrame` holds `exception_slot` (a
handle-table index, -1 = nothing caught), `has_caught`, `rethrown` (see
below), and `message`/`stack` strings.

- `v8x_hermes_trycatch_push`/`pop`: push a frame on `TryCatch::CONSTRUCT`, pop
  (truncate) on `DESTRUCT` - the same watermark discipline `HandleScope`
  already uses for the handle table.
- `RuntimeWrapper::capture_exception(const jsi::JSError&)`: the single sink
  every JS-throwing entry point (`v8x_hermes_run`, `v8x_hermes_eval_buffer`,
  `v8x_hermes_function_call`) now routes its `catch (const jsi::JSError&)`
  through. It copies `err.value()` into a fresh handle-table slot and
  `err.getMessage()`/`getStack()` into the frame - all while the `jsi::Runtime`
  is still alive (the JSError, and the Runtime it references, are both still
  live inside the `catch` block - the C2 lifetime rule). If `tc_stack` is
  empty (no TryCatch on the stack), the exception is silently dropped, same as
  V8 would fall through to a fatal-error handler we do not model.
- `v8x_hermes_throw_exception`: the embedder-throw path
  (`Isolate::ThrowException`). Captures the given value straight into the
  innermost frame. If the value is an Error-like object (has a string
  `.stack`/`.message` - what the real JS `Error`/`TypeError`/etc constructors
  populate), those are captured too, so `TryCatch::message()` can build the
  same "Uncaught <ctor>: <msg>" text a real thrown Error produces. A plain
  non-Error thrown value (e.g. a bare string) has no such properties, so
  message/stack stay empty - matching V8, which has no "<ctor>: text" line for
  that case either.
- `v8x_hermes_exception_new`: `Exception::Error/TypeError/RangeError/
  ReferenceError/SyntaxError` construct (but do NOT throw) the JS Error
  subtype via its global constructor (`new TypeError(msg)`) - JSI exposes no
  C++ Error-subtype factory, same pattern C8 used for ArrayBuffer/TypedArray.
- `v8x_hermes_trycatch_message`: synthesizes the Message text on demand
  ("Uncaught " + the stack's first line, or the bare message as fallback for a
  no-stack throw), pushed as a fresh String handle. A v8 `Message` handle IS
  this String slot; `v8__Message__Get` on the Rust side is a same-slot
  phantom-type re-tag, not a second C++ call.
- `v8x_hermes_trycatch_rethrow`: propagates a frame's caught exception to the
  next-outer live frame (marks it caught with the same value/message/stack),
  and sets `rethrown = true` on the inner frame so a later `reset()` on it is a
  documented no-op - the vendored test explicitly checks that `reset()` after
  `rethrow()` leaves `has_caught()` true
  (https://chromium-review.googlesource.com/c/v8/v8/+/5050065, referenced in
  the vendored test's own comment).

**Rust side (`core.rs`).** The vendored `raw::TryCatch` buffer is
`[MaybeUninit<usize>; 6]` (48 bytes, linking is name-only so only our own
read/write needs to agree on layout). Used as `[0]` = isolate ptr, `[1]` =
the C++ tc_stack frame index this scope owns - the same shape
`HandleScope::CONSTRUCT/DESTRUCT` already established for its watermark.
`v8__TryCatch__{CONSTRUCT,DESTRUCT,HasCaught,Exception,Message,StackTrace,
Reset,ReThrow,CanContinue,HasTerminated,IsVerbose,SetVerbose,
SetCaptureMessage}` all became real; `CanContinue` is always true (we never
model a fatal termination exception) and `HasTerminated`/`IsVerbose` are
always false (no `TerminateExecution`, no isolate-level message-listener
routing modeled). `v8__Isolate__ThrowException` and the five
`v8__Exception__*` constructors round out the surface.

## The Isolate::Enter/Exit bug this cycle found and fixed (load-bearing)

`v8__Isolate__Enter`/`Exit` have existed since C3 as a flat
`CURRENT_ISO.with(|c| c.set(...))`/`set(null)` pair - correct as long as
Enter/Exit is never nested. TryCatch's own test coverage was the first thing
to nest it: the vendored `Exception::type_error` (`exception.rs::new_error_with`)
brackets its constructor call in `scope.enter(); ...; scope.exit();` - a
SECOND, nested Enter/Exit pair while the isolate is already entered by the
enclosing `OwnedIsolate`/`HandleScope`. The old `Exit` unconditionally set
`CURRENT_ISO` to null, so after `Exception::type_error` returned,
`current_iso()` was null for the REST of the enclosing scope - breaking every
later `current_iso()`-dependent call, including the isolate's own
disposal-order assert (`OwnedIsolate::drop` asserts
`self.cxx_isolate == Isolate::GetCurrent()`), which double-panicked inside a
panicking destructor and SIGABRT'd the whole test binary
(`try_catch_caught_lifetime`, before this fix).

Fixed with a proper re-entrancy stack: `ISO_STACK: RefCell<Vec<*mut
RealIsolate>>`. `Enter` pushes whatever was current before it, then sets
current; `Exit` pops and restores exactly that. `Isolate::Dispose` also
retains-filters any stale stack entries for the disposed isolate as a
defensive guard (should always already be empty since every Enter is paired
with an Exit, but a leaked frame must never survive disposal).

## A second, more subtle bug: reading through a handle-table pointer across a push

While debugging why `try_catch_caught_lifetime`'s caught message didn't
contain "DANG" (after the Enter/Exit fix removed the crash), instrumentation
showed the thrown value's `isObject()` flipping from `true` (right after
`v8x_hermes_exception_new` pushed it) to `false` a few lines later inside
`v8x_hermes_throw_exception`. Root cause: `v8x_hermes_throw_exception` held
`const jsi::Value *v = slot_ref(w, value_slot)` (a raw pointer into
`w->handles`), then called `w->push(std::move(copy))` - which can reallocate
`std::vector<jsi::Value> handles`, invalidating `v` - and THEN read
`v->isObject()`/`v->getObject(...)` through the dangling pointer. Classic
iterator/pointer invalidation, silent because the freed memory usually still
looks plausible instead of crashing outright. Fixed by reading everything
needed from `*v` (the `.stack`/`.message` property lookups) BEFORE the
`push()` call. Audited every other `w->push(...)` call site in
`hermes_shim.cpp` for the same pattern; this was the only one (every other
site either pushes without holding a stale `slot_ref` pointer afterward, or
uses a Runtime-owned local `jsi::Value`/`jsi::Object`, not a handle-table
pointer).

## The poisoned-lock discovery (why the jump is 33 -> 58, not 33 -> 36)

Running the harness plain (`node tests/harness/run.mjs rusty_v8 hermes`) after
the fixes above still reported `try_catch`/`try_catch_caught_lifetime`/
`throw_exception` as FAILED, with the actual panic being
`called \`Result::unwrap()\` on an \`Err\` value: PoisonError { .. }` at
`test_api.rs:37:41` (`setup::parallel_test()`'s `PROCESS_LOCK.read().unwrap()`).
This vendored test-infra detail means: with `--test-threads 1`, ALL tests in
one process share `static PROCESS_LOCK: RwLock<()>`; if ANY earlier test
panics while its `SetupGuard` (holding a read/write guard) is still on the
stack, the lock poisons and EVERY later test that calls `setup::parallel_test()`
fails immediately at setup, before running any of its own body. In this run
`clear_kept_objects` (an unrelated, expected `Option::unwrap()` failure on a
still-missing subsystem) was the poisoning test; the ~200 or so alphabetically
later tests all failed with the same generic `PoisonError`, masking real
passes underneath, including all three TryCatch tests.

This exact failure mode (and its fix) is already documented from the QuickJS
backend's own rusty_v8 work: `run.mjs --rescue` solo-`--exact`-reruns every
batch-FAILED test in a fresh process (fresh, unpoisoned lock); a solo `ok`
supersedes the batch `FAILED` (dedup in `lib.mjs`'s `parseLibtest`). Running
`node tests/harness/run.mjs rusty_v8 hermes --rescue` immediately surfaced 25
new passes (0/3 of that run's remaining batch-FAILED tests were rescuable -
i.e. every poison-cascade victim was fully accounted for, and only 3 genuinely
still fail: the ones with real independent bugs). Confirmed deterministic
across two independent `--rescue` runs (identical 25-new-pass list both
times), then `--update --rescue` (58 passing) and `--check --rescue` (holds).

`--rescue` is opt-in per suite/backend and is NOT wired into `.github/
workflows/ci.yml` for hermes (hermes is not in the CI matrix at all yet - this
is a local-branch spike backend, matching the task's constraints). Plain
`--check` WITHOUT `--rescue` against this same baseline reports a false
"ratchet regression" (the poison cascade masking real passes again) - this is
expected and exactly mirrors why quickjs's CI cell already passes `--rescue`
(`ci.yml` sets `RESCUE="--rescue"` only for `matrix.backend == quickjs`).
**Any future CI wiring for hermes's rusty_v8 cell must pass `--rescue`**, or
`--check` will flap red on this exact artifact. Left as a note for whoever
wires hermes into CI; not done here (out of scope, and this branch never
touches CI per the task constraints).

## Process-crash landmines neutralized

- The `Isolate::Enter`/`Exit` bug above: before the fix, ANY test that
  triggered a nested Enter/Exit (in practice, any use of
  `Exception::type_error`/`Exception::error`/etc, which all go through
  `new_error_with`'s enter/exit bracket) would corrupt `CURRENT_ISO` and
  SIGABRT the whole binary on isolate teardown (a panic inside
  `OwnedIsolate::drop`, itself invoked during Rust's unwind of an EARLIER
  assertion failure, triggers "panic in a destructor during cleanup" ->
  `abort()`). This was a real process-crasher, not just a wrong-answer bug -
  it would have masked every test after the first `Exception::*` use in
  alphabetical order.
- The use-after-realloc in `v8x_hermes_throw_exception` above: silent memory
  corruption (read, not write - so no crash observed), would have produced
  wrong `isObject()`/property answers unpredictably depending on vector
  capacity headroom at the time of the call.

Both are fixed; no new crashers observed across three full-suite `--rescue`
runs (deterministic pass/fail sets each time) plus the internal smoke suite.

## No regressions

- Internal hermes smoke tests: 12/12 pass
  (`cargo test --no-default-features --features hermes,link_hermes --lib hermes::`).
- Stub-hermes build (`--features hermes`): the TryCatch/Exception/Message/
  ThrowException symbols all match `tools/gen_hermes_shims.sh`'s regex (no
  `paste!`-built names like C8's TypedArray family), so the generator drops
  their stubs automatically when core.rs implements them - no hand-written
  `misc.rs` gate needed. (A PRE-EXISTING, unrelated compile error in
  `misc.rs`'s `typed_array_new_stub!` macro - missing `c_void` import - blocks
  this build target already, on `main`/before this change too; confirmed via
  `git stash`. Not touched, out of scope.)
- QuickJS build (`--features quickjs`): unaffected (only `src/hermes/*`
  changed).
- `gen_hermes_shims.sh` re-run: idempotent (byte-identical `shims.rs` diff on
  a second run), drops exactly the 13 newly-real symbols' stubs.
- Baseline `tests/status/baselines/hermes/rusty_v8.txt` 33 -> 58; `--check
  --rescue` holds deterministically across two independent runs.

## Recommended next target

**Native `Function::new` callbacks** (`FunctionCallbackArguments`/
`ReturnValue`), per C8's original recommendation - unlocks the
microtask-queue and property-accessor clusters, and is the next-largest
still-stubbed subsystem. The **BackingStore + `std::shared_ptr` refcount**
subsystem (neutralizes the one remaining known process-crasher,
`array_buffer_with_shared_backing_store`) is the other standing
recommendation from C8, still open.
