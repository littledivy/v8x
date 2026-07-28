# E7 — ext/net + `for await` over a socket on Hermes (the climax cycle)

Goal: prove a real loopback TCP round-trip AND `for await` over a listener, on
the Hermes backend, through deno_core's real `run_event_loop`. Two backend/
lowering walls fell; one deeper event-loop wall in the DENO CHECKOUT (not
Hermes) blocks the end-to-end socket I/O and is the honest stopping point.

## Brutal-honesty summary (read this first)

- Does a byte genuinely round-trip over a real OS socket via `run_event_loop`
  on Hermes? **NO — BLOCKED**, but not by Hermes and not by anything E0-E6 was
  about. The wall is the deno checkout's custom event loop: a genuinely-pending
  async op future (a `tokio::net` socket accept/connect, or a `spawn_blocking`)
  spawned into deno_core's op-driver `poll_task` **never gets re-polled** through
  `run_event_loop`, while a **timer** future does. So socket ops (and ext/net,
  and my raw-op fallback, both `tokio::net`-backed) never complete.
- Does `for await` genuinely accept a real connection? **NO — BLOCKED** for the
  same reason: the async-iterator's `next()` awaits a real `op_raw_accept`
  future that never re-polls. The `for await` construct itself parses, runs, and
  suspends correctly on Hermes (proven — it reaches and awaits the accept op);
  it just never gets a connection because the accept future never wakes.
- What DID land: two genuine fixes that were REAL prerequisites, plus a precise
  root-cause isolation of the remaining wall (with a reproducing probe).

## Results (asserted / observed, from the probe)

| target | result | evidence |
|---|---|---|
| ext/net (`deno_net`) builds + boots on Hermes | PASS | crate compiles; `deno_net::deno_net::init(None,None)` + allow-all PermissionsContainer loads |
| ext/net `01_net.js` loads + classes construct on Hermes | PASS (after fix) | `new Listener(...)`/`validatePort` work; was blocked by the lowering bug below |
| `op_net_listen_tcp` (sync) | PASS | returns a valid rid + `{hostname,port}` |
| `op_net_accept_tcp` / `op_net_connect_tcp` dispatch | PASS | return real promises, no synchronous throw |
| accept/connect promises SETTLE through `run_event_loop` | **BLOCKED** | never resolve/reject; loop never returns (parked) |
| raw tokio socket round-trip on the SAME runtime (no deno_core) | PASS | `Ok(Ok("ping"))` — a byte round-trips over a real socket |
| raw `spawn_blocking` on the SAME runtime (no deno_core) | PASS | `Ok(42)` |
| genuinely-pending socket/spawn_blocking op via `run_event_loop` | **BLOCKED** | SPLIT A/B/C all `None` |
| timer op via `run_event_loop` (E6 carryover) | PASS | `await op_delayed(41) === 42` |
| backend lib suite | 42 passed, 0 failed | `hermes,link_hermes` (was 41 at E6) |

## (1) TCP round-trip: BLOCKED (deno-checkout event loop, NOT Hermes)

I wired the real `deno_net` (ext/net) into the probe crate:
`deno_net.workspace = true` + `deno_permissions` + `sys_traits`, extension
`deno_net::deno_net::init(None, None)`, and an allow-all `PermissionsContainer`
seated in `OpState` (the exact pattern ext/net's own `#[test]` modules use).
`deno_net` compiled cleanly (quinn/rustls/tls/tunnel and all), booted, and its
ops dispatched.

The full ext/net JS choreography (`net.listen` -> `listener.accept()` +
`net.connect` -> `conn.read`/`conn.write` echo) got as far as: listener binds a
real ephemeral port; `op_net_accept_tcp(rid)` and `op_net_connect_tcp(...)` both
dispatch and return real promises. Then, driven only by `run_event_loop` on a
tokio current-thread runtime, **neither promise ever settles**, and
`run_event_loop` never returns (it parks). Even connecting to a DEAD port does
not reject.

### Root cause (isolated with a reproducing probe, corroborated by an architect pass)

deno_core (this checkout, `libs/core`) does not poll async op futures inline in
`run_event_loop`. It pushes each pending op into a `FuturesUnordered` owned by a
SEPARATE task spawned via `deno_unsync::spawn` (= `tokio::spawn`), and the event
loop only DRAINS already-completed results
(`op_driver/futures_unordered_driver.rs`, `poll_ready` in `jsruntime.rs`). So an
op future makes progress only if that spawned `poll_task` is re-polled when its
future's waker fires.

I split the waker classes with four probe ops dispatched the same way as the
proven-working `op_delayed`:

| probe op | what it awaits | settles through `run_event_loop`? |
|---|---|---|
| `op_delayed` (E6) | `tokio::time::sleep` (TIMER) | YES |
| `op_probe_tcp_connect` | `TcpStream::connect` to a bad addr | YES — but only because it errors on the FIRST inline poll (never goes Pending) |
| `op_raw_accept` / `op_raw_connect` | real `tokio::net` accept/connect that genuinely go Pending | NO |
| `op_probe_spawn_blocking` | bare `tokio::task::spawn_blocking(...).await` | NO |

Controls on the SAME runtime, NOT through `run_event_loop`: a raw
`tokio::net` listener+client byte round-trip returns `Ok("ping")`, and a raw
`spawn_blocking` returns `Ok(42)`. So the tokio runtime's I/O + blocking-pool
wakes work; they just do not reach the op-driver's `poll_task` when the loop is
driving.

Conclusion: **timer wakes reach `poll_task`; kqueue-readiness and blocking-pool
wakes do NOT** when `run_event_loop` is parked. This checkout drives I/O through
a custom libuv-based reactor (`libs/core/uv_compat`, `libs/core/reactor.rs`
explicitly notes `uv_compat` "talks to tokio directly"); the op-driver's
`tokio::spawn`'d `poll_task` and its `tokio::net`/`spawn_blocking` wakes are not
serviced while the loop parks on that custom reactor. This is a deno-checkout
event-loop / reactor-integration issue, NOT a Hermes backend gap, and NOT the
`for await`/async-op-bridge class E0-E6 were about (that bridge — op -> promise
-> `__eventLoopTick` -> resolve — is proven and still works for timer-backed
deferred ops).

I confirmed the wall is not worked around by interleaving short
`run_event_loop` passes with explicit `tokio::task::yield_now()` +
`sleep(1ms)` (to give the scheduler turns): the socket ops still never complete.

### Did I use real deno_net or the raw-op fallback?

Both were attempted. Real `deno_net`: wired and booting, but its ops hang at the
event-loop wall above (and every ext/net op additionally routes through
`resolve_addr`/`lookup_host` = `spawn_blocking`, which is exactly the blocked
wake class). Raw-op fallback (kqueue-only `tokio::net` sockets in the deno_core
resource table, no DNS/Happy-Eyeballs): built and dispatching, but it hits the
SAME event-loop wall because it is also `tokio::net`-backed and its
accept/connect futures genuinely go Pending. So a byte does NOT round-trip
through `run_event_loop` on either path. Raw tokio sockets DO round-trip a byte
on the same runtime OUTSIDE `run_event_loop` (the control), which is the honest
floor of what works.

## (2) `for await` over a listener: BLOCKED (same event-loop wall)

I built a real async-iterable listener whose `[Symbol.asyncIterator]().next()`
awaits a real `op_raw_accept` future (a real OS `accept()` in the resource
table), and drove `for await (const conn of listener) { ...; break; }` through
`run_event_loop`. The construct RUNS on Hermes: it reaches the `for await`, calls
`next()`, dispatches `op_raw_accept`, and suspends awaiting it (the probe shows
the accept op dispatched and the async-iteration suspended). It never yields a
connection because the accept future never re-polls (the (1) wall). So the
literal original ceiling — async-iteration OVER a socket — is no longer a
Hermes/`for await` limitation (that machinery works); it is blocked purely by
the socket op never completing through this event loop.

## (3) deno_net build: it DID build (the fix that let its JS load)

`deno_net` compiled and booted. The wall that first stopped it was NOT a build
error but a Hermes LOWERING bug surfaced by loading `01_net.js`:

`new Listener(...)` threw `TypeError: undefined is not a function` at the ctor.
Root cause: the Hermes CompileFunction async-generator lowering
(`src/hermes/lower.rs`) targeted `ESTarget::ES2017`, which enables EVERY
downlevel pass at/below ES2017 — including the ES2022 class-properties /
private-field pass. `01_net.js` has classes with `#rid` private fields AND an
`async *[Symbol.asyncIterator]()`; because the file contained an async
generator, the whole unit went through oxc's transform, which rewrote the
private fields into `babelHelpers.classPrivateFieldInitSpec(...)` /
`classPrivateFieldSet2(...)` — helpers our four-helper `BABEL_HELPERS` object
does NOT provide. So the ctor's first statement was `undefined(...)`.

This is Hermes-specific (it only bites because Hermes needs the async-generator
lowering; V8 never runs the transform). It is NOT the deno_napi-style E0282
non-Hermes class. Fixed in the backend (below).

The `#[smi]` op-argument observation: raw ops declared with `#[smi] rid` /
`#[smi] port` received `0` instead of the passed integer on the Hermes op2
dispatch (a listener rid of 1 arrived as 0; a real port arrived as 0, so every
connect hit EADDRNOTAVAIL on `127.0.0.1:0`). Declaring the args as plain `u32`
(no `#[smi]`) decoded correctly. This is a likely real Hermes op2 fast-call/smi
arg-decode gap, noted for a follow-up cycle; it did not need fixing to reach the
event-loop wall, since plain-`u32` args are a clean workaround in the probe.

## (4) Backend fixes: files + commit SHAs (NOT pushed) + test count

Branch `hermes-backend-spike`, NOT pushed:

- `50509dd` — `src/hermes/lower.rs`: lower ONLY async generators, not private
  fields / other ES2022. Build all-off `TransformOptions::default()` and enable
  only `env.es2018.async_generator_functions`, instead of
  `TransformOptions::from(ESTarget::ES2017)` (which also enabled the
  class-properties pass that rewrote `#x` into missing babel helpers). Private
  fields, class properties, object spread, optional chaining etc. now stay
  NATIVE (Hermes supports them); only the async-generator declaration syntax is
  downleveled. This is what let ext/net's `01_net.js` classes construct.
  Regression test: `private_fields_survive_alongside_async_generator`.

Backend lib suite: **42 passed, 0 failed** (`hermes,link_hermes`); was 41 at E6.
New regression test: `private_fields_survive_alongside_async_generator`.

Sandbox (deno checkout `v8x-rebase-rc`, NOT pushed): `libs/hermes_web_probe`
extended — `deno_net`/`deno_permissions`/`sys_traits` deps + allow-all perms
extension; the E7 net round-trip + `for await` tests (raw-op fallback); and the
`net_ops_diag` / SPLIT A/B/C / CONTROL probes that isolate the event-loop wall.
Reverted: no Deno test file touched; no ext/net op stubbed.

## (5) Next wall + recommended E8

The single wall now is the deno-checkout event loop: `tokio::net` (kqueue) and
`spawn_blocking` op-future wakes do not reach deno_core's `poll_task` while
`run_event_loop` parks on the custom libuv reactor. Recommended E8: fix that
integration IN THE DENO CHECKOUT (it is scratch, not a test file) — ensure the
op-driver's spawned `poll_task` is serviced by the same reactor the loop parks
on (chain the event-loop park to tokio's I/O + blocking unpark, OR drive
`poll_task` from inside the custom loop's I/O phase). That single fix unblocks
BOTH the full ext/net API and the raw-op fallback simultaneously, at which point
E7's round-trip + `for await` should complete unchanged. Only after that is
fetch (E8's originally-suggested target) reachable, since fetch also rides
`tokio::net`/hyper through the same event loop. Secondary Hermes follow-up: the
`#[smi]` op2 arg-decode gap noted in (3).

## (6) Disk at end

`df -h /`: 4.3Gi avail (81% used) after adding the full `deno_net` dep tree
(quinn/rustls/hickory/tunnel). `CARGO_INCREMENTAL=0` throughout; no ENOSPC.
Watch disk on E8 — the checkout's `target/debug` grew ~5Gi this cycle.

## Honesty ledger

- GENUINELY landed and verified: the lowering fix (private fields survive the
  async-generator pass) — without it ext/net's JS classes cannot even
  construct on Hermes; the ext/net crate building + booting on Hermes; ext/net's
  sync `op_net_listen_tcp` returning a real listener; ext/net's async ops
  dispatching to real promises. Backend suite 42/42.
- NOT achieved (honest, no overclaim): a byte does NOT round-trip over a real OS
  socket through `run_event_loop` on Hermes, and `for await` does NOT accept a
  real connection, because the socket op futures never complete through this
  checkout's event loop. The raw-op fallback did NOT rescue this — it is
  `tokio::net`-backed and hits the identical wall. What genuinely round-trips a
  byte / runs `spawn_blocking` is RAW tokio on the same runtime OUTSIDE
  `run_event_loop` (the control), which proves the OS/runtime can do it and
  localises the defect to the deno-checkout event-loop/reactor integration —
  NOT to Hermes, and NOT to the op->promise->loop bridge E6 proved.
- The `for await` CONSTRUCT itself works on Hermes (it parses, runs, calls the
  async iterator's `next()`, and suspends on the real accept op); only the op's
  completion is blocked. So the overnight spike's original "Hermes can't do
  `for await` over a socket" ceiling is dismantled at the language/engine level;
  the residual blocker is a runtime-integration bug in the scratch deno
  checkout, not the engine.
