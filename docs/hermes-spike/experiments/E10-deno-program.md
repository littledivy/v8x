# E10 — a real Deno program runs end to end on Hermes (the capstone)

Goal (from the cycle brief): run an ACTUAL Deno program end to end on Hermes.
Not isolated probe assertions: a real user ES module, loaded and executed
through deno_core's real module loader, using the Web/Deno globals, driven by
`run_event_loop`, with its console output asserted.

## Brutal-honesty summary (read this first)

Did a genuine multi-feature Deno program run to completion and produce correct
asserted output on Hermes? **YES.**

A real user ES module (`file:///main.js`, with TOP-LEVEL AWAIT) was loaded
through deno_core's real module loader (`load_main_es_module_from_code` -> the
module is instantiated via the resolve/load path and marked main) and evaluated
via the async `mod_evaluate`, driven to completion by the real `run_event_loop`
on one shared tokio runtime. The program did, together:

- `await fetch("http://127.0.0.1:<port>/")` + `await res.text()` over a real
  loopback HTTP/1.1 server (real socket, real ext/fetch JS + ops),
- an async generator `async function* gen(){ yield 1; yield 2; yield 3; }`
  consumed with `for await (const n of gen())`,
- `new URL("https://x/y").pathname`,
- `console.log(...)` through ext/web's real `inspectArgs` formatter.

It printed exactly `BODY: hello-from-hermes SUM: 6 URL: /y`, which the probe
asserted (via a captured console buffer). `mod_evaluate` + `run_event_loop`
returned Ok in ~3ms. No fallback: this is the REAL module loader
(`load_main_es_module_from_code` + async `mod_evaluate`), not `execute_script`
of a string. The one genuine wall (top-level await in the module closure) was a
Hermes backend gap, fixed in the backend and covered by a regression test.

```
BOOT OK: JsRuntime::new (webidl+web+net+fetch) on Hermes
  bootstrap installed
  loopback HTTP server bound on 127.0.0.1:54212
  loading user module file:///main.js via module loader...
  module instantiated, id=2
  run_event_loop + mod_evaluate elapsed: 2.764292ms
  module evaluated: Ok (top-level await settled)
  --- captured program stdout ---
BODY: hello-from-hermes SUM: 6 URL: /y
  ASSERT captured output contains "BODY: hello-from-hermes SUM: 6 URL: /y": PASS

=== E10 CAPSTONE: a real Deno program on Hermes: PASS ===
```

## (1) Did a real user ES module run end to end through the module loader?

YES. The exact program (`libs/hermes_web_probe/src/bin/deno_program.rs`,
`user_program_source`):

```js
const res = await fetch("http://127.0.0.1:<port>/");
const body = await res.text();

const items = [];
async function* gen() { yield 1; yield 2; yield 3; }
for await (const n of gen()) items.push(n);
const sum = items.reduce((a, b) => a + b, 0);

const u = new URL("https://x/y");

console.log("BODY:", body, "SUM:", sum, "URL:", u.pathname);
```

How it was loaded: the REAL module loader path, not `execute_script`.
- `RuntimeOptions { module_loader: Some(Rc::new(StaticModuleLoader::default())), .. }`.
- `rt.load_main_es_module_from_code(&file:///main.js, src).await` -> the source
  is compiled to a module, instantiated through the resolve/load path, marked as
  the MAIN module, returns a `ModuleId` (id=2).
- `rt.mod_evaluate(id)` (the async variant) returns a future for the module's
  top-level-await promise; both it and `rt.run_event_loop(..)` are driven
  together. When the loop returns Ok, the top-level await has settled.

Asserted console output (captured): the program's `console.log` output equals
`BODY: hello-from-hermes SUM: 6 URL: /y`. Capture is honest: `console.log`
routes through ext/web's real `internals.inspectArgs`
(`op_console_inspect_args`, a native Rust formatter) and emits via a probe op
(`op_capture_print`) wired as the console printFunc, appended to a buffer in
OpState and asserted from Rust. (Deno's own `Console` class delegates formatting
to a cppgc-backed `ConsoleWrap`, which is unimplemented on Hermes; `inspectArgs`
is the SAME native formatting engine without cppgc. This is the honest
functional console path, established in E9.)

## (2) Capabilities the ONE program exercised together

In a single module evaluation, driven by one `run_event_loop`:

1. **fetch over real http://** — `fetch()` + `res.text()` through the real
   ext/fetch JS (`26_fetch.js`) and ops (`op_fetch` hyper connect,
   `op_fetch_send` await response), over a real loopback `tokio::net` socket
   served by an in-probe hyper HTTP/1.1 server returning `hello-from-hermes` @
   200 (the E9 path). Result: `BODY: hello-from-hermes`.
2. **async generator + for-await** — `async function* gen()` consumed with
   `for await (const n of gen())`, summed to 6 (`SUM: 6`). Rides the E1
   async-generator lowering pass, now inside a real module body.
3. **URL** — `new URL("https://x/y").pathname` === `/y` (`URL: /y`), through
   ext/web `00_url.js` (op_url_parse).
4. **console** — `console.log` formats via ext/web `inspectArgs` and emits the
   asserted line.
5. **top-level await** — the module itself uses TLA (`await fetch`, `await
   res.text()`), evaluated through the module loader's async `mod_evaluate`.

All five worked together in one real program, end to end.

## (3) Backend gap found + fix (files + commit SHA, NOT pushed)

### v8x backend — `/Users/divy/gh/v82jsc`, branch `hermes-backend-spike`, commit `f5c99ef`

Gap: **top-level await was impossible in a Hermes ES module.** Hermes/JSI has no
ES-module-record API, so v8's module semantics are modeled by wrapping each
module body in a JS closure and calling it (`src/hermes/modules.rs`). That
wrapper was a PLAIN (non-async) function:

```
(function (__imports, __exports) { <module body> })
```

A module with a top-level `await` (which every non-trivial async Deno program
has) is then a SyntaxError: the closure fails to compile, `v8x_hermes_run`
returns not-ok, `evaluate_rec` marks the module `Errored` and
`v8__Module__Evaluate` returns null. deno_core's async `mod_evaluate`
(`libs/core/modules/map.rs`) saw `module.evaluate()` return `None` with no
termination flag set, dropped the oneshot sender, and surfaced
`ExecutionTerminated` ("Cannot evaluate module, because JavaScript execution has
been terminated"). So NO real user ES module with top-level await could run
through the module loader on Hermes.

Fix (`f5c99ef`, `src/hermes/modules.rs`):
- `transform_module`: wrap the module body in an **`async function`** so a
  top-level `await` is legal syntax. Correct for sync modules too: an async
  function with no await runs its body synchronously and returns an
  already-resolved promise (exports assigned before any await point, matching
  V8's module-evaluation semantics).
- `evaluate_rec`: the async closure call returns a Promise (the module's TLA
  promise); pin it on the module record (`eval_promise_pin`).
- `v8__Module__Evaluate`: return that REAL promise instead of a fresh
  pre-resolved one, so deno_core awaits genuine TLA (pending = await in flight,
  already-resolved = synchronous module body). Synthetic modules (no closure)
  still fall back to a fresh resolved promise.
- New field `ModuleRecord::eval_promise_pin` (both constructors updated).

Regression test `hermes_module_top_level_await` (`src/hermes/mod.rs`):
compile+instantiate+evaluate a module whose body is
`__tla_before=1; await Promise.resolve(); __tla_done=42;`; assert the synchronous
prologue ran, then after a microtask drain the eval result is a real Promise in
`Fulfilled` state and the post-await assignment (`__tla_done === 42`) is visible
— proving the async-wrapped module body ran through its top-level await.
(The test does not assert a Pending intermediate state: Hermes may resume some
microtasks eagerly during the closure call, an execution-model timing detail
that does not affect correctness.)

**Backend hermes suite from a CLEAN build: 45 passed, 0 failed**
(`cargo clean -p v8x` then
`cargo test --no-default-features --features hermes,link_hermes --lib hermes::`).
Was 44 at E9; +1 is the new TLA regression test. No cppgc duplicate-symbol
artifact this cycle (the clean build ran green).

### deno checkout (scratch) — `/Users/divy/gh/deno-v8x-rebase`, branch `v8x-rebase-rc`, commit `c29836b`

New probe binary (NOT for merge, no Deno test file touched):
- `libs/hermes_web_probe/src/bin/deno_program.rs`: the capstone. Boots
  webidl+web+net+fetch on Hermes, installs the minimal global bootstrap
  (console/URL/TextEncoder/Decoder/ReadableStream/structuredClone/fetch, all
  from the real ext modules), stands up the E9 loopback HTTP server, loads the
  user module via `load_main_es_module_from_code`, evaluates via async
  `mod_evaluate` + `run_event_loop`, captures console via `op_capture_print`,
  asserts the output line.
- `libs/hermes_web_probe/Cargo.toml`: register the `deno_program` bin.

No `libs/core/` change was needed; the async `mod_evaluate` + `run_event_loop`
module-loader path in deno_core is used unmodified.

## (4) Wall hit (and cleared)

One wall, cleared: **top-level await in the module closure**. Diagnosed from the
`ExecutionTerminated` error at `mod_evaluate`, traced to `v8__Module__Evaluate`
returning null because the plain-function module closure couldn't compile a
top-level `await`. Fixed in the backend (async wrapper + return the real
promise). After the fix, the program ran to completion.

No remaining wall for THIS program. Named honest limitations that did NOT block
it but are worth knowing:
- `v8__Module__IsGraphAsync` is hardcoded `false`. deno_core only reads it in
  the SYNC `mod_evaluate_sync` path (to reject TLA there); the ASYNC
  `mod_evaluate` used here ignores it, so TLA works. If a future cycle drives
  modules through the sync path, IsGraphAsync would need to reflect real TLA.
- The module import/export rewriter (`transform_module`) is a line-based
  source-to-source transform (documented in D2), not a full ESM parser. This
  program has no `import`/`export` (it uses globals, like a real Deno script),
  so it did not exercise multi-module graphs here; that path is covered
  separately by the D2/module rusty_v8 work.
- console still uses `inspectArgs` (not the cppgc `Console` class); cppgc/Oilpan
  remains the standing unlock for the full `Console` object and `test_api`/
  `test_cppgc` (tracked in #2).

## (5) Recommended E11

The capstone is done: a real multi-feature Deno program runs end to end. The
honest next targets, in rough leverage order:

1. **A user program with real `import` between modules** through the loader:
   two ES modules (`main.js` imports `./lib.js`), loaded via a real
   `ModuleLoader` (FsModuleLoader or a StaticModuleLoader with two entries),
   asserting a value that crosses the module boundary. This exercises the D2
   multi-module resolve/instantiate/evaluate graph under a real program, and
   would surface any TLA-in-a-dependency ordering issues (V8 evaluates a TLA
   dependency's promise before the importer resumes).
2. **cppgc / Oilpan stubs** so the real `Console` class (and `test_api` /
   `test_cppgc`) link and run — the single biggest dashboard jump (#2). Then the
   program can use the genuine `Console` object rather than the inspectArgs
   shim.
3. **TLS fetch** (`fetch("https://...")` against a loopback `tokio_rustls`
   server), the E9-deferred item: the client side is ready, only the loopback
   TLS server (self-signed cert) is missing.

## (6) Disk at end

`df -h /`: **7.9Gi avail** (69% used). UP from 5.4Gi at E9 start, because
`cargo clean -p v8x` (run for the clean backend-suite count) freed 5.8Gi of
stale v8x build artifacts. `CARGO_INCREMENTAL=0` throughout; no ENOSPC, no
incremental dir created.

## Honesty ledger

- GENUINELY landed and verified end to end on Hermes: a real user ES module
  (`file:///main.js`, with top-level await) loaded through deno_core's REAL
  module loader (`load_main_es_module_from_code` + async `mod_evaluate`) and
  driven to completion by `run_event_loop`, exercising fetch-over-real-http +
  async-generator/for-await + URL + console TOGETHER, producing the asserted
  output `BODY: hello-from-hermes SUM: 6 URL: /y`. This is a Deno program
  running on Hermes.
- The one real backend fix is small and precise: wrap the modeled module-body
  closure in an `async function` and return its real (TLA) promise from
  `v8__Module__Evaluate`, unblocking top-level await for every ES module.
  Covered by a new regression test; backend suite 45/45 from a clean build.
- No fallback to `execute_script` for the program: the real module-loader path
  was used. The console capture goes through ext/web's real `inspectArgs`
  formatter (honest, cppgc-free); it is not a stubbed print.
- Nothing under `vendor/`, no Deno test file, no `libs/core/` change, no
  `report.json`/`history.jsonl` touched. Commits are NOT pushed.
