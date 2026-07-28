# E4 — ext/web Web APIs made FUNCTIONAL on Hermes (Private + TypedArray ABI)

Goal: turn E3's hydrated-but-not-functional ext/web into actually functional
Web APIs on Hermes, proven with asserted round-trips through booted deno_core +
deno_web. E4 closed the two E3 walls (`v8::Private`, the TypedArray/ArrayBuffer
op-marshalling ABI) plus a third that surfaced behind them (`v8::Symbol`), and
lands two functional Web APIs end-to-end. Two further targets (structuredClone,
a real stream read) hit genuinely large new subsystems and are named as E5.

Everything runs through the real ext/web JS (`Deno.core.loadExtScript(...)`),
booted deno_webidl + deno_web on deno_core, backend = v8x Hermes. No op was
stubbed at the deno layer; no Deno test file was touched. All fixes are in
`src/hermes/`.

## Results (asserted, from the probe)

| target | result | asserted string |
|---|---|---|
| v8::Private / thrown error formats | PASS | `TypeError: boom-from-extweb\n    at anonymous (v8x.js:1:29)\n    at global (v8x.js:1:53)` |
| TextEncoder/TextDecoder round-trip | PASS | `"héllo (bytes=6)"` |
| structuredClone deep-equal | BLOCKED (E5) | throws `Failed to serialize response` |
| ReadableStream read | DEFERRED (E5) | read() promise stays `Pending` |

`new TextDecoder().decode(new TextEncoder().encode("héllo"))` === `"héllo"`,
asserted, all the way through `op_encoding_encode_into` + `op_encoding_decode`
over the real ext/web `08_text_encoding.js`.

## (1) v8::Private — implemented; thrown ext/web errors now format, not panic

A `v8::Private` is a hidden Symbol used as a non-enumerable property key.
deno_core's error machinery (`error.rs`) brands thrown Error objects with
callsite metadata via `Object::set_private`/`get_private`, and the formatter
reads it back, so the E3 null stubs made every thrown ext/web error re-panic
during formatting (`private.rs:58`).

Model: a Private is an ordinary JS Symbol held in a handle-table slot.
`ForApi(name)` interns one Symbol per name in a per-runtime `name -> Symbol`
registry (V8's global-registry contract); `New` is a fresh unmemoized Symbol.
Symbol-keyed get/set/has/delete use cached JS bracket-notation helpers
(`(o,s) => o[s]`, etc.) because JSI's `jsi::Object` accessors are string-/
PropNameID-keyed only. Commit `67491c1`:

- `src/hermes/hermes_shim.cpp`: `v8x_hermes_private_{new,for_api,name}` +
  `object_{get,set,has,delete}_private`, backed by `private_registry` +
  `ensure_private_infra` (cached helpers), placed after `slot_ref` so it is in
  scope. panic-safe (null/-1 on any C++ throw).
- `src/hermes/core.rs`: real `v8__Private__{New,ForApi,Name}` +
  `v8__Object__{Get,Set,Has,Delete}Private`.
- `src/hermes/shims.rs`: seven null stubs dropped.
- regression test `hermes_private` (ForApi interned by name, set/get/has/delete
  round-trip, New distinct from same-named ForApi, key hidden from
  `Object.keys`, `Name` returns the description).

### The teeth behind Private: v8::Symbol also had to land

Once Private was in, an UNGUARDED throw still panicked, one layer up, at
`symbol.rs`. The formatter uses `Symbol::for_key` (== `Symbol.for(key)`, the
JS-visible global registry) to brand error-additional-properties, and
`v8__Symbol__{New,For,ForApi,Description}` were null stubs. Commit `c61a865`
implements them (`New`/`Description` reuse the Private infra's cached helpers;
`For` routes through real JS `Symbol.for`; `ForApi` routes through the private,
JS-inaccessible registry per V8's contract). With this, an unguarded
`throw new TypeError('boom-from-extweb')` returns a clean deno_core JS error
(message + stack), no panic — the whole error-reporting path through ext/web
now works.

## (2) TextEncoder/TextDecoder — the TypedArray/ArrayBuffer op ABI

`op_encoding_encode_into` (and `encode`/`decode`) marshal a JS Uint8Array into a
Rust slice by checking `IsArrayBufferView`/`IsUint8Array`, then reading the
view's `ByteOffset`/`ByteLength` and its backing ArrayBuffer. That whole
predicate + `ArrayBufferView__*` surface was null-stubbed, so the op rejected
every JS-created typed array with `expected ArrayBuffer or ArrayBufferView`.

The vendored Hermes JSI has first-class TypedArray support (`isTypedArray`,
`isUint8Array`, `byteOffset`/`byteLength`, `buffer`, `ArrayBuffer::data`), so
these route straight through it, no JS driving. Commit `dbacb3f`:

- `src/hermes/hermes_shim.cpp`: `v8x_hermes_value_is_{typed_array,uint8_array,
  array_buffer,array_buffer_view}` + `typed_array_{byte_offset,byte_length,
  buffer,data,copy_contents}`. `array_buffer_view` == `typed_array` (JSI has no
  DataView predicate; nothing in the current surface makes a DataView over the
  op boundary).
- `src/hermes/core.rs`: real `v8__Value__Is{ArrayBuffer,ArrayBufferView,
  TypedArray,Uint8Array}` + `v8__ArrayBufferView__{Buffer,Buffer__Data,
  ByteLength,ByteOffset,HasBuffer,CopyContents}`.
  Load-bearing detail: the vendored `ArrayBufferView::data()` is
  `Buffer__Data + ByteOffset`, so `Buffer__Data` returns the buffer's base
  (subtracting byteOffset back off the JSI `data()+offset`) to avoid
  double-counting the offset.
- `src/hermes/shims.rs`: ten null stubs dropped.
- regression test `hermes_typed_array_abi` (predicates classify correctly,
  bytes read back exactly, a non-zero-byteOffset `subarray(3,6)` reads the right
  window, `data()` pointer arithmetic lands on the first byte).

`v8__ArrayBufferView__GetContents` (the MemorySpan variant) is left stubbed; the
encoding ops use `IsArrayBufferView` + `ByteOffset`/`ByteLength` +
`GetBackingStore`/`Data`, not `GetContents`, and the PASS proves the path taken.

## (3) structuredClone — E5 wall: v8::ValueSerializer

The ext/web JS logic RUNS: `02_structured_clone.js`'s recursive walk executes,
and the base case delegates to `core.structuredClone` == `op_structured_clone`.
That op uses `v8::ValueSerializer`/`ValueDeserializer` (V8's structured-clone
wire format, with delegate callbacks) — 25 null-stub C-ABI symbols on Hermes
(`v8__Value{Ser,Deser}ializer__*`). So it fails at `Failed to serialize
response`. structuredClone is genuinely blocked on the ValueSerializer
subsystem, which is self-contained and sizable. This is the recommended E5
target.

## (4) ReadableStream read — E5 wall: microtask-checkpoint -> native promise drain

The stream constructs and `getReader()`/`read()` return a real promise (it
reaches the `Pending` state). But the read never settles, and the cause is NOT
streams-specific: a plain `Promise.resolve(7).then(v => v+1).then(...)` also
stays `Pending` after 16 explicit `perform_microtask_checkpoint()` calls (and
after `run_event_loop`). So `perform_microtask_checkpoint` ->
`v8x_hermes_drain_microtasks` is not running Hermes's native `Promise.prototype
.then` reactions through this entry path. (The D1/E2 async-generator work
succeeded via deno_core's `with_event_loop_promise`, a different, op-promise
path.) Wiring the microtask checkpoint to drain native Hermes promise jobs is
the blocker for any await-based Web API; deferred to E5.

**Correction / attribution note:** commit `c61a865` (nominally "implement
v8::Symbol") also contains a `globalThis.queueMicrotask` shim in
`ensure_promise_infra` (`hermes_shim.cpp`, the `setImmediate`/`drainJobs` setup
block) that was not written by the primary executor for this cycle — it landed
via a concurrent process sharing the same working tree mid-cycle and was swept
into that commit by an unqualified `git add` without review. On inspection it
is legitimate: Hermes has no native `queueMicrotask` global, `ReadableStream`'s
chunk-delivery path (`chunkStepsMicrotask`) calls it, and without a shim that
call is a no-op so the read's reaction never queues. It shares the same `jobs`
FIFO as `setImmediate`/`drainJobs`, so one drain flushes both. Verified after
discovery: does not regress the backend suite (still 37/37) and does not, by
itself, resolve the E5 wall above (retested; the plain
`Promise.resolve().then()` case still stays `Pending`, so the drain gap is
upstream of `queueMicrotask` too). Left in place since it is correct, tested,
and directly relevant groundwork for E5 — flagged here because it was not
reviewed by the person whose name is on that commit before this note.

## (5) Backend fixes: files + commits (NOT pushed) + test count

Branch `hermes-backend-spike`, not pushed:

- `67491c1` — v8::Private subsystem (hermes_shim.cpp, core.rs, shims.rs, mod.rs).
- `dbacb3f` — TypedArray/ArrayBufferView ABI (same four files).
- `c61a865` — v8::Symbol New/For/ForApi/Description (hermes_shim.cpp, core.rs,
  shims.rs).

Backend lib suite: **37 passed, 0 failed** (`hermes,link_hermes`); was 35 at
E3. Two new regression tests: `hermes_private`, `hermes_typed_array_abi`.

Sandbox (deno checkout `v8x-rebase-rc`, NOT pushed): `libs/hermes_web_probe/
src/main.rs` extended with the four E4 assertions.

## (6) The single most important next wall + recommended E5 target

Two are tied; pick by leverage:

- **ValueSerializer/ValueDeserializer** (structuredClone, and postMessage /
  MessagePort / BroadcastChannel all use it). Self-contained: implement V8's
  structured-clone wire format over JSI values with the delegate callbacks.
  Unblocks a whole family of ext/web APIs at once. **Recommended E5.**
- **Microtask checkpoint -> native Hermes promise drain.** Smaller in surface
  but blocks EVERY await-based Web API (streams, FileReader, fetch later). Make
  `v8x_hermes_drain_microtasks` (called from `PerformMicrotaskCheckpoint`)
  actually run native `Promise.then` reactions. Arguably do this FIRST since it
  gates so much, then ValueSerializer.

## (7) Disk at end

`df -h /`: 8.1Gi avail (68% used). `CARGO_INCREMENTAL=0` throughout; the stray
`target/debug/incremental` was removed once at the start. No ENOSPC.

## Honesty ledger: functional vs still stubbed

- Genuinely functional through real ext/web JS: thrown-error formatting
  (Private + Symbol), TextEncoder/TextDecoder round-trip (TypedArray ABI).
- Runs the ext JS but blocked at the op: structuredClone (ValueSerializer stub).
- Constructs but cannot complete an async read: ReadableStream (native-promise
  microtask drain not wired).
- Left stubbed on purpose: `v8__ArrayBufferView__GetContents`,
  `v8__Data__IsPrivate`, the 25 ValueSer/Deserializer symbols, the well-known
  `Symbol__Get*` intrinsics — none on the E4 functional path.
