# C10: native function callbacks on Hermes, ratchet 58 -> 61

**Result: native function callbacks work.** A Rust/C v8 `FunctionCallback` is
now invoked when JS calls the function: `Function::New` builds a real callable,
the callback reads its arguments/this/data through the
`v8__FunctionCallbackInfo__*` accessors, sets a result through
`v8__ReturnValue__*`, and the value flows back out to JS. `FunctionTemplate`
(`New` + `GetFunction`) is implemented as a deferred `Function::New`. The
Hermes rusty_v8 baseline moves from 58 to **61 passing tests**
(`tests/status/baselines/hermes/rusty_v8.txt`), and `--check --rescue` holds
deterministically across two independent re-runs.

## What "callbacks work" means concretely

```
rv8_test_api::function_builder_raw          - Function::new_raw, args.length/
                                              get, ReturnValue::set(string)
rv8_test_api::function_callback_info_parts  - FunctionCallbackInfo::get_parts,
                                              args from parts, data (a Boolean)
                                              read as is_true, ReturnValue via
                                              parts.return_value
rv8_test_api::return_value                  - FunctionTemplate::new +
                                              get_function, every ReturnValue
                                              setter: set_bool/int32/uint32/
                                              null/undefined/double/empty_string
```

All three pass. An internal smoke test (`hermes_native_callback`) also proves
the full round trip: a `Function::new` callback that reads `args.get(0)`, sets
`rv.set_int32(1000 + len)`, installed on the global and called from `eval`.

## Design implemented

### The bridge (JSI host function <-> v8 FunctionCallback)

JSI drives host functions through a C++
`std::function<jsi::Value(Runtime&, const Value& this, const Value* args,
size_t count)>`; a v8 `FunctionCallback` is a C fn ptr the vendored surface
invokes with a `*const FunctionCallbackInfo`, reading everything back through
`v8__FunctionCallbackInfo__*` / `v8__ReturnValue__*` accessors. The two are
stitched together across the extern "C" boundary:

1. **C++ (`hermes_shim.cpp`): `v8x_hermes_function_new`** creates the JSI
   function via `jsi::Function::createFromHostFunction`. The host-function
   lambda captures the `FunctionCallback` fn ptr (as a `uintptr_t`) and the
   callback's `data` (copied into a `std::shared_ptr<jsi::Value>`, NOT a
   handle-table slot: `data` must outlive every HandleScope, and a slot would
   be truncated away by any scope exit; the shared_ptr is released when JSI
   tears down the host function, before the Runtime, honoring the C2 rule).
2. When JS calls the function, the lambda marshals `this`, `data`, and each
   arg into fresh handle-table slots (recording the watermark first), then
   calls back into Rust: **`v8x_hermes_dispatch_callback`**.
3. **Rust (`core.rs`): `v8x_hermes_dispatch_callback`** builds a Rust-owned
   `CbInfo` (the object a `*const FunctionCallbackInfo` points at), transmutes
   `callback_bits` back to a `FunctionCallback`, and invokes it. Each `CbInfo`
   field is a tagged Local pointer (the C4 `slot_ptr` encoding) into the
   isolate handle table, except `return_slot` (a `Box<usize>` the ReturnValue
   setters mutate, seeded with the tagged-undefined pointer so `rv.get()`
   before any set reads undefined).
4. After the callback returns, Rust reads the return slot back as a
   handle-table index and hands it to C++.
5. C++ materializes the result as a `jsi::Value` (a copy, taken BEFORE the
   handle table is truncated back to the watermark, since the result lives in
   a slot about to be freed), releases the callback's slots, and returns it to
   JSI. This emulates v8's implicit per-callback HandleScope (same reasoning
   as the QuickJS backend's op-dispatch arena save/restore).

### The FunctionCallbackInfo / ReturnValue layout contract

The accessors must match the vendored `function.rs` layout exactly (linking is
name-only, but our own reads/writes and the vendored reads must agree):

- `v8__FunctionCallbackInfo__GetReturnValue` returns a `usize` that is a raw
  pointer to the return-value storage (the `Box<usize>` in `CbInfo`).
- `v8__FunctionCallbackInfo__GetParts` returns
  `RawFunctionCallbackInfoParts { isolate, return_value: usize, data, length }`,
  the one-FFI-call fast path several callbacks use.
- `v8__ReturnValue__Value__Set*` take `*mut RawReturnValue` where
  `RawReturnValue(usize)` is that raw storage pointer. `Set(local)` writes the
  tagged Local pointer straight in; the primitive setters
  (`Set__Int32/Uint32/Double/Bool`, `SetNull/SetUndefined/SetEmptyString`)
  intern a fresh handle for the value and store its tagged pointer. `Value__Get`
  reads the slot back.

This is the same model the QuickJS/JSC backends use, adapted to Hermes's
tagged-handle-index Local encoding (their return slot is a `JSValue`; ours is
a `usize` holding a tagged Local pointer).

### FunctionTemplate

A `FunctionTemplate` in v8 is a deferred `Function::New` (captures callback +
data + length; `GetFunction` instantiates a real function). Hermes has no
template concept, so `v8__FunctionTemplate__New` leaks a small Rust-owned
`FnTemplate` record as a stable pointer (a template is not a JS value, so the
tagged-pointer scheme does not apply), and `v8__FunctionTemplate__GetFunction`
routes straight through `v8x_hermes_function_new`. Only the New/GetFunction
path the tests exercise is implemented; `SetClassName`/`PrototypeTemplate`/
`InstanceTemplate`/etc remain stubbed (the ObjectTemplate + prototype-chain
subsystem the `object_template*` tests need is a separate, larger cluster).

### Small value-predicate gaps the callback tests exposed (implemented)

`return_value` and `function_callback_info_parts` asserted through predicates
still stubbed (a null-returning stub reads as `false`), so the callbacks fired
correctly but the test assertions failed. Filled in, all small and pure-shape:
- `v8__Value__IsInt32` / `IsUint32`: Hermes stores every JS number as a double,
  so these test integer-valued doubles in i32 / u32 range.
- `v8__Value__IsNull` (routes to `jsi::Value::isNull`).
- `v8__Value__IsTrue` / `IsFalse`: the boolean `true`/`false` oddball (not
  truthiness), via the existing `boolean_value` (which returns 1/0/-1).
- `v8__Value__NumberValue` / `Int32Value`: the `Maybe<T>` out-param coercions
  (`Uint32Value` already existed). `Int32Value` applies ECMAScript ToInt32
  (truncate toward zero, wrap modulo 2^32) for finite doubles.

## The exception path (a callback that throws)

`Isolate::ThrowException` now records the thrown value both into the innermost
live TryCatch frame (the C9 path) AND into a new `IsoState::pending_exception`
slot. After a callback returns, `v8x_hermes_dispatch_callback` checks
`pending_exception`; if set, it clears it, hands the slot to C++
(`v8x_hermes_set_pending_callback_exception`), and signals `*threw = 1`. The
host-function lambda then re-throws it as a `jsi::JSError`, so it propagates
through JSI exactly like a JS-level throw and is caught by any enclosing
TryCatch via the normal C9 boundary. This is the required behavior (a callback
that throws must surface via TryCatch, not abort): a native throw during a
callback no longer silently returns undefined, and never unwinds a C++
exception across the extern "C" boundary.

## Process-crash landmines neutralized

- **No unwinding across `extern "C"`.** `v8x_hermes_dispatch_callback` is the
  one place a Rust `FunctionCallback` runs; it returns a plain slot sentinel
  and never lets a panic cross into C++. The C++ host-function lambda wraps its
  JSI calls and only throws a *controlled* `jsi::JSError` (the pending
  exception), which JSI is built to unwind; no other C++ exception escapes
  (the surrounding `v8x_hermes_function_new` is inside the C2 catch-all, and
  the lambda's own JSI value constructions are the only throwing calls, all of
  which produce a `jsi::JSError` JSI expects).
- **Handle-table pointer invalidation avoided.** The dispatch trampoline reads
  the return slot (and the pending-exception slot) BEFORE any truncation, and
  the C++ lambda copies the result `jsi::Value` out before `handles.resize`
  back to the watermark - the same use-after-realloc class C9 fixed in
  `throw_exception` (audited: no `slot_ref` pointer is held across a `push`
  here).
- No new process-crashers observed: the only SIGABRTs across three full
  `--rescue` runs are the three pre-existing `cppgc_*` tests (the standing
  BackingStore/cppgc gap), cleanly skipped by the harness, not masking passes.

## No regressions

- Internal hermes smoke tests: 13/13 pass (12 prior + the new
  `hermes_native_callback`)
  (`cargo test --no-default-features --features hermes,link_hermes --lib hermes::`).
- QuickJS build (`cargo check --no-default-features --features quickjs`):
  unaffected (only `src/hermes/*` changed).
- `gen_hermes_shims.sh` re-run: idempotent; auto-detects the newly-real
  symbols in core.rs (all plain names matching its regex, no `paste!` tokens)
  and drops their stubs (21 callback/ReturnValue/template stubs + the value
  predicates). No hand-written `misc.rs` gate needed.
- Stub-only `--features hermes` build: still blocked ONLY by the PRE-EXISTING,
  unrelated `misc.rs:100` `typed_array_new_stub!` macro error (missing
  `c_void` import), confirmed present on HEAD via `git stash` (12 identical
  errors before and after this change). Not touched, out of scope, same as C9
  noted. The real backend (`link_hermes`), the one the ratchet runs, builds
  and links clean.
- Baseline `tests/status/baselines/hermes/rusty_v8.txt` 58 -> 61; `--check
  --rescue` holds deterministically across two independent runs. Plain
  `--check` WITHOUT `--rescue` still false-regresses on the shared
  PROCESS_LOCK poison artifact (the C9 finding); any future CI wiring for
  hermes must pass `--rescue`.

## Recommended next target

**ObjectTemplate + FunctionTemplate instantiation** (`object_template`,
`object_template_from_function_template`, `instance_template_with_internal_
field`, `function_template_signature`, `context_from_object_template`): the
next-largest cluster, now within reach because the callback + template
machinery is landed. It needs `SetClassName`/`PrototypeTemplate`/
`InstanceTemplate`/`Template__Set` and instantiating a JS object with the
template's shape (properties + prototype). The other standing recommendation
from C8/C9 remains the **BackingStore + `std::shared_ptr` refcount**
subsystem, which also neutralizes the last three `cppgc_*`/backing-store
process-crashers. Native **property accessors** (`Object::SetAccessor`,
`FunctionTemplate::SetAccessorProperty`) reuse this cycle's callback bridge
via the `PropertyCallbackInfo` accessors and are a natural follow-on.
