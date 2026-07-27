# C4: object identity is solvable on Hermes (strict equality + stable hash)

**Result: YES, solvable.** JSI hands out no raw pointer, so two `Local`s
(handle-table slot indices, see C3) obtained for the same JS object really do
carry different tagged pointers: this is demonstrated first, not assumed. But
both identity-sensitive parts of the V8 C-ABI that a naive port would get
wrong reroute cleanly through JSI's own primitives instead of slot identity:

- `v8__Value__StrictEquals`/`SameValue` reroute through
  `jsi::Value::strictEquals`, JSI's own `===`-semantics comparison over the
  underlying heap value, not the handle-table slot.
- `v8__Object__GetIdentityHash` reroutes through a hidden, non-enumerable,
  Symbol-keyed property lazily attached to the object itself, holding a
  monotonically increasing id. The id lives on the object's own heap storage,
  so it survives being read back through any number of different slots.

A new test, `hermes_identity` (behind
`cfg(all(feature = "engine_hermes", feature = "link_hermes"))`), proves all of
this end to end against a real libhermes and passes.

## Test command and result

```bash
cargo test --no-default-features --features hermes,link_hermes \
  --lib hermes:: -- --nocapture
```

```
running 4 tests
test hermes::hello_world::hermes_backend_runs_hello_world ... ok
test hermes::hermes_identity::hermes_identity ...
hermes_identity: two Locals to globalThis.o -> tagged pointers 0xb vs 0xf (differ: true)
hermes_identity: StrictEquals(o,o)=true StrictEquals(o,p)=false
hermes_identity: GetIdentityHash(o via slot A)=1 GetIdentityHash(o via slot B)=1 GetIdentityHash(p)=2
hermes_identity: Object.keys=["marker"] JSON.stringify={"marker":"same-object"} for-in-count=1 getOwnPropertySymbols.length=1
hermes_identity: PASS - StrictEquals/SameValue and GetIdentityHash both reroute through JSI object identity, not slot identity; hidden id invisible to Object.keys/JSON.stringify/for-in and non-enumerable
ok
test hermes::tests::hermes_smoke_catches_js_error ... ok
test hermes::tests::hermes_smoke_eval_40_plus_2 ... ok

test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 13 filtered out; finished in 0.00s
```

Verified stable across repeated runs, both with `--test-threads=1` and the
default parallel test runner (see "V8 global init is process-wide" below).

## Point 1: the problem, demonstrated first

The test stashes one real JS object on `globalThis.o`, then evaluates
`globalThis.o` twice as two independent scripts. Each eval pushes a fresh
handle-table slot holding the same underlying JSI value, so the two `Local`
pointers differ:

```
tagged pointers 0xb vs 0xf (differ: true)
```

A literal port of V8's `Value*`/`Object*` pointer-equality identity would
therefore call these "different objects", which is wrong: this is the C0/C3
risk made concrete and measured, not assumed.

## Point 2: strict equality routes through `jsi::Runtime::strictEquals`

`hermes_shim.cpp` adds `v8x_hermes_strict_equals(rtw, slotA, slotB)`, which
calls `jsi::Value::strictEquals(runtime, a, b)` (JSI's own static helper,
which dispatches internally to the `Symbol`/`String`/`Object` overloads of
`Runtime::strictEquals` for the respective kinds, and does value comparison
for primitives). `core.rs` wires this into `v8__Value__StrictEquals`, and
`v8__Value__SameValue` calls the same path (see the residual-risk note
below).

Test assertions:
- `o_first.strict_equals(o_second)` is `true` (same object, two different
  slots).
- `o_first.strict_equals(p)` is `false` (different objects).
- Both also hold for `same_value`.

## Point 3: stable identity hash via a hidden Symbol-keyed property

`v8__Object__GetIdentityHash` is the crux, because JSI has no built-in
identity hash at all. The fix is the standard native-embedder trick, done at
the JSI/C++ level in `v8x_hermes_get_identity_hash`:

- Lazily (once per runtime), evaluate a tiny JS snippet that creates a real JS
  `Symbol('v8x_identity_id')` and returns it alongside a reference to
  `Object.defineProperty`. Both are cached as `jsi::Value`s on the
  `RuntimeWrapper`, so no further script compiles happen on the hot path.
- On `GetIdentityHash(obj)`: read back an existing id via
  `Object.getOwnPropertyDescriptor(obj, symbol)` (Symbol-keyed reads have no
  direct `Object::getProperty` overload in the JSI C++ surface, so this goes
  through the real JS function, called via JSI's `Function::call`). If a
  descriptor exists, return its `.value`.
- Otherwise, assign the next monotonically increasing counter value and
  install it via `Object.defineProperty(obj, symbol, { value, enumerable:
  false, writable: false, configurable: false })`, called directly through
  the cached `Function` (no per-call script compile).

Because the id is stored ON the object's own heap storage (a real property,
keyed by a Symbol), not on the non-canonical slot, reading the same object
back through any number of different `Local`s/slots yields the same id.

Test evidence:

```
GetIdentityHash(o via slot A)=1 GetIdentityHash(o via slot B)=1 GetIdentityHash(p)=2
```

Same object through two different slots: hash `1` both times. A different
object: hash `2`. A third read of `globalThis.o` through a brand new eval (a
third slot) was also checked and still returns `1`.

### Why not a JSI-native Symbol-keyed property API

JSI's `PropNameID` is always string-based
(`PropNameID::forAscii`/`forUtf8`/`forString`); there is no
`Object::setProperty`/`getProperty` overload that takes a `Symbol` key in the
C++ surface (`jsi/jsi.h`), even though the JS language itself supports
Symbol-keyed properties. So installing/reading a Symbol-keyed hidden property
from native code has to go through the real JS-level `Object.defineProperty`/
`Object.getOwnPropertyDescriptor`, not a JSI method call. This is not a
workaround unique to this spike: it is how any Hermes/JSI embedder (including
React Native itself) attaches hidden per-object native state today.

### Is the hidden id actually invisible to JS?

Yes, with one precise caveat that the test checks explicitly rather than
assuming:

- `Object.keys(o)`, `JSON.stringify(o)`, and `for...in` over `o` all show
  only the real `marker` property. Confirmed in the test output above
  (`Object.keys=["marker"]`, `JSON.stringify={"marker":"same-object"}`,
  `for-in-count=1`). This is because Symbol-keyed properties are never
  enumerated by any of the *string-keyed enumerable* mechanisms, independent
  of whether they are themselves enumerable.
- `Object.getOwnPropertySymbols(o)` DOES see the hidden Symbol as an own
  property (`getOwnPropertySymbols.length=1`): this is correct JS semantics
  (Symbol-keyed properties are only hidden from string-keyed enumeration,
  never from explicit reflection) and is not a leak of the marker itself
  (the symbol carries no readable name collision with real code, and no
  ordinary program enumerates `getOwnPropertySymbols` on arbitrary objects).
  The test additionally confirms the property found this way reports
  `enumerable: false`, which is the actual "hidden" guarantee: it will never
  appear in `Object.keys`/`for-in`/`JSON.stringify`/`Object.assign`/spread,
  which are the enumeration paths real JS code and the rest of the v8 surface
  (e.g. `GetOwnPropertyNames` with the default filter) rely on.

## Symbols added

`src/hermes/hermes_shim.cpp` (C++/JSI side):
- `v8x_hermes_strict_equals(rtw, a, b) -> int` (1/0/-1)
- `v8x_hermes_get_identity_hash(rtw, slot) -> int64_t` (>=1, or -1 on error)
- `v8x_hermes_value_is_object(rtw, slot) -> int` (needed so Rust can safely
  `Local<Value>::try_cast::<Object>()`, which checks `Value::is_object()`
  first)
- A `RuntimeWrapper` extension: `identity_symbol`, `define_property_fn`
  (lazily cached `jsi::Value`s), `next_identity_id` (the monotonic counter).

`src/hermes/core.rs` (v8 C-ABI side, real, replacing the stub):
- `v8__Value__StrictEquals`, `v8__Value__SameValue`
- `v8__Object__GetIdentityHash`
- `v8__Value__IsObject`, `v8__Value__ToObject` (needed to safely reach an
  `Object` handle from a `Value` handle in the test and in general; `ToObject`
  is the identity function on the tagged pointer here, since `Value` and
  `Object` share the same handle-table-slot representation).

`src/hermes/shims.rs`: the five now-real symbols above had their
auto-generated `unimplemented!()` stubs re-gated behind
`#[cfg(not(feature = "link_hermes"))]`, by hand, matching the existing
pattern for every other real symbol in this file. Do NOT re-run
`tools/gen_hermes_shims.sh` blindly: the checked-in `shims.rs` has been
hand-patched with these gates for every already-implemented symbol, and the
generator script itself does not emit them (it only decides whether to
include a stub at all, based on whether the symbol appears anywhere in
`src/hermes/*.rs`, with no notion of feature-gating the exclusion). Running
it fresh silently drops every existing gate and breaks the pure-Rust stub
build's link step (verified empirically during this experiment: `cargo build
--features hermes` still "succeeds" because a `cdylib`-free `cargo build`
does not force full symbol resolution, but `cargo test --features hermes`
immediately fails to link with "symbol not found" for every symbol `core.rs`
defines, since `core` itself is excluded by `#[cfg(feature = "link_hermes")]`
in `mod.rs`). This is a pre-existing sharp edge in the generator design, not
introduced by this experiment, and worth fixing before the generator is run
again for a future cycle.

## Load-bearing fix found along the way: V8 global init is process-wide

`v8::V8::initialize_platform`/`initialize` gate a single, process-wide
`GLOBAL_STATE` mutex in the vendored crate (`Uninitialized ->
PlatformInitialized -> Initialized`, panicking on any other transition). The
existing C3 `hello_world` test module had its own private `Once` guarding
those calls; adding a second test module (`hermes_identity`) with its own
independent `Once` made the second module's initialization attempt panic with
"Invalid global state", because the process had already been driven to
`Initialized` by whichever test ran first. Fixed by hoisting a single
`init_v8_once()` helper to `src/hermes/mod.rs`, shared by every test module
in the file. This is a general rule for any future Hermes (or other backend)
test module added to this file: share the one init guard, never declare a
private one. Verified stable across both `--test-threads=1` and the default
parallel runner, several repeated runs.

## Residual risks (recorded honestly, not papered over)

1. **`SameValue` is currently implemented identically to `StrictEquals`.**
   Real V8 `SameValue` (`Object.is`) differs from `===` only at `NaN`
   (`NaN` is same-value to itself, unlike `===`) and signed zero (`+0`/`-0`
   are same-value-distinct, unlike `===`). JSI's `Value` does not expose
   bit-level float inspection (no `isNaN`/raw-bits accessor) needed to
   special-case these without going through JS (`Object.is` itself, or
   `Number.isNaN` + a sign check). The current implementation is exact for
   the object-identity surface this experiment targets (objects, strings,
   booleans) and for ordinary non-NaN, non-zero numbers, but not yet exact
   for the NaN/+-0 edge cases. Fix direction: route `SameValue` through a
   cached JS `Object.is` function the same way `GetIdentityHash` routes
   through `Object.defineProperty`, rather than reusing `StrictEquals`.
2. **No canonicalization / interned slot per object was built.** Point 4 in
   the mission (a per-runtime identity-id -> canonical-slot map, so repeated
   Locals to one object could share a slot) was left undone by design, to
   keep scope tight. Whether it is needed depends on what the broader rusty_v8
   surface actually requires: `StrictEquals`/`SameValue`/`GetIdentityHash`
   together are almost certainly sufficient for `Map`/`Set`/`WeakMap` keying
   correctness (V8's own embedder-facing identity contract IS "same
   StrictEquals + same GetIdentityHash", not "same pointer"), since Rust-side
   consumers (rusty_v8's own `HashMap`/hashing helpers, if any exist over raw
   `Local` pointers rather than going through `GetIdentityHash`) are the only
   place slot-pointer equality could still leak through as an implicit
   identity check. That surface has not been audited yet; flagging it as the
   next thing to check before broad `Object`/`Map`/`Set` work lands, rather
   than assuming it away.
3. **`GetIdentityHash`'s counter is per-runtime, unbounded, and never
   reclaimed.** Every distinct object ever hashed keeps a permanent
   `next_identity_id` slot conceptually reserved (though the actual storage
   cost is one property per hashed object, freed when the object itself is
   GC'd; the counter itself never shrinks). This matches real V8's own
   identity-hash behavior (also monotonic, also never reused) so it is not a
   new limitation, just worth naming.
4. **The two extra script-eval round trips inside `GetIdentityHash`
   (`getOwnPropertyDescriptor`, and the one-time `Symbol`/`defineProperty`
   setup) are real JS calls, not free.** Fine for a hash that is called
   occasionally (e.g. `Map`/`Set` key computation), but this is not a
   free/inline operation the way a pointer-identity hash would be. Not
   measured yet; flagging as a possible future perf item if `GetIdentityHash`
   turns out to be hot in the wider test surface.

## Recommended next step

Point 2 in the mission (`Object`/`Array` basics, canonicalization
audit) is now unblocked to start incrementally, now that the deepest
open risk from C0/C3 has a working, tested answer: widen the surface toward
`Object::Get`/`Set`, `Array` basics, and register the 4th backend in
`tests/harness/config.json` with an empty baseline once `test_api` links, to
start hill-climbing like the other backends. Before that broad work, resolve
residual risk 2 above (audit whether any Rust-side rusty_v8 code hashes/keys
raw `Local` pointers instead of going through `GetIdentityHash`), since that
would silently reintroduce the slot-identity bug this experiment fixed.
