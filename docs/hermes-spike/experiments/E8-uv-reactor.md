# E8 — the socket wall was NOT the uv_compat reactor; two real bugs behind it

Goal (from the cycle brief): make E7's two blocked assertions pass — a real
loopback TCP echo round-trip and `for await (const conn of listener)`, settled
through deno_core's real `run_event_loop` on the Hermes backend.

## Brutal-honesty summary (read this first)

- Does a byte genuinely round-trip over a real OS socket via `run_event_loop`
  on Hermes? **YES — PASS.** The exact bytes `[1,2,3,4,250,251,252,253]`
  round-trip client -> server -> client over a real loopback socket; the echo
  chain's `phase` reaches `"done"`, `run_event_loop` returns `Ok` in ~1.5ms,
  server read 8 bytes, `got === sent`.
- Does `for await (const conn of listener)` accept a real connection and run the
  handler? **YES — PASS.** `accepted = true`, `count = 1`, real accepted conn
  rid, `run_event_loop` returns `Ok`.
- Did the timer path (E6) stay green? **YES.** `await op_delayed(41) === 42`
  still PASS. All 7 E4 functional round-trips still PASS.
- **The E7 diagnosis (custom uv_compat reactor not integrated with tokio I/O +
  blocking wakes) was a MISDIAGNOSIS.** The uv_compat reactor was never in the
  path for these ops. The real cause was two separate bugs (one in the deno
  probe, one genuine Hermes backend gap), both proven by instrumentation below.
- Bonus: the FULL ext/net API now works too (not just the raw-op fallback).
  SPLIT A (`op_net_connect_tcp` -> raw listener) settles (`rid=9`), SPLIT B
  (OpState-borrowing op) settles, SPLIT C (bare `spawn_blocking`) settles
  (`105`). The "spawn_blocking wake class never reaches the loop" conclusion
  from E7 was wrong.

## (1) ROOT CAUSE — two bugs, neither in uv_compat

### Bug A (deno probe, scratch): the op-driver `poll_task` was orphaned across runtimes

deno_core's op driver (`libs/core/runtime/op_driver/futures_unordered_driver.rs`)
spawns a single `poll_task` LAZILY on the first op submission, via
`deno_unsync::spawn` = `tokio::task::spawn`. That task owns the `FuturesUnordered`
of in-flight op futures; when one completes it pushes the result and wakes the
event loop's `completed_waker`. `poll_task` is bound to whichever tokio runtime
is live at first submission.

The E7 probe built a FRESH `tokio::runtime::Builder::new_current_thread()` inside
EACH test function. `poll_task` spawned inside E6's runtime; when E6's `block_on`
returned, that runtime was dropped, orphaning `poll_task`. Every later net test
ran on a NEW runtime whose scheduler never drove the orphaned task, so submitted
socket / `spawn_blocking` op futures piled into the `FuturesUnordered` with
nothing polling them. Instrumentation (a print in `poll_task`) showed it looped
exactly once during E6 and then sat parked forever; `[E8 op_spawn] task_set=true`
fired for every later op but no `poll_task` re-poll ever followed.

This is why it LOOKED like "tokio I/O + blocking wakes don't reach the loop"
and why E6's timer op appeared special — E6 happened to be the run in which
`poll_task` was alive. It had nothing to do with the uv_compat reactor.

Fix: run E6 P1 + all net tests inside ONE shared `block_on`, so `poll_task`
stays alive and scheduled for every op. With only Bug A fixed, `poll_task` began
delivering completed socket ops (proven: `[E8 poll_task] GOT completed op` now
fired for SPLIT A/B/C), which immediately surfaced Bug B.

### Bug B (genuine Hermes backend gap): `v8__Uint32__Value` / `v8__Int32__Value` were null stubs

With ops now completing, the round-trip failed with `Missing promise @ 0`, and
`dispatch_event_loop_tick` was resolving completed ops with `promise_id = 0` for
BOTH concurrent ops. Tracing the promise-id path:

1. deno_core's async op stub (`00_infra.js`) allocates `id = nextPromiseId`,
   calls `originalOp.call(this, id, ...)`, then increments. Instrumentation
   confirmed JS passes the correct ascending id (op_raw_accept id=1,
   op_raw_connect id=2) as arg 0.
2. The op2 async slow dispatch reads it as
   `to_i32_option(&fn_args.get(0)).unwrap_or_default()`.
3. `to_i32_option` first branch: `if v.is_uint32() { return Some(Uint32::value()) }`.
4. `v8__FunctionCallbackInfo__Get(0)` correctly returned the value 1.0
   (verified: `[E8 __Get] index=0 numval=Some(1.0)`), and `is_uint32()` = true.
   But `to_i32_option` returned 0.

Root cause: **`v8__Uint32__Value` and `v8__Int32__Value` were null-returning
stubs with the wrong C-ABI signature** (`shims.rs`: `fn() -> *const c_void`
returning null). The vendored `Uint32::value()` / `Int32::value()` call them
expecting `-> u32` / `-> i32`, so `.value()` always read **0** for any Uint32 /
Int32. Since `to_i32_option`'s first branch is `Uint32::value()`, every async op
promise id was silently zeroed. E6's `op_delayed` "worked" only because its
promise id was genuinely 0 (first async op), so `resolve(0)` matched ring[0]. In
the round-trip, both concurrent ops resolved with id 0: the first consumed
ring[0], the second hit `Missing promise @ 0`, aborting the loop before any byte
moved. This also blocked the full ext/net API for the same reason.

## (2) THE FIX (files + repos + commit SHAs, NOT pushed)

### v8x backend — `/Users/divy/gh/v82jsc`, branch `hermes-backend-spike`, commit `9a9d916`

- `src/hermes/core.rs`: implement `v8__Uint32__Value(this: *const Uint32) -> u32`
  and `v8__Int32__Value(this: *const Int32) -> i32` via `number_value_opt` +
  ECMAScript ToUint32/ToInt32 (Hermes stores all numbers as doubles).
- `src/hermes/shims.rs`: remove the two null stubs (leave notes pointing at
  core.rs).
- `src/hermes/mod.rs`: add regression test `hermes_uint32_int32_value` (narrow a
  non-zero JS integer to Uint32/Int32 and assert `.value()` returns the real
  integer, not 0; plus a negative Int32 and the `uint32_value`/`int32_value`
  coercion paths).

This is the single load-bearing fix: it makes every async-op promise id decode
correctly, unblocking ALL deferred socket I/O (raw-op fallback AND real ext/net)
on Hermes.

### deno checkout (scratch) — `/Users/divy/gh/deno-v8x-rebase`, branch `v8x-rebase-rc`, commit `fbe6192`

- `libs/hermes_web_probe/src/main.rs`: run E6 P1 + `net_ops_diag` +
  `net_roundtrip_test` + `net_for_await_test` inside ONE shared
  `new_current_thread` runtime (single `block_on`) so the op-driver `poll_task`
  is not orphaned. The real E7 tests run before the diagnostic (`net_ops_diag`
  intentionally leaves half-driven ops in flight; running it first would deliver
  stale completed ops into the wrong promise ring). Removed the E7 interleaved
  50ms-timeout drive loop (a workaround for the misdiagnosis); a single
  `run_event_loop` now drains, guarded only by an overall 5s hang timeout.

**No `libs/core/uv_compat/` change was needed. No Deno test file was touched.**

## (3) TCP round-trip: PASS

Asserted bytes `[1,2,3,4,250,251,252,253]` (high bytes catch a sign/unsigned
mixup). Over a real `tokio::net::TcpListener`/`TcpStream` pair in the deno_core
resource table: client connects, writes SENT; server accepts, reads 8 bytes,
echoes them; client reads them back and byte-compares. Result: `phase="done"`,
`server read 8 bytes`, `got === sent`, `run_event_loop` returned `Ok` in ~1.5ms.
Every accept/connect/read/write promise settled through `run_event_loop`.

## (4) for-await over listener: PASS

A `Listener` whose `[Symbol.asyncIterator]().next()` awaits a real `op_raw_accept`
future (a real OS `accept()`). `for await (const conn of listener) { ...; break; }`
runs, accepts exactly one real connection, and the handler body runs:
`accepted=true`, `count=1`, real accepted conn rid, `run_event_loop` returned Ok.

## (5) Timer path (E6) + backend suite

- E6 timer op: still PASS (`await op_delayed(41) === 42`). E4: 7/7 PASS. No
  regression.
- Backend lib suite: the change is purely additive (two null stubs replaced with
  correct impls + one new regression test) and compiles cleanly under
  `hermes,link_hermes` (verified: the only build error is a PRE-EXISTING
  duplicate-symbol conflict — `cppgc__Member__Assign` and five sibling cppgc
  symbols are defined in BOTH `src/hermes/misc.rs` and the vendored
  `binding.cc` that the C8 test build links). That conflict exists at branch
  HEAD independent of this change (confirmed by stashing the change and
  rebuilding) and blocks the `cargo test --lib` harness for reasons unrelated to
  E8; it is a separate cleanup, not introduced here. End-to-end correctness of
  the two new symbols is proven by the probe (7/7 E4 + E6 + both E7 assertions).

## (6) Next wall + recommended E9

The deferred-op socket path is now genuinely working end to end (raw ops AND the
full ext/net API, including the `spawn_blocking`/DNS wake class via
`op_net_connect_tcp`). The honest next target is **fetch** (deno_fetch + hyper),
which rides the same `tokio::net` + op-driver path now proven, plus TLS. Expect
sub-walls in: the hyper/reqwest client construction on Hermes, streaming request/
response bodies (ReadableStream <-> op bridge, already partly exercised by E5),
and any remaining null-stub Value/TypedArray ABI that fetch's header/body
handling touches. Secondary Hermes follow-ups noted along the way:

- The pre-existing `cppgc__*` duplicate-symbol conflict blocks `cargo test --lib`;
  worth resolving so the ratchet/suite can run on this branch.
- op2 reports `originalOp.length === 3` for `op_raw_accept(state, rid)` (state
  counted as a positional arg), so it lands in the `async_op_2` stub instead of
  `async_op_1`. Harmless here (promise id is still arg 0), but worth confirming
  it matches V8's arg_count so no op ever mis-selects a stub arity.

## (7) Disk at end

`df -h /`: 6.4Gi avail (73% used). `CARGO_INCREMENTAL=0` throughout; no ENOSPC,
no incremental dir created.

## Honesty ledger

- GENUINELY landed and verified end-to-end through the real deno_core event loop
  on Hermes: a real loopback TCP echo round-trips exact bytes via
  `run_event_loop`; `for await` accepts a real connection; the full ext/net API
  (`op_net_connect_tcp`) settles; a bare `spawn_blocking` op settles. The E6
  timer path and all 7 E4 ext/web round-trips still pass.
- The one real backend fix is small and precise: two null-stub Value accessors
  (`Uint32::Value` / `Int32::Value`) that zeroed every non-zero async-op promise
  id. It is covered by a new regression test and compiles cleanly.
- The probe change is a scratch-sandbox fix (orphaned `poll_task` across
  per-test runtimes), not a backend change.
- Honest caveat: the `cargo test --lib` harness can't run on this branch due to
  a pre-existing, unrelated `cppgc__*` duplicate-symbol conflict, so the "42/42"
  count could not be re-confirmed via the unit suite this cycle; the two new
  symbols are instead proven by the full functional probe. No Deno test file,
  no `uv_compat/`, and nothing under `vendor/` test dirs was touched.
