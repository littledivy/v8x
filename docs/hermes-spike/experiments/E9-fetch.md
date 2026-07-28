# E9 — real fetch() over http:// through ext/fetch on Hermes

Goal (from the cycle brief): get `fetch()` working over real HTTP on Hermes,
asserted end to end. Wire deno_fetch into the probe, stand up a loopback HTTP
server in the probe, and drive `const r = await fetch("http://127.0.0.1:<port>/")`
+ `await r.text()` through the real deno_core `run_event_loop` on ONE shared
tokio runtime, asserting body + status.

## Brutal-honesty summary (read this first)

Does a real HTTP response body genuinely come back through `fetch()` over a real
socket on Hermes? **YES.** The exact body `"hello-from-hermes"` and status `200`
come back through the real ext/fetch JS (`26_fetch.js`) and its native ops
(`op_fetch` = hyper connect, `op_fetch_send` = await response), over a real
loopback `tokio::net` socket, drained via `r.text()` (the response body
ReadableStream), driven by deno_core's real `run_event_loop`. `run_event_loop`
returned `Ok` in ~3.7ms. No shortcut: the request went through deno_fetch's real
JS + ops. This is proven by the two intermediate walls I had to clear inside the
real fetch path (a bare-global `URL` reference in `mainFetch`, then a
`ByteString` ABI gap in `op_fetch`'s argument marshaling) before it passed.

## (1) fetch over http://: PASS

```
=== E9: fetch() over real http:// through ext/fetch ===
  loopback HTTP server bound on 127.0.0.1:52643
  after dispatch: phase="fetching"
  run_event_loop elapsed: 3.650959ms
  run_event_loop: Ok (returned)
  phase  = "done"
  status = 200
  body   = "hello-from-hermes"
  FETCH over http://: PASS -> body "hello-from-hermes" status 200 via real ext/fetch + run_event_loop
```

Asserted in the probe: `body === "hello-from-hermes" && r.status === 200`.

Path exercised, end to end:
1. JS calls the real `fetch` from `ext:deno_fetch/26_fetch.js`.
2. `mainFetch` -> `httpRedirectFetch` builds the request, calls `op_fetch`
   (native, `#[op2(stack_trace)]`): parses method/url/headers, creates the
   hyper client (default `Gai` DNS resolver), returns a request rid.
3. `op_fetch_send(rid)` (native `async`): awaits the hyper response future — the
   exact tokio/op-driver deferred-op path E8 proved. Returns status + a body rid.
4. `23_response.js` wraps the body rid as a `ReadableStream`
   (`readableStreamForRid`), and `r.text()` drains it via deno_core stream reads
   over the real socket — the streams+socket path proven in E5/E7.
5. The loopback hyper HTTP/1.1 server in the probe (a real
   `tokio::net::TcpListener` + `hyper::server::conn::http1` on the SAME shared
   runtime) accepts the connection and returns `200 hello-from-hermes`.

Everything ran inside ONE `block_on` on ONE shared `new_current_thread` tokio
runtime (E8's lesson: a fresh per-test runtime orphans the op-driver's spawned
`poll_task`; here it stays alive so hyper's I/O completions re-poll it).

No regression: E4 7/7, E5 plain-promise, E6 deferred-op, E7 TCP round-trip +
`for await` all still PASS in the same run.

## (2) https / TLS: DEFERRED to E10 (named, with reason and plan)

Not attempted this cycle. deno_fetch already supports TLS
(`Options.unsafely_ignore_certificate_errors`, `deno_tls` builds and links, and
`tokio-rustls`/`rustls` are already compiled in the workspace dep tree), so the
CLIENT side is ready. The missing piece is a loopback TLS SERVER in the probe:
it needs a self-signed cert+key, and `rcgen` is NOT in the workspace (`grep
rcgen Cargo.toml` = 0). Adding a cert-gen dep or checking in a static
self-signed cert/key pair is a clean, contained task but out of scope for the
plaintext goal, which is firmly PASS. Deferring avoids a risky build/complexity
chase with disk at 5.4G.

**E10 TLS plan (concrete):** stand up a `tokio_rustls::TlsAcceptor` loopback
server (self-signed cert via a checked-in PEM pair, or add `rcgen` to the
probe), init deno_fetch with `unsafely_ignore_certificate_errors:
Some(vec!["127.0.0.1".into()])` (or seat a `RootCertStoreProvider` with the
self-signed cert), and assert `fetch("https://127.0.0.1:<port>/")` returns the
same body/status. The op path is identical to E9's; only the transport differs,
and the rustls stack is already built.

## (3) Backend gaps found + fix (files + commit SHAs, NOT pushed)

### v8x backend — `/Users/divy/gh/v82jsc`, branch `hermes-backend-spike`, commit `c060d6e`

Gap: **the Latin-1 (one-byte) String read ABI was three null stubs.** serde_v8's
`ByteString` deserialization (`libs/serde_v8/magic/bytestring.rs`) gates on
`v8str.contains_only_onebyte()` then copies via `write_one_byte_v2()`. op_fetch
takes `#[scoped] method: ByteString` and `#[scoped] headers: Vec<(ByteString,
ByteString)>`. In the Hermes shim, `v8__String__ContainsOnlyOneByte`,
`v8__String__IsOneByte`, and `v8__String__WriteOneByte_v2` were
`fn() -> *const c_void` null stubs (wrong C-ABI signature). So
`contains_only_onebyte()` returned false even for pure-ASCII `"GET"`, and
ByteString decode threw `TypeError: invalid type, expected: latin1` from inside
`op_fetch` — blocking all of ext/fetch (and, generally, every op taking a
ByteString arg).

Fix (`c060d6e`):
- `src/hermes/core.rs`: implement the three on top of the existing `read_string`
  (`v8x_hermes_value_to_utf8`) path.
  - `v8__String__ContainsOnlyOneByte(this) -> bool` / `v8__String__IsOneByte`:
    true iff every code point <= 0xFF (Hermes does not expose an internal
    one-byte flag, so the semantic "contains only one-byte chars" answer is the
    correct one for the fast-path callers).
  - `v8__String__WriteOneByte_v2(this, isolate, offset, length, buffer, flags)`:
    copy UTF-16 code units from `offset`, low byte each (Latin-1). `-> ()` to
    match the vendored binding; callers size the buffer via `Length()` first.
- `src/hermes/shims.rs`: remove the three null stubs (notes point at core.rs).
- `src/hermes/mod.rs`: regression test `hermes_string_onebyte_read` — asserts
  `"GET"` reports one-byte and copies `b"GET"`; a high-latin1 string
  (`0xE9 0xFF`) copies exactly (guards a sign/mask bug); and `a + U+2603`
  (SNOWMAN) reports NOT one-byte (must not falsely claim latin1).

Backend suite from a clean build: **44 passed, 0 failed**
(`cargo test --no-default-features --features hermes,link_hermes --lib hermes::`).
Was 43 at E8; +1 is the new regression test. The suite ran clean with no
cppgc duplicate-symbol artifact this cycle.

### deno checkout (scratch) — `/Users/divy/gh/deno-v8x-rebase`, branch `v8x-rebase-rc`, commit `6a46bcb`

Probe (`libs/hermes_web_probe/`) extended, NOT for merge, no Deno test file
touched:
- `Cargo.toml`: add `deno_fetch`, `deno_tls` (build dep of deno_fetch) +
  `hyper`/`hyper-util`/`http-body-util`/`bytes` for the loopback server.
- `src/main.rs`:
  - `deno_fetch::deno_fetch::init(Options::default())` in the extension set.
  - `fetch_http_test`: a loopback hyper HTTP/1.1 server returning
    `hello-from-hermes` at 200, and the `fetch()` + `.text()` drive/assert, run
    inside the shared runtime's single `block_on`.
  - Two things the real runtime bootstrap does that this bare probe had to do
    itself before fetch runs: (a) install ext/web's `URL`/`URLSearchParams` on
    `globalThis` (26_fetch.js references bare-global `URL` in `mainFetch`, it
    never imports it); (b) pre-seat `internals.__telemetry` /
    `internals.__telemetryUtil` stubs with `TRACING_ENABLED:false` so fetch
    never loads `deno_telemetry` (avoids the whole opentelemetry dep tree — all
    telemetry use in 26_fetch.js is gated on `TRACING_ENABLED`).

## (4) Walls hit inside the real fetch path (each proving it was genuine)

1. `ReferenceError: Property 'URL' doesn't exist at ?anon_2_mainFetch
   (v8x.js:396)` — 26_fetch.js uses bare-global `URL`. Fixed in the probe by
   installing ext/web's URL on globalThis (what the real bootstrap does). This
   is a probe wiring gap, NOT a Hermes engine gap.
2. `TypeError: Invalid type, expected: latin1 at op_fetch` — the genuine Hermes
   backend gap in (3). Fixed in the backend.

After both, fetch passed. Both walls are inside deno_fetch's real JS/op path,
which is the honest proof that the fetch is real (not a shortcut).

## (5) Recommended E10

TLS fetch: `fetch("https://127.0.0.1:<port>/")` against a loopback
`tokio_rustls` server with `unsafely_ignore_certificate_errors` (plan in (2)).
The op path is identical to E9's PASS; only the transport differs and the rustls
stack is already compiled. Secondary follow-ups still open from E7/E8:
- The `#[smi]` op2 arg-decode gap (E7 (3)): raw ops declared `#[smi] rid` /
  `#[smi] port` received 0; plain `u32` decodes correctly. Worth confirming the
  op2 fast-call/smi arg path so no op mis-decodes an SMI arg.
- The pre-existing `cppgc__*` duplicate-symbol conflict noted at E8 did NOT
  recur this cycle (the lib suite ran clean, 44/44); still worth a permanent
  fix so it never blocks the ratchet.
- A POST fetch with a request body (extractBody -> op_fetch `has_body`/`data`
  or a streamed body via op_pipe) would exercise the request-body ByteString +
  Uint8Array path further; E9 only covered a GET.

## (6) Disk at end

`df -h /`: 5.4Gi avail (77% used). `CARGO_INCREMENTAL=0` throughout; no ENOSPC,
no incremental dir created. deno_fetch's hyper/http stack added ~1Gi to the
checkout's `target/debug` (from 6.4Gi to 5.4Gi free across the cycle). Watch
disk on E10 if TLS server-side crates are added.

## Honesty ledger

- GENUINELY landed and verified end to end through the real deno_core
  `run_event_loop` on Hermes: `fetch("http://127.0.0.1:<port>/")` returns a real
  HTTP response whose body is `"hello-from-hermes"` and status is `200`, through
  the real ext/fetch JS (`26_fetch.js`) and its native ops (`op_fetch`,
  `op_fetch_send`), over a real loopback `tokio::net` socket, with the body read
  via `r.text()` off the response ReadableStream. `run_event_loop` returns Ok in
  ~3.7ms.
- The one real backend fix is small and precise: three null-stub Latin-1 String
  read accessors (`ContainsOnlyOneByte` / `IsOneByte` / `WriteOneByte_v2`) that
  made every `ByteString` op arg throw `ExpectedLatin1`. Covered by a new
  regression test; backend suite 44/44 from a clean build.
- The probe changes are scratch-sandbox wiring (deno_fetch init + a loopback
  hyper server + the two bootstrap steps a bare probe must do: install `URL`,
  stub telemetry). No Deno test file, nothing under `vendor/`, and no
  `uv_compat/` was touched.
- Not attempted (honest, no overclaim): https/TLS. The client side is ready; the
  loopback TLS server (self-signed cert) is the only missing piece, deferred to
  E10 with a concrete plan.
