# C12: Signature receiver check + a TryCatch exception-slot lifetime fix, ratchet 76 -> 77

**Result: `FunctionTemplate::Signature` (the receiver check) works.** `Signature::
New` records the source FunctionTemplate; a signature-bearing FunctionTemplate's
function now rejects a call whose receiver (nor any of its hidden prototypes)
was constructed by that template, throwing `TypeError: Illegal invocation`
before running the callback. Along the way this cycle found and fixed a
pre-existing TryCatch exception lifetime bug (the same class C11 found for
`EscapableHandleScope::escape`, applied to `TryCatch::Exception`), without
which the new test could not pass. The Hermes rusty_v8 baseline moves from 76
to **77 passing tests** (`tests/status/baselines/hermes/rusty_v8.txt`),
`--check --rescue` holds, and the internal Hermes smoke suite (now 15 tests)
is green.

This was a wind-down cycle with a hard time budget; named property
interceptors (the planned headline feature) were investigated and partly
designed but not landed - see "What did not land" below for the concrete
state and the recommended resumption point.

## What "Signature works" means concretely

```
rv8_test_api::function_template_signature - FunctionTemplate::new (templ0,
    no signature) + Signature::new(scope, templ0) + FunctionTemplate::builder
    (templ1).signature(...).build(): templ1's function accepts a receiver
    constructed by templ0 (`f.call(new C)` succeeds) and rejects any other
    receiver (`f.call(new Object)` throws "Illegal invocation").
```

An internal smoke test (`hermes_function_template_signature`) proves the same
round trip directly against the Hermes backend, independent of the vendored
test file.

## Design implemented

### Every FunctionTemplate gets a stable, process-global id

`FnTemplate` gained two `i64` fields: `template_id` (this template's own
stable id, assigned from a new process-global `AtomicUsize` counter
`NEXT_TEMPLATE_ID` at `FunctionTemplate::New` time, starting at 1) and
`signature_templ_id` (the target template id a `Signature` requires of the
receiver, or -1 for "no signature", the default). A process-global counter
(rather than a per-runtime one, unlike the C4 identity hash) is correct here
because templates are Rust-owned leaked pointers never tied to one Isolate/
Runtime.

### Signature is just the source FunctionTemplate's pointer

`v8__Signature__New(isolate, templ)` does no separate allocation: it returns
`templ` reinterpreted as `*const Signature`. The vendored surface treats a
`Signature` as an opaque `Data`-like handle it only ever hands back into
`FunctionTemplate::builder().signature(...)` (which reads a raw pointer out
of the `Local<Signature>` and forwards it as `signature_or_null: *const
Signature` to `FunctionTemplate::New`) - so `v8__FunctionTemplate__New` simply
casts `signature_or_null` back to `*const FnTemplate` and copies its
`template_id` into the new template's `signature_templ_id`. No unwrapping is
needed anywhere else; a `Signature` never flows through the tagged-Local
handle-table scheme the way a real JS value does; it lives entirely in the
same raw-pointer space as `FnTemplate`/`ObjTemplate` (see C11's `template_kind`
dispatch), just without needing a `TemplateHeader` of its own since nothing
ever inspects a `Signature`'s kind.

### Stamping + checking the receiver (C++, JSI side)

Every instance a template-backed constructor produces is stamped with its
FnTemplate's `template_id` via a hidden, non-enumerable Symbol-keyed property
(`v8x_template_id`), the same trick C4's identity hash and C11's internal
fields use - a per-runtime lazily-created Symbol plus cached
`Object.defineProperty`/`Object.getPrototypeOf`/a tiny `(obj, sym) => obj[sym]`
JS helper (JSI's `jsi::Object::getProperty` has no Symbol-keyed overload, so
reading a Symbol-keyed property back needs a one-line JS helper, same
reasoning as the identity-hash infra's own comment). `v8x_hermes_
stamp_template_id`/`v8x_hermes_check_signature` (new in `hermes_shim.cpp`) do
the write and the walk-and-compare respectively; the check walks `this`, then
`Object.getPrototypeOf(this)` repeatedly (capped at 64 hops defensively),
looking for a stamped id matching the signature's target - exactly the
vendored doc comment's contract: "the receiver (or any of its hidden
prototypes) was created from the signature's FunctionTemplate".

`v8x_hermes_function_new` gained two new parameters, `template_id` and
`signature_templ_id`, threaded from `v8__FunctionTemplate__GetFunction` (a
plain `Function::New`, unrelated to any template, passes `template_id=0`
which is never stamped, and `signature_templ_id=-1`, matching the v8 default
of "no signature, any receiver accepted"). Inside `hostFn`, BEFORE any of the
existing marshaling/internal-field/callback work: if `signature_templ_id >=
0`, the receiver is checked and a mismatch throws `jsi::JSError(rt, "Illegal
invocation")` immediately - matching v8, where an illegal-invocation call has
no other observable side effect. On the constructor path (`ifc >= 0` already
established by C11), the receiver is additionally stamped with `template_id`
right where C11 already ensures its internal-field slots, before the
constructor callback runs.

## The TryCatch exception-slot lifetime bug (load-bearing fix, found this cycle)

`function_template_signature` uses the vendored `eval()` test helper, which
wraps every `Script::run` in an `EscapableHandleScope` (see C11's own note on
this same helper). The new Signature test is the first baselined-cluster test
to call `eval()`, have it **throw**, and then read `scope.exception()`
**after** `eval()` has already returned. That sequence exposed a bug
independent of anything Signature-specific: `TryCatchFrame::exception_slot`
was a raw handle-table slot index recorded once, at catch time, while the
`eval()` helper's `EscapableHandleScope` was still on the stack. When `eval()`
returns `None` (nothing to escape, since nothing succeeded), that
`EscapableHandleScope`'s destructor still runs and truncates the handle table
back to its own watermark - reclaiming the very slot the exception was
recorded at. Every later read of `TryCatch::Exception()` (via
`v8x_hermes_trycatch_exception`) then returned a stale or reused slot instead
of the real exception - so the message assertion failed even though the throw
and the `has_caught()` signal were both completely correct. This is the exact
same *class* of bug C11 fixed for `EscapableHandleScope::escape` (a value
recorded once, in a slot a later scope-exit truncates away), just for a
different subsystem (`TryCatch` instead of `EscapableHandleScope`) - and, like
that one, was dormant because no previously-baselined test exercised a
post-`eval()`-throw exception read.

The fix: `TryCatchFrame` now holds the caught exception as a Runtime-owned
`std::shared_ptr<jsi::Value>` (`exception_value`, outliving every
HandleScope - the same durable-storage pattern C10's callback `data` and
C11's accessor `key`/`data` captures already use) instead of a handle-table
slot index. `v8x_hermes_trycatch_exception`/`v8x_hermes_trycatch_rethrow` now
materialize a **fresh** handle-table slot from that holder on every call,
rather than returning a slot recorded once at capture time; `capture_exception`,
`v8x_hermes_throw_exception`, and `v8x_hermes_trycatch_reset` were updated to
match (write/clear `exception_value` instead of `exception_slot`). No test
that was passing before this fix depended on the old (buggy) behavior, and
`try_catch`/`try_catch_caught_lifetime`/`throw_exception` (already passing,
unbaselined) continue to pass identically after the change.

## Process-crash landmines: investigated, none neutralized this cycle

Before starting Signature/interceptors, this cycle investigated the 4
pre-existing crashers (`array_buffer_with_shared_backing_store` +
`cppgc_cell`/`cppgc_object_wrap8`/`cppgc_object_wrap16`) to see whether they
could be converted from SIGABRT to a graceful test failure, per the mission's
stability goal. Finding, precisely reproduced:

- Every one of these crashes is `panic in a function that cannot unwind` /
  `thread caused non-unwinding panic. aborting.` - a Rust panic (an `assert!`
  or `.unwrap()` failure) occurring **inside a native `FunctionCallback`**
  that the vendored `rusty_v8` test itself registers (e.g. `test_cppgc.rs`'s
  `op_wrap` asserting `obj.is_api_wrapper()`, or unwrapping
  `scope.get_cpp_heap()`, both currently null-stub-backed).
- The panic must physically unwind out of the vendored `unsafe extern "C" fn
  c_fn` trampoline (`vendor/rusty_v8/src/support.rs`'s `impl_c_fn_from!`
  macro) to reach ANY enclosing Rust `catch_unwind` - Rust's default ABI marks
  `extern "C" fn` as non-unwinding, so the panic aborts the process trying to
  leave that vendored frame, before it ever reaches a catch site in
  `src/hermes/core.rs` (confirmed with a minimal standalone repro: a
  `catch_unwind` placed one Rust frame further OUT than the panicking
  `extern "C" fn` never even runs; a `catch_unwind` placed INSIDE the exact
  same `extern "C" fn` that panics works fine, but that frame is vendored code
  this repo must never edit).
- A `catch_unwind` was prototyped at the two Hermes-owned dispatch boundaries
  (`v8x_hermes_dispatch_callback`, the accessor getter/setter trampolines) and
  built/tested cleanly, but confirmed (via the same repro plus a live rerun of
  `rv8_test_cppgc`) to have **no effect** on these specific crashers, since the
  vendored `c_fn` frame sits between the panic and any catch site Hermes owns.
  It was reverted (not committed) rather than kept as inert dead weight.

Conclusion: neutralizing these four crashers is not reachable without editing
vendored `rusty_v8` source (`support.rs`'s callback trampoline macro), which
is out of scope per repo policy. They remain the standing recommendation for
whoever next revisits this - either accept the constraint and design a
different bridge for callback dispatch that does not route through
`impl_c_fn_from!`'s `unsafe extern "C" fn c_fn`, or treat them as permanently
un-neutralizable within the vendor-verbatim policy. `--rescue` continues to
cleanly skip all four without masking any other pass, both before and after
this cycle's changes - the suite is exactly as stable as it was at the start
of C12, no better and no worse.

## What did not land: named property interceptors

The planned headline feature (`ObjectTemplate::SetNamedPropertyHandler`,
`object_template_set_named_property_handler`,
`context_with_object_template`, `security_token`) was investigated in depth
but not implemented this cycle, in the interest of banking the Signature +
exception-lifetime fixes cleanly rather than leaving a half-built subsystem
uncommitted. Design notes for whoever picks this up next:

- The exact C-ABI shape is confirmed from `vendor/rusty_v8/src/template.rs`
  (`v8__ObjectTemplate__SetNamedPropertyHandler`, taking 7 optional callback
  fn-ptr params + `data_or_null` + `flags: PropertyHandlerFlags` (a `u32`
  newtype, NOT a `c_int`)) and `vendor/rusty_v8/src/function.rs` (the
  `NamedGetter/Setter/Query/Deleter/DefinerCallback` types, all
  `unsafe extern "C" fn(SealedLocal<Name>, ..., *const PropertyCallbackInfo
  <T>) -> Intercepted`, plus `PropertyEnumeratorCallback` which is the one
  exception - `void`-returning, no `Intercepted`).
- **`Intercepted` is `#[repr(u32)] { kYes = 0, kNo = 1 }` - INVERTED from the
  intuitive mapping.** Any dispatch trampoline for a getter/setter/query/
  deleter/definer callback must treat a returned `0` as "yes, intercepted"
  and `1` as "no, fall through to normal JS semantics". This was confirmed
  against the JSC backend's real bindgen output
  (`v8_Intercepted` in a generated `binding.rs`) since the enum is not
  otherwise visible from the vendored Rust source directly (it re-exports a
  C++ enum through `crate::binding::v8__Intercepted`).
- The recommended implementation strategy (unchanged from the plan handed
  into this cycle): reuse C11's `PropCbInfo`/accessor-dispatch bridge
  (`v8x_hermes_dispatch_accessor_getter/setter` already builds the
  `PropertyCallbackInfo`-shaped object C11 needs; a parallel `PropCbInfo`
  variant or a small extension covering `Intercepted`-returning callbacks
  should suffice for getter/setter/deleter) and materialize the intercepted
  object as a JSI `Proxy`: `ObjectTemplate::NewInstance` builds the plain
  target object exactly as C11 already does (with its internal fields, since
  the vendored tests store the interceptor's state THERE, not in the Proxy
  itself), then, only if a named handler was registered, wraps it via
  `global.Proxy` + `callAsConstructor(target, handler)` where `handler`'s
  `get`/`set`/`deleteProperty`/`ownKeys` are JSI host functions dispatching
  back into Rust through the new trampolines. `args.holder()` inside a
  callback should return the **target** (not the Proxy wrapper), matching
  the vendored test's own internal-field reads through `this` inside each
  callback.
- Recommended scope for a first landing pass: getter + setter + deleter +
  enumerator only (skip `query`/`definer`/`descriptor`, which need a real
  `PropertyDescriptor` CONSTRUCT/DESTRUCT bridge - a separate, larger
  subsystem mirroring `vendor/rusty_v8/src/property_descriptor.rs`'s
  `v8__PropertyDescriptor__*` C++-object-shaped ABI, out of scope for a
  single cycle). This scope is enough to fully pass `security_token`
  (getter + `.data()` only) even though it will not be enough to fully pass
  `object_template_set_named_property_handler` (which exercises all seven
  handler slots in one `#[test] fn` - partial support only reduces failures
  inside that one test, it does not flip it green on its own).
- `object_template_set_indexed_property_handler` /
  `indexed_property_handler_non_masking` are the index-keyed analog
  (`u32` index instead of a `Name` key) and should reuse the same Proxy
  machinery with an integer-keyed `get`/`set`/`deleteProperty` trap
  dispatch, once the named case is solid.

## No regressions

- Internal hermes smoke tests: 15/15 pass (14 prior + the new
  `hermes_function_template_signature`)
  (`cargo test --no-default-features --features hermes,link_hermes --lib hermes::`).
- QuickJS build (`cargo check --no-default-features --features quickjs`):
  unaffected (only `src/hermes/*` and the auto-generated `shims.rs` changed).
- `gen_hermes_shims.sh` re-run: idempotent; drops the now-real
  `v8__Signature__New` stub, no hand-written gate needed.
- `try_catch`/`try_catch_caught_lifetime`/`throw_exception` (already passing
  before this cycle, not yet baselined) continue to pass identically after
  the exception-lifetime fix - confirmed via a direct before/after run on a
  clean `git stash` of this cycle's changes.
- Baseline `tests/status/baselines/hermes/rusty_v8.txt` 76 -> 77; `--update
  --rescue` then `--check --rescue` holds.
- The 4 pre-existing crashers (`array_buffer_with_shared_backing_store`,
  `cppgc_cell`, `cppgc_object_wrap8`, `cppgc_object_wrap16`) are unchanged:
  still cleanly skipped by `--rescue`, not masking any pass, and (per the
  investigation above) not neutralizable without editing vendored code.

## Recommended next target (C13)

Named property interceptors (getter/setter/deleter/enumerator via a JSI
`Proxy`), per the detailed design notes above - this is the highest-leverage
remaining item, large enough to warrant its own cycle rather than a
partial/uncommitted attempt. `function_template_signature`'s receiver check
composes naturally with a future subclassing test if one gets added (the
`template_id` stamp/walk already generalizes past single-level construction).
The BackingStore/`std::shared_ptr` refcount subsystem and the 4 process
crashers remain open, now confirmed non-trivial to close within this repo's
"never edit vendored code" constraint - closing them would need either a
different callback-dispatch bridge design (avoiding
`impl_c_fn_from!`'s `unsafe extern "C" fn c_fn`) or accepting them as a
permanent, harness-skipped gap.
