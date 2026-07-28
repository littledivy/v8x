# E5 — async event-loop drive + v8::ValueSerializer on Hermes

Goal: close the two ext/web features E4 deferred, both driven through the real
ext/web JS + deno_core (no hand-rolled shortcuts). Both landed.

1. The async event-loop drive: a real `ReadableStream` chunk read now resolves
   through the event loop (`reader.read()` -> `{value:42,done:false}`).
2. `v8::ValueSerializer`/`Deserializer` for `structuredClone`:
   `structuredClone({a:1,b:[2,3]})` deep-equals the input, clone independent.

Everything runs through booted deno_webidl + deno_web on deno_core, backend =
v8x Hermes. No op stubbed at the deno layer, no Deno test file touched. All
fixes in `src/hermes/`.

## Results (asserted, from the probe)

| target | result | asserted |
|---|---|---|
| plain `Promise.resolve().then()` settles on first state read | PASS | `state=Fulfilled value="8"` (was Pending forever) |
| ReadableStream `reader.read()` through event loop | PASS | `"value=42,done=false"` |
| structuredClone deep-equal | PASS | `{"a":1,"b":[2,3]} -> {"a":1,"b":[99,3]}` |
| structuredClone rich types (strings/bool/null/float/deep nest) | PASS | `"ok"` |

Probe: 5 passed, 0 failed (up from E4's 2 passed, 1 failed, 1 deferred).

## (1) Event-loop drive — ROOT CAUSE: a state-observation lag, not a drain gap

### Diagnostic (async-gen works vs plain-promise Pending), instrumented in one probe

The same probe compared three promise paths after driving microtasks the way
E4 did (16 `perform_microtask_checkpoint()` calls, then read `Promise::State`).
Before the fix:

```
A: setup Promise.resolve(7).then(v=>v+1) -> drain -> read : state=Pending
B: same but READ STATE FIRST -> drain -> read            : Fulfilled "8"
C: Promise.resolve(123) -> drain -> read                 : state=Pending
D (pure JS): settled promise's fresh .then reaction after
   one perform_microtask_checkpoint                      : reaction RAN
```

The teeth are in A vs B vs D:

- **D proves the backend drain is fine.** A freshly-attached `.then` reaction on
  an already-settled Hermes promise DOES run on a single
  `perform_microtask_checkpoint` (`v8x_hermes_drain_microtasks`). Native Hermes
  promise reactions drain correctly. So this is NOT a backend microtask gap
  (contradicting E4's leading hypothesis).
- **A vs B isolate the real bug.** The only difference is WHEN state is first
  read. Reading state arms a lazy WeakMap recorder (`ensureTracked` ->
  `record(p)` -> `p.then(setOnce)`); that recorder's OWN reaction only fires on
  a LATER drain. In A the first (and only) `getState` happens AFTER all drains,
  so the recorder is armed but never drained again -> reads Pending forever. In
  B the pre-drain read arms the recorder BEFORE the drains, so a following drain
  settles the WeakMap.

Root cause: `Promise::State()` lagged one microtask checkpoint behind reality.
V8's `Promise::State` reflects `[[PromiseState]]` synchronously (the read that
follows settlement observes the settled state); our model only observed it a
drain later. deno_core's async-gen path (E2, `with_event_loop_promise`) happened
to work because it reads state, THEN the event loop drains again — masking the
lag. Every await-based Web API driven by a bare state read (ReadableStream
reads, and later timers/fetch/net) hit the lag and stayed Pending.

### The fix

`v8x_hermes_promise_state` (hermes_shim.cpp) now, on a Pending result (a WeakMap
miss that just armed the recorder), drains the microtask queue once via
`v8x_hermes_drain_microtasks` and re-reads. That drain flushes BOTH our
`setImmediate` `jobs` FIFO and Hermes's native `drainMicrotasks` queue (the
freshly-attached reaction on an already-settled promise lands in the latter —
which is why an earlier attempt to drain only the JS `jobs` FIFO from inside
`getState` did NOT settle it; the drain had to go through the full C++ path).
`getState` itself stays a pure read (arm-on-miss, return 0); the C++ accessor
owns the drain + re-read. A settled read costs nothing (drain only on Pending).

With this, `Promise.resolve(7).then(v=>v+1)` reads `Fulfilled 8` on the first
state read after one checkpoint, and the real ReadableStream read settles:
`reader.read()` on a stream that was enqueued a chunk -> `{value:42,done:false}`,
asserted, through the real ext/web `06_streams.js` + deno_core event loop.

This is the foundation for all async Deno (timers, streams, fetch, net).

## (2) v8::ValueSerializer/Deserializer — implemented; structuredClone round-trips

deno_core's `op_structured_clone` drives `v8::ValueSerializer`/`Deserializer`
(`WriteHeader`, `WriteValue`, `Release`, then `ReadHeader`, `ReadValue`). On V8
those produce/consume V8's structured-clone wire format. Hermes/JSI has no native
value serialization (unlike QuickJS's `JS_WriteObject`), so E5 implements the
whole surface (25 C-ABI symbols, previously null stubs) as a self-describing
recursive walk over JSI values into a private byte format, plus a matching reader.

- `src/hermes/hermes_shim.cpp`: `v8x_hermes_structured_serialize` /
  `_deserialize` / `_sc_free` — the JSI value <-> bytes walk. Wire tags: `n`
  null, `u` undefined, `t`/`f` bool, `i` int32 / `d` f64 number, `s` string
  (len + utf8), `a` array (count + elems), `o` plain object (count + string-key
  / value pairs).
- `src/hermes/serializer.rs`: the 25 `v8__Value{Ser,Deser}ializer__*` symbols.
  Rust owns the byte buffer and the deno delegate glue (mirroring the QuickJS
  backend); `WriteValue`/`ReadValue` call the C++ walk. An uncloneable value
  makes `WriteValue` return `MaybeBool::Nothing` so the op raises a clean
  DataCloneError instead of corrupting. A 4-byte magic (`HRMV`) brackets the
  payload so `ReadHeader` validates the stream.
- `src/hermes/shims.rs`: the 25 null stubs dropped.

structuredClone asserted result: `{a:1,b:[2,3]}` deep-equals its clone and the
clone is independent (mutating `clone.b[0]=99` leaves `input.b[0]===2`), through
the real ext/web `02_structured_clone.js` recursive walk + `op_structured_clone`.
A richer graph (strings incl. `"héllo"`, booleans, null, floats, 3-deep nesting,
mixed arrays) also round-trips deep-equal and independent.

Types that round-trip: null, undefined, boolean, number (int + float), string,
array, plain string-keyed object (recursively). Types NOT covered (fail cleanly
with a DataCloneError, honest scope): BigInt, Symbol (uncloneable in V8 too),
Date, RegExp, Map, Set, TypedArray/ArrayBuffer, transferables, host objects
(MessagePort/CryptoKey), and object identity / cyclic graphs. Those are the next
serializer increments if postMessage/MessagePort/BroadcastChannel are targeted.

## (3) Backend fixes: files + commits (NOT pushed) + test count

Branch `hermes-backend-spike`, not pushed:

- `63ae403` — `Promise::State` drains once on a Pending read so JS promises
  settle (hermes_shim.cpp + mod.rs regression test).
- `3f4b93b` — v8::ValueSerializer/Deserializer for structuredClone
  (serializer.rs new, hermes_shim.cpp walk helpers, shims.rs 25 stubs dropped,
  mod.rs `mod serializer` + regression test).

Backend lib suite: **39 passed, 0 failed** (`hermes,link_hermes`); was 37 at E4.
Two new regression tests:
`promise_then_state_settles_on_first_read_after_checkpoint` and
`hermes_value_serializer_round_trip`.

Sandbox (deno checkout `v8x-rebase-rc`, NOT pushed, commit `5786bfe061`):
`libs/hermes_web_probe/src/main.rs` extended with the E5 assertions (stream read,
structuredClone x2, plain-promise event-loop foundation). 5/5 probe assertions
pass.

## (4) The single most important next wall + recommended E6 target

The event-loop foundation and structured clone are in; the next wall is the
**timer + real-op async path end to end** feeding into the network stack. The
plain-promise/stream fix settles promises that are ALREADY resolved by the time
state is read. The next test is a promise resolved LATER by a real op
completing on the event loop (a timer callback, then an op that returns a
future), i.e. `setTimeout` / `op_sleep` driven by `run_event_loop`, not just a
microtask checkpoint. That is the bridge to `for await` over a socket.

Recommended E6: bring up **ext/console + ext/url** first (small, mostly-sync,
high-confidence, they hydrate the rest of the runtime surface and console gives
real observability), then move to the **ext/net / fetch** network stack where
`for await` over sockets lives — that is where a genuinely deferred op-backed
promise (accept/read returning a future resolved by the event loop) has to
settle, exercising `run_event_loop` beyond the microtask checkpoint this cycle
proved.

## (5) Disk at end

`df -h /`: 9.6Gi avail (65% used). `CARGO_INCREMENTAL=0` throughout; no ENOSPC,
no incremental dir created.

## Honesty ledger

- Genuinely functional through real ext/web JS: the event-loop-driven
  ReadableStream read (a chunk delivered and read to `{value:42,done:false}`),
  structuredClone deep-equal + independence for primitives/strings/arrays/plain
  objects.
- The event-loop fix settles ALREADY-resolved promises observed via
  `Promise::State`. It has NOT yet been exercised with a promise resolved later
  by a real deferred op on `run_event_loop` (timers/net) — that is the E6 test
  and the honest boundary of "async works" today.
- ValueSerializer covers JSON-shaped graphs only; BigInt/Symbol/Date/Map/Set/
  TypedArray/transferables/host-objects/cycles are refused with a clean
  DataCloneError, not silently mishandled.
