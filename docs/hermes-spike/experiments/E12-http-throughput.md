# E12 — a REAL HTTP req/s number for a JS server on Hermes, and the honest comparison

E11 could not produce an HTTP req/s number: a real JS HTTP server on Hermes
reached boot/bind/accept/read then hit a "backend wall on the WRITE path", named
as an "op2 write-buffer marshaling gap" (`#[buffer] &[u8]` throwing
`expected i32`). E12's job was to fix that gap and load-test the server.

## Headline

1. **The E11 "write-buffer marshaling gap" DOES NOT EXIST.** It was a
   misdiagnosis. The write op marshals a `Uint8Array` fine. The real blocker was
   a **Hermes async-frame defect** that made the connection id `undefined` at
   write time, plus a **wrong op choice** (`writeSync` is unsupported on TCP).
2. A real JS HTTP/1.1 server now **round-trips genuine requests on Hermes**
   (curl + ApacheBench, 30000 requests, 0 failed).
3. Real load-test numbers are below. On a like-for-like (connection-close)
   trivial handler, **debug** Hermes does ~9.2k rps vs ~45k for release
   QuickJS/V8. The compute handler exposes the interpreter gap as expected.

BRUTAL HONESTY up front: the Hermes server runs in a **debug** build (the
release build hits a separate, pre-existing embedder-data assertion during
`JsRuntime::try_new`, see §5), so its rps is a **conservative floor**, not a
tuned number, and the compute-handler comparison against release QuickJS/V8 is
not apples-to-apples on optimization level. The trivial (IO-shaped) comparison
is the meaningful one and is reported with that caveat stated.

## (1) The write-buffer backend gap: root cause + fix

E11 reported: `op_write_sync(connRid, RESPONSE)` throws
`TypeError: expected i32 typeof=object` on the same `Uint8Array` that `op_read`
accepted, and attributed it to `#[buffer] &[u8]` (write-direction) marshaling.

**That is not what happens.** Isolated with a diagnostic probe
(`libs/hermes_web_probe/src/bin/http_bench.rs`, then reduced to
`src/bin/async_capture.rs`), the sequence is:

```
diag2: before-read connRid=1 typeof=number     # id valid before the inner await
diag3: after-read  connRid=undefined ...        # id is UNDEFINED after it resumes
diagA: after Promise.resolve(1) connRid=undefined localCapture=1
```

`localCapture` (a `const` declared inside the async body) survives; `connRid`
(a `const` captured from the enclosing accept-loop) does not. So
`op_write_sync(undefined, RESPONSE)` runs, and `#[smi] rid` decode throws
`expected i32` on `undefined`. The buffer was never the problem.

### The actual bug (Hermes engine, reproduced with ZERO deno_core)

A loop-scoped `const`/`let` bound AFTER an `await` in the loop body, then
CAPTURED by a detached async closure that itself awaits, reads back `undefined`
after the inner closure resumes — for every iteration except the last live one.
Characterization (`src/hermes/mod.rs::hermes_async_capture`, pure Hermes,
microtask-pumped, no sockets, no serde_v8, no op layer):

| variant | inner closure | outcome |
|---|---|---|
| captures loop const, inner awaits | closes over `rid` | `7:undefined \| 8:8` (BUG) |
| captures loop const, inner does NOT await | closes over `rid` | `7:7 \| 8:8` (ok) |
| takes `rid` as an ARGUMENT, inner awaits | no capture | `7:7 \| 8:8` (ok) |

The defect is in the vendored Hermes async/generator frame handling for
loop-scoped captured bindings, not in `src/hermes/` or deno_core. We cannot
patch the vendored framework in this cycle.

### The fix (realized in the backend + the server)

Do NOT capture per-connection state across an await: **pass the connection id as
a function argument** to a top-level handler. Argument slots live in the
callee's own frame, which Hermes preserves correctly across `await`. The E12
server uses `async function handleConn(connRid, RESPONSE)` called as
`handleConn(connRid, RESPONSE)` from the accept loop.

A second, independent bug fell out once the id was valid: E11's `core.writeSync`
(`op_write_sync`) **is unsupported on TCP streams**. `TcpStreamResource`
(`ext/net/io.rs`) implements only async `read`/`write`; `Resource::write_sync`'s
default returns `not_supported()`. So `op_write_sync` on a conn ALWAYS errors.
E11 chose the sync write specifically to dodge the async op's "promise_id
expected i32" — but that too was the same loop-capture bug making the id
undefined, not a promise-id marshaling gap. With the id passed by argument, the
async `core.write` (`op_write`) works. The server uses async `core.write`.

**Files / commits (NOT pushed):**
- `v82jsc` (branch `hermes-backend-spike`): `src/hermes/mod.rs` — new test module
  `hermes_async_capture` (2 tests) pinning both the engine defect and the
  argument-passing workaround. No `v8__*` C-ABI change; test-only.
  Commit SHA: `4045cd2091a885098bc64d2818e0062844e0b715` (branch `hermes-backend-spike`, not pushed).
- `deno-v8x-rebase` (branch `v8x-rebase-rc`): server switched to arg-passing +
  async write in `libs/hermes_web_probe/src/bin/http_bench.rs`; new
  `src/bin/http_server.rs` (standalone long-lived server for external load
  testing) and `src/bin/async_capture.rs` (the minimal isolation probe).

### Backend suite from a clean build

`cargo clean -p v8x` then
`cargo test --no-default-features --features hermes,link_hermes --lib hermes::`:

```
test result: ok. 51 passed; 0 failed; 0 ignored; 0 measured; 13 filtered out
```

51/51 (was 49; +2 are the new `hermes_async_capture` regression tests). No
regression; no cppgc dup-symbol error from the clean build.

## (2) Does one HTTP request genuinely round-trip on Hermes? YES

`http_bench` (in-process warm request + assert):

```
BOOT OK: JsRuntime::new (webidl+web+net+fetch) on Hermes
  js server listening (raw ops) on 127.0.0.1:39117 rid=0
  warm request OK, body contains "hello world"
  server-side __served counter = 2001
=== E11 HTTP THROUGHPUT (JS server on Hermes): PASS ===
```

Standalone server + real curl:

```
$ curl -s -i http://127.0.0.1:39120/
HTTP/1.1 200 OK
content-type: text/plain
content-length: 11

hello world
```

And ApacheBench: 30000 requests, **0 failed**. These are real HTTP round trips
through a JS accept loop (`op_net_listen_tcp` + `op_net_accept_tcp` +
`core.read` + `core.write`) over real OS sockets, driven by deno_core's
`run_event_loop` on one current-thread tokio runtime. This is a genuine JS HTTP
server, NOT `Deno.serve` (which does not exist on Hermes) and NOT a proxy.

## (3) Real load-test numbers (same machine, ApacheBench 2.3, conc=50)

`oha`/`wrk`/`bombardier`/`hey` are NOT installed on this box; `ab`
(ApacheBench 2.3, `/usr/sbin/ab`) is what was used.

Two comparison modes matter because the Hermes server closes each connection
(no keep-alive), while `Deno.serve` (QuickJS/V8) keeps connections alive:

**Trivial handler (fixed body), like-for-like connection-close (`ab` without `-k`):**

| engine | build | rps | p50 | p99 | failed |
|---|---|---|---|---|---|
| Hermes (JS accept loop) | debug | 9236 | 5 ms | 7 ms | 0 |
| QuickJS (`Deno.serve`) | release | 45406 | 1 ms | 3 ms | 0 |
| V8 (`Deno.serve`) | release | 42903 | 1 ms | 3 ms | 0 |

**Compute handler (fib(24)+JSON per request), connection-close:**

| engine | build | rps | p50 | p99 | failed |
|---|---|---|---|---|---|
| Hermes | debug | 213 | 239 ms | 247 ms | 0 (n=3000) |
| QuickJS | release | 394 | 79 ms | 294 ms | 0 |
| V8 | release | 3774 | 8 ms | 22 ms | 0 |

For reference, the `Deno.serve` keep-alive ceiling on this box (its native path,
`ab -k`): QuickJS 71077 rps, V8 188863 rps. QuickJS matches BENCHMARKS.md's
~72k; this V8 (2.9.2 canary) is far above the ~72k in the older run.

Notes that make these honest:
- QuickJS and V8 both **converge** on the connection-close trivial handler
  (45.4k vs 42.9k, within noise). That reproduces BENCHMARKS.md's key insight:
  once you are connection/socket bound, the engine is nearly invisible.
- Hermes at 9.2k is ~4.7x below that convergence point. Measuring rps vs
  concurrency (10/50/100 -> 8.2k/8.0k/8.7k) shows it is **flat**, i.e. NOT
  socket-bound: it is bound by the single-threaded `run_event_loop` +
  per-op cost in a **debug** build with a hand-rolled JS accept loop (vs
  `Deno.serve`'s native-Rust hyper server). This is not the clean
  "engine-invisible ~72k" result; the confounders (debug build, JS-side HTTP
  framing, no keep-alive, no native hyper) dominate and are stated plainly.
- The compute-handler Hermes number is debug-crippled (fib(24) in the
  unoptimized interpreter per request) and must not be read as the engine's
  real compute cost. E11's already-measured pure-JS compute proxy (Hermes 225 /
  QuickJS 422 / V8 4060 rps) is the faithful compute comparison; it shows the
  same interpreter-vs-JIT gap without the debug penalty.

## (4) Honest takeaway

- **IO-bound HTTP:** Hermes CAN serve real HTTP requests correctly. Whether it
  lands at the engine-invisible ceiling (~45k connection-close / ~72k keep-alive)
  is NOT demonstrated here, because the Hermes path is a debug build with a
  JS-side accept loop, not a release build with `Deno.serve`'s native hyper
  server. What is demonstrated: correctness (0 failed / 30k), and that the
  Hermes throughput is loop-driver bound (flat vs concurrency), not engine
  bound. A fair "is it ~72k" test needs (a) the release build fixed (§5) and
  (b) a `Deno.serve` equivalent on Hermes (native hyper), neither of which
  exists yet.
- **Compute handler:** the interpreter gap is real and large (V8's JIT is
  ~10-18x ahead), exactly as E11's compute proxy and the CPU table show. This is
  where a Hermes-backed Deno would lose on per-request JS work.
- The E12 net result is a **correctness** milestone (real JS HTTP round trip on
  Hermes, with the true root cause found and worked around) plus an honest
  statement that a tuned throughput comparison is blocked on the release-build
  fix.

## (5) Reproduction commands

```bash
# --- backend regression tests + clean-build suite count (v82jsc) ---
cd /Users/divy/gh/v82jsc
export DYLD_FRAMEWORK_PATH=/Users/divy/gh/v82jsc/vendor/hermes
CARGO_INCREMENTAL=0 cargo test --no-default-features \
  --features hermes,link_hermes --lib hermes::hermes_async_capture -- --nocapture
cargo clean -p v8x && CARGO_INCREMENTAL=0 cargo test --no-default-features \
  --features hermes,link_hermes --lib hermes:: -- --test-threads=1   # 51 passed

# --- one real round trip (in-process warm request + assert) ---
cd /Users/divy/gh/deno-v8x-rebase
export DYLD_FRAMEWORK_PATH=/Users/divy/gh/v82jsc/vendor/hermes
CARGO_INCREMENTAL=0 cargo build -p hermes_web_probe --bin http_bench
./target/debug/http_bench      # prints "warm request OK ... PASS"

# --- isolate the async-frame bug with no sockets ---
CARGO_INCREMENTAL=0 cargo build -p hermes_web_probe --bin async_capture
./target/debug/async_capture
#   __out1 = before=123 after=123           (single op-await: ok)
#   __out2 = before=7 after=undefined|before=8 after=8   (loop-capture BUG)

# --- standalone server + load test (Hermes, debug) ---
CARGO_INCREMENTAL=0 cargo build -p hermes_web_probe --bin http_server
./target/debug/http_server trivial 40001 &         # or: compute 40011
curl -s -i http://127.0.0.1:40001/                 # HTTP/1.1 200 hello world
ab -n 30000 -c 50 http://127.0.0.1:40001/          # rps / p50 / p99

# --- same-machine QuickJS + V8 (Deno.serve), connection-close ---
cat > /tmp/e12bench/trivial.js  # Deno.serve returning "hello world" (see repo)
cat > /tmp/e12bench/compute.js  # + fib(24)+JSON per request
~/deno-quickjs/deno run -A /tmp/e12bench/trivial.js 40002 &  # QuickJS 2.9.3
~/.deno/bin/deno         run -A /tmp/e12bench/trivial.js 40003 &  # V8 2.9.2
ab -n 30000 -c 50 http://127.0.0.1:40002/          # (add -k for keep-alive ceiling)
```

Handler JS used (both engines), reproduced verbatim:

```js
// trivial.js
Deno.serve({ port: Number(Deno.args[0]||39130), hostname: "127.0.0.1" },
  (_r) => new Response("hello world", { headers: { "content-type": "text/plain" } }));

// compute.js
function fib(n){ return n < 2 ? n : fib(n-1) + fib(n-2); }
Deno.serve({ port: Number(Deno.args[0]||39131), hostname: "127.0.0.1" }, (_r) => {
  const v = fib(24);
  const _ = JSON.parse(JSON.stringify({ fib: v, items: [1,2,3,{a:v}] })).fib;
  return new Response("hello world", { headers: { "content-type": "text/plain" } });
});
```

## (6) Known follow-ups (not fixed this cycle)

- **Release-build Hermes panics during `JsRuntime::try_new`** at
  `vendor/rusty_v8/src/context.rs:199` (`assert! GetNumberOfEmbedderDataFields
  > ANNEX_SLOT`) via `set_aligned_pointer_in_embedder_data`. Debug is fine.
  This is a pre-existing release-only path (E1-E11 all ran via `cargo test`,
  i.e. debug), likely a UB/aliasing issue the optimizer exposes in the Hermes
  context/embedder-data shim. It blocks a fair release-build throughput number
  and is the single most valuable next fix for the HTTP story.
- **Op error VALUES are lost on Hermes** (a thrown op error marshals to
  `undefined`: `String(e)==="undefined"`, no `.message`/`.constructor`). This
  masked the real `write_sync` "not supported" error for a long time and is the
  same class as E11's `Deno.listen` "opaque undefined" wrapper failure. Worth
  fixing so op errors are debuggable and `Deno.listen`'s JS wrapper works.
- A `Deno.serve` equivalent (native hyper server) on Hermes would let the
  trivial handler actually test the "engine-invisible ceiling" claim.
