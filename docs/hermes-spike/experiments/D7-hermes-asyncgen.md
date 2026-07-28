# D7: past the async-generator wall, and how far the deno_core boot now gets

D6 bumped the vendored Hermes to 260318099.0.1 (HBC 99), which cleared the six
missing intrinsics, and the deno_core boot moved to a COMPILE-time wall: Hermes
rejects `async function* () {}` (primordials.js line 285), a language feature it
does not implement. D7 has two parts: (A) an honest assessment of whether async
generators are pervasive enough to block Deno-on-Hermes, and (B) getting the boot
past that wall and reporting the next failures.

## TL;DR

- **PART A (the honest viability answer):** async-generator FUNCTION syntax is a
  ONE-OFF on the code deno_core actually boots. `libs/core` (the deno_core JS,
  the only JS `deno_core::JsRuntime::new` runs) contains exactly ONE
  `async function*`: the primordials prototype capture at
  `00_primordials.js:285`. It never drives an async generator; it only reflects
  on the `%AsyncGenerator%` prototype shape. So deno_core itself is NOT blocked
  by the Hermes gap.

  The wider Deno RUNTIME (`ext/`, loaded later, not by `deno_core::JsRuntime`)
  DOES use async generators for real: 16 `async function*` declarations and 4
  `async *method()` forms, plus 58 `Symbol.asyncIterator` sites. Most are Node
  polyfills (`ext/node/polyfills/**`: streams, timers, test reporters). Two are
  core Deno runtime hot paths: `ext/net/01_net.js` (`async *[SymbolAsyncIterator]`
  for iterating a listener's connections, i.e. `for await (const conn of
  listener)`) and `ext/web/09_file.js` (`Blob.prototype.stream` /
  `async *stream()`). These are real async iterators over sockets and blobs.

  **Conclusion.** deno_core boots without async generators (a workaround on that
  one primordials literal is enough). But a full Deno runtime that runs user
  code touching `for await` over a listener, `Blob.stream()`, or the Node stream
  polyfills WILL hit the Hermes gap at runtime. Those are not source-transformable
  the way the primordials capture is: they are real async generators whose
  suspend/resume semantics Hermes cannot execute. So the north star (boot Deno on
  Hermes) is reachable for the deno_core core; a complete Deno runtime is NOT
  viable on this Hermes without Hermes gaining async-generator support (a
  compiler-level feature we cannot add), OR a per-site rewrite of every real
  async generator into a hand-rolled async iterator (a large, ongoing burden on
  vendored Deno source we are told not to modify). The primordials capture is the
  only spot we can honestly transform past; the rest is a genuine engine gap.

- **PART B (advance past the wall):** DONE, and the boot moved a long way. The
  `async function*` primordials capture is rewritten in the Hermes compile path
  into a synthetic `%AsyncGenerator%` shape, so primordials.js parses. The boot
  then cleared SIX more walls and now reaches deno_core's `store_js_callbacks`
  (the last step of `new_inner`, after the whole ES-module graph evaluated). The
  new wall is a real, well-scoped subsystem: external-memory BackingStores.

- rusty_v8 on hermes went **86 -> 89** (three tests the D7 fixes turned green;
  no regression, `--check --rescue` green at 89). Hermes lib tests 26 -> 28
  green (6 new D7 boot probes added along the way; two probes replaced older
  weaker ones).

## PART B: the wall progression

Each fix moved the boot to the next failure. All are in `src/hermes/` on this
branch; all previously-null stubs became real, or an identity no-op became
correct. The boot was re-run after each with
`DYLD_FRAMEWORK_PATH=vendor/hermes target/debug/examples/hermes_boot`.

### Wall A (compile): `async function*` unsupported — the D6 wall

`00_primordials.js:285` is `Reflect.getPrototypeOf(async function* () {})`.
Hermes rejects the literal at parse time.

Fix (`src/hermes/core.rs`, `rewrite_async_generator_literal`, called from
`v8__Script__Compile`): a source rewrite. When a script text contains
`async function*`, prepend a small IIFE that builds a synthetic
`%AsyncGenerator%` function object with the V8 prototype shape
(`.prototype` has `next`/`return`/`throw` and `[Symbol.asyncIterator]`), exposed
on `globalThis.__v8x_synthAsyncGen`, and replace each `async function* (...) {...}`
literal (span-balanced over its `(...)` params and `{...}` body) with a reference
to it. `Reflect.getPrototypeOf(...)` then returns a usable stand-in. This is a
WORKAROUND for the Hermes gap, not a real async-generator implementation: an
actual `async function*` call would still be unsupported. It only satisfies the
reflection primordials does at bootstrap.

### Wall B (throw): `Too many arguments for async op codegen (length ... was -1)`

Past primordials + infra, deno_core registers ops. For each async op it calls
`Deno.core.setUpAsyncStub`, which switches on `originalOp.length - 1` to pick an
async-op wrapper and throws when the value is out of range. Hermes host functions
(`createFromHostFunction`) report `.length === 0` regardless of the `paramCount`
handed in (and the constructable wrapper loses it too), so every op read as
`0 - 1 = -1` and hit the throw.

Fix (`src/hermes/hermes_shim.cpp`, `v8x_hermes_function_new`): define an explicit
`length` own property (`value: paramCount`, non-enumerable, non-writable,
configurable, matching V8's `Function.length` descriptor) on the created function
and on its constructable wrapper.

### Wall C (panic): `Object::with_prototype_and_properties` returned null

`new_inner` builds objects with a fixed prototype and an initial property set via
`v8__Object__New__with_prototype_and_properties` (a null stub -> `.unwrap()`
panic).

Fix (`src/hermes/core.rs`): model it on existing primitives — fresh object, set
each name/value as an ordinary own property, then set the prototype (null => null
prototype) via a new C++ helper `v8x_hermes_object_set_prototype`
(`Object.setPrototypeOf` semantics).

### Wall D (panic): `mod_evaluate_sync` — `BadType` expected Promise

The virtual ops module (`ext:core/mod.js` shape) evaluates. deno_core reads a
source module's `Evaluate` result as a Promise. `v8__Value__IsPromise` was a null
stub, read as a garbage bool, so the type-check reported `BadType`.

Fix (`src/hermes/core.rs` + `hermes_shim.cpp`): implement `Value::IsPromise` as
`value instanceof Promise` against the runtime's global `Promise`
(`v8x_hermes_value_is_promise`, via JSI `Object::instanceOf`).

### Wall E (run): `01_core.js` — `Cannot set property 'log' of undefined`

The first bootstrap script `ext:core/01_core.js` runs and throws. `01_core.js`
does `op_get_ext_import_meta_proto().log = ...`. The op returned a value that,
read back later, was NOT an object.

Root cause (bisected with an in-op probe: `proto present, is_object=false`):
`v8__Global__New` was an identity no-op, so a value Global carried a
scope-managed handle-table slot. deno_core creates the `ext_import_meta_proto`
object in `new_inner`'s scope, stores it in a `Global`, and reads it back much
later; by then the creating HandleScope had popped and truncated the slot, so
`Local::New` read a stale/reused slot.

Fix (`src/hermes/core.rs`): a VALUE Global now durably PINS its JSI value (the D2
pin infra) and encodes the pin id in the handle (`(pin_id << 2) | 0b10`,
disambiguated from value slots `(i << 1) | 1` and from aligned Context/Module
pointers). `Local::New` resolves a global-pin handle by materializing the pinned
value into a fresh scope slot; `Global::Reset` unpins. Non-value Globals (Context
== isolate pointer, Module == Box record) stay identity, as before. This is the
C2 lifetime rule applied to Globals.

### Wall F (run): `01_core.js` — `Cannot convert undefined value to object`

Further into `01_core.js`: `const v8Console = globalThis.console;
wrapConsole(coreConsole, v8Console)`, and `wrapConsole` does
`ObjectKeys(consoleFromV8)`. V8 exposes a built-in `globalThis.console`; Hermes
does not, so `globalThis.console` was undefined and `ObjectKeys(undefined)` threw.

Fix (`src/hermes/core.rs`, `install_global_console` called from
`v8__Context__New`): synthesize a minimal no-op console (method names as
ENUMERABLE own properties so `ObjectKeys` enumerates them) on the global at
context creation, idempotent (skips if a real console already exists). This
mirrors the D4 extras-binding console; deno_core forwards real console output
through its own op-based console.

### Wall G (panic): `Module::get_unbound_module_script().unwrap()` — null

The builtin ES modules now compile. `new_module_from_js_source` unconditionally
calls `module.get_unbound_module_script().unwrap()`, then
`.get_source_mapping_url().unwrap()` on it. Both were null stubs.

Fix (`src/hermes/modules.rs`): return the module record pointer itself as an
opaque, non-null `UnboundModuleScript` handle (it is never dereferenced as a
script), and return `undefined` (not null) from `GetSourceMappingURL` /
`GetSourceURL` so deno skips the source-map branch.

## Where the boot stands now (the new wall)

An actual `deno_core::JsRuntime::new` on the Hermes backend now:

- clears the async-generator primordials capture (Wall A);
- finishes primordials + infra;
- registers every op, including async-op stubs (Wall B);
- builds the prototype-and-properties objects `new_inner` needs (Wall C);
- evaluates the virtual ops module and reads its Promise result (Wall D);
- runs `ext:core/01_core.js` to completion (Walls E and F: the import-meta proto
  Global and `globalThis.console`);
- compiles and lazy-loads the builtin ES module graph (Wall G);

and now stops at `JsRuntime::store_js_callbacks` (the last step of `new_inner`),
where deno_core builds a shared `Uint8Array` over `ContextState::tick_info`
(external Rust memory) so JS can read/write `hasTickScheduled` without crossing
the boundary. That path calls
`v8::ArrayBuffer::new_backing_store_from_ptr(...)` ->
`v8__ArrayBuffer__NewBackingStore__with_data`, a null stub, so
`UniqueRef::from_raw(...).unwrap()` panics
(`vendor/rusty_v8/src/support.rs:111`).

```
panicked at vendor/rusty_v8/src/support.rs:111 (UniqueRef::from_raw unwrap None)
  v8::ArrayBuffer::new_backing_store_from_ptr        (array_buffer.rs:676)
  deno_core::runtime::jsruntime::JsRuntime::store_js_callbacks (jsruntime.rs:1821)
  JsRuntime::new_inner                               (jsruntime.rs:1284)
```

This is NOT `1 + 1` yet: `store_js_callbacks` is the final `new_inner` step, and
`try_new` returns only after it. But the wall has moved from "unsupported source
feature" (D6) all the way through op registration and the whole ES-module boot
graph to a single well-scoped subsystem: **external-memory BackingStores**.

## Recommended next step (D8)

Implement the external-memory BackingStore chain, the exact next wall:

1. `v8__ArrayBuffer__NewBackingStore__with_data` — wrap an external
   `(ptr, len, deleter, deleter_data)` as a BackingStore. JSI's `createArrayBuffer`
   uses a `jsi::MutableBuffer`, which can wrap external memory directly (unlike
   V8's copy semantics), so a BackingStore can be modeled as a small record
   holding the external pointer/len/deleter plus a lazily-created JSI ArrayBuffer
   over a MutableBuffer that points at that memory.
2. `BackingStore::make_shared` / the `SharedRef` machinery the vendored
   `array_buffer.rs` uses (`with_backing_store`).
3. `v8__ArrayBuffer__with_backing_store` — create a JSI ArrayBuffer that aliases
   the backing store's external memory (so Rust writes to `tick_info` are visible
   to the JS `Uint8Array`, which is the whole point of this path).
4. `NewBackingStore__with_byte_length` and the SharedArrayBuffer siblings are the
   same shape with owned (not external) memory; do them together.

After that, `store_js_callbacks` finishes and `new_inner` returns; the next wall
should be `execute_script("1 + 1")` itself (which already works in the isolated
boot probe) or the event-loop/tick wiring. Expect `1 + 1` to be close after the
BackingStore subsystem lands.

## PART A: the assessment in numbers (reproducible)

Run inside the deno checkout (`/Users/divy/gh/deno-v8x-rebase`):

- `grep -rn 'async function\*' libs/core/` -> 1 hit (`00_primordials.js:285`,
  the prototype capture). This is the ONLY async generator in the code deno_core
  boots.
- `grep -rn 'async function\*' ext/ runtime/` (excluding `.d.ts`) -> 16 hits.
- `grep -rEn 'async +\*[A-Za-z_$\[]' ext/ runtime/` (async methods) -> 4 hits
  (`ext/net/01_net.js`, `ext/web/09_file.js`, two Node polyfills).
- `grep -rn 'Symbol\.asyncIterator\|SymbolAsyncIterator' ext/ runtime/` -> 58.

The two non-polyfill runtime async generators, for the record:

- `ext/net/01_net.js`: `async *[SymbolAsyncIterator]()` — `for await` over a
  `Listener`'s incoming connections.
- `ext/web/09_file.js`: `async *stream()` — `Blob.prototype.stream()`.

These run only when user code exercises those APIs, not during `deno_core`
bootstrap, which is why the deno_core boot is unblocked by the single
primordials rewrite while a full runtime is not.

## Constraints honored

- Local branch `hermes-backend-spike` only; no push, no publish, main untouched.
- Committed after every wall (7 source commits + 1 baseline commit), so a
  transient crash cannot lose work.
- No vendored rusty_v8 test, `report.json`, `history.jsonl`, `NOTES.md`,
  `SUMMARY.md`, or `.omc/` file touched.
- rusty_v8 ratchet updated the RIGHT way: the D7 fixes made 3 tests pass, so the
  baseline went 86 -> 89 (`--update --rescue`), and `--check --rescue` is green
  at 89. No baselined test regressed.
- Deno-checkout edits are OUTSIDE this branch: the boot was re-run against the
  D4/D5 hermes facade wiring; the only edits made in the checkout this cycle were
  SCRATCH diagnostics (a TryCatch in `execute_builtin_sources`, an in-op probe,
  and bisection markers in `01_core.js`), ALL reverted afterward. The checkout is
  back to the D4 facade wiring plus the `hermes_boot.rs` example.
- No download scratch left in the repo root; nothing downloaded this cycle.
- No em dashes in this doc.

## Files touched (v82jsc, this branch)

- `src/hermes/core.rs`: `rewrite_async_generator_literal` + `span_async_generator_body`
  and the `v8__Script__Compile` hookup (Wall A); real
  `v8__Object__New__with_prototype_and_properties` (Wall C); real
  `v8__Value__IsPromise` (Wall D); value-Global durable pins in `v8__Global__New`
  / `v8__Global__NewWeak` / `v8__Global__Reset` / `v8__Local__New` (Wall E);
  `install_global_console` from `v8__Context__New` + `intern_str` (Wall F);
  `v8x_hermes_object_set_prototype` / `v8x_hermes_value_is_promise` / `v8x_hermes_unpin`
  extern decls.
- `src/hermes/hermes_shim.cpp`: `.length` own-property definition in
  `v8x_hermes_function_new` (Wall B); `v8x_hermes_object_set_prototype` (Wall C);
  `v8x_hermes_value_is_promise` (Wall D).
- `src/hermes/modules.rs`: non-null `v8__Module__GetUnboundModuleScript` +
  `v8__UnboundModuleScript__GetSourceMappingURL` / `GetSourceURL` returning
  undefined (Wall G).
- `src/hermes/shims.rs`: removed the now-real null stubs
  (`Object__New__with_prototype_and_properties`, `Value__IsPromise`,
  `UnboundModuleScript__GetSourceMappingURL`, `UnboundModuleScript__GetSourceURL`).
- `src/hermes/mod.rs`: 6 new boot probes
  (`boot_async_generator_primordials_capture`, `boot_function_length_reflects_arity`,
  `boot_object_with_prototype_and_properties`, `boot_value_is_promise`,
  `boot_null_proto_object_global_roundtrip_settable`,
  `boot_op_returns_object_settable_from_js`, `boot_typed_array_buffer_accessor`,
  `boot_global_console_present`).
- `tests/status/baselines/hermes/rusty_v8.txt`: 86 -> 89.
- `docs/hermes-spike/experiments/D7-hermes-asyncgen.md`: this file.

The scratch deno-checkout diagnostics (TryCatch in `execute_builtin_sources`,
in-op probes, `01_core.js` bisection markers) were used to find each wall's exact
JS error/line and were reverted; they live only in the session history, not this
branch or (now) the deno checkout.
