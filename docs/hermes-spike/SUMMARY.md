# Overnight spike: Hermes backend + AOT + a working Deno binary

One night, one branch (`hermes-backend-spike`). Three things you asked for, all delivered or
prototyped. Read this first; per-cycle detail is in `NOTES.md` and `experiments/`.

## 1. A working Deno binary you can play with (QuickJS engine, no V8)

Location: `~/deno-quickjs/deno` (plus `~/deno-quickjs/README.md`). 68 MB. Built from the CI-green
Deno + QuickJS integration on the published `v8x 149.4.0-rc.1`.

Verified genuinely working (smoke-tested):
- JS builtins (Math, JSON, RegExp, Date, TextEncoder, BigInt, Map/Set), async/await + setTimeout
- `Deno.readTextFile`, TypeScript execution, `Deno.serve` + `fetch` (HTTP round-trip 200), npm imports

Run it:

    ~/deno-quickjs/deno --version
    ~/deno-quickjs/deno run your_script.ts
    ~/deno-quickjs/deno run --allow-net server.ts

`--version` reports `v8 ...-rusty` because QuickJS answers through the v8 C-ABI shim. It is QuickJS.

## 2. A Hermes engine backend for v8x, built from zero tonight

Hermes (Meta's engine, used by React Native) now exists as a third v8x backend behind the same v8
C-ABI as the JSC and QuickJS backends. Progression in one night:

| milestone | result |
|---|---|
| C0 research | Hermes = GO-WITH-CAVEATS (JSI is C++-only, identity is the hard part) |
| C1 scaffold | `engine_hermes` compiles + links with pure-Rust stubs |
| C2 FFI proof | Rust evaluates JS on real libhermes and gets 42 back, through an `extern "C"` C++/JSI shim |
| C3 hello-world | a script runs through the backend via the v8 C-ABI (`Script::compile`/`run` -> "hello world") |
| C4 identity | object identity solved (`strictEquals` + hidden-symbol-id `GetIdentityHash`) |
| C5 AOT | precompiled Hermes bytecode (HBC) runs parse-free, 21x faster boot |
| C6 surface | objects / arrays / numbers / functions run through the backend (12/12 smoke tests) |
| C7-C12 ratchet | 0 -> 10 -> 33 -> 58 -> 61 -> 76 -> 77 real rusty_v8 tests passing |

What works through the Hermes backend today: isolates, contexts, handle scopes, strings, numbers,
booleans, objects, arrays, typed arrays, function calls, native function callbacks
(`Function::New`/`FunctionTemplate` + the full `FunctionCallbackInfo` bridge), ObjectTemplate with
internal fields, native property accessors, FunctionTemplate signatures, TryCatch and thrown
exceptions, and correct object identity.

Build + test it (macOS; the prebuilt `hermes.framework` is vendored):

    cargo test --no-default-features --features hermes,link_hermes --lib hermes:: -- --nocapture
    node tests/harness/run.mjs rusty_v8 hermes --rescue

Note: hermes MUST run with `--rescue` (a shared test-infra `PROCESS_LOCK` poisons on any earlier
panic, same as the QuickJS backend). The C++/JSI bridge is `src/hermes/hermes_shim.cpp`; the real
v8__* live in `src/hermes/core.rs`.

## 3. Does AOT solve the snapshot problem? Answered with data.

The honest, measured conclusion (experiments E6 + C5):

- A V8 startup snapshot serializes an initialized HEAP; AOT serializes CODE. They are not the same.
- E6: a real post-bootstrap heap snapshot IS achievable on QuickJS (v8x already has the machinery:
  `JS_WRITE_OBJ_REFERENCE` + a symbolic native-function reference-path registry). But restore
  (~10 ms) is NOT faster than re-running the bootstrap (~8.4 ms): QuickJS has no mmap-and-fixup fast
  path, so `JS_ReadObject` does the same order of work as running the code. A heap snapshot's value
  is state capture (side effects, non-determinism), not startup latency.
- C5: on Hermes, running the same bootstrap-shaped code from precompiled HBC is 21x faster than from
  source (202 ms -> 9.3 ms). Hermes builtins are native C++, so only the runtime/app bootstrap runs
  at boot, and HBC removes the parse+compile cost.

So the "wonderful" lever is parse-free AOT bytecode, not heap snapshotting. For a Deno-style runtime
the path is: native builtins + ship the JS bootstrap as HBC (or QuickJS bytecode) instead of source.
This maps directly onto `deno compile` shipping bytecode, and it sidesteps the QuickJS snapshot
replay-tape pain entirely.

## Honest state and next steps

Delivered and solid: the QuickJS Deno binary; a Hermes backend that runs real JS and passes 77 real
V8 tests with identity, exceptions, callbacks, templates/accessors, and signatures; the AOT measurements.

Not done (this was a spike, not a product):
- Hermes is nowhere near running Deno yet. 77/267 rusty_v8 tests; then comes the whole deno_core suite
  (modules, async, ops, microtasks), then the runtime. That is a large, multi-week road, not a night.
- Known gaps: `SameValue` is not yet exact for NaN/+-0; weak handles over-retain (no GC weak-callback);
  4 tests crash inside the vendored extern-C trampoline (unfixable without editing vendored code;
  --rescue skips them); named property interceptors are designed but unimplemented; modules,
  async/microtask pump, and Promises are unimplemented.

Prioritized next targets: named/indexed property interceptors -> BackingStore/shared_ptr (also kills the last crashers) -> Promises/microtask queue -> ES modules ->
start the deno_core hill-climb. Separately, wire an HBC path into `deno compile` to cash in the 21x
startup win on real Deno bootstrap.


## Update: grinding toward Deno-on-Hermes (D-series)

The overnight run built the Hermes BACKEND (runs JS, 77/267 rusty_v8), not Deno-on-Hermes. Now
grinding toward an actual Deno boot. Recon (D0) found the boot path is blocked by exactly three
missing subsystems, proved with an in-repo boot probe: Promises, microtasks, and ES modules (all
null stubs). deno_core boots from source but loads its core JS as an ES module, so modules are
required, not optional. Progress: Promises + microtasks + ES modules all DONE - all 4 in-repo boot probes now pass, and
rusty_v8 is at 83/267. ES modules required a from-scratch mini-linker on bare Hermes (JSI has no module
API). The boot PROBES are a proxy though; the real milestone is still ahead: wire actual deno_core to
build against the Hermes backend and attempt a JsRuntime boot (full bootstrap + real module graph + ops).
That is the next cycle. Multi-session effort; no boot claimed until a real deno_core runtime runs a script. Honest note: Hermes v0.11.0 is
interpreter-only (no JIT) and is not a compute win over QuickJS - benchmarks in ~/deno-quickjs/BENCHMARKS.md.


## MILESTONE: deno_core boots on Hermes (into Deno's bootstrap JS)

An actual deno_core::JsRuntime::new now RUNS on the Hermes backend: through v8 platform init, isolate,
context (with deno_core's global ObjectTemplate + embedder data), the full string-interning surface, and
the Deno.core namespace, and INTO executing Deno's real bootstrap JavaScript. It stops at the first
bootstrap script ext:core/00_primordials.js, which throws because the vendored Hermes v0.11.0 is an OLD
build missing 6 intrinsics Deno needs (AggregateError, BigInt, BigInt64Array, BigUint64Array,
FinalizationRegistry, WeakRef). Then bumped Hermes to a 2026 build (260318099.0.1, HBC 99) - which cleared the intrinsics wall and pushed
rusty_v8 to 86/267. The boot now gets PAST primordials' intrinsic enumeration and fails one step deeper at
compile time: Hermes has no `async function*` (async generators), which primordials.js uses to grab the
%AsyncGenerator% prototype. So the wall is now a Hermes source-language gap. Next: transform/polyfill that
construct and advance toward the ext:core/mod.js module graph. Still no 1+1 - honest, incremental. Risk: async
generators may recur in Deno's real runtime code, which would make this a pervasive blocker.


## The viability ceiling (honest, D7): deno_core yes, full Deno no

D7 answered the crux. The boot now advances to the LAST step of deno_core::JsRuntime::new_inner (past
primordials, op registration, all of 01_core.js, and the builtin ES-module graph), blocked only on a
well-scoped ArrayBuffer BackingStore subsystem - so deno_core booting + running 1+1 on Hermes is within reach.
BUT: Deno's WIDER runtime (ext/net for-await over a listener, ext/web Blob.stream(), Node stream polyfills)
uses REAL async generators (async function* / async *method / Symbol.asyncIterator) pervasively, and Hermes's
compiler does not support async function* at all. The one occurrence in deno_core's boot (primordials capturing
the %AsyncGenerator% prototype) was source-transformable; the runtime ones are real suspend/resume generators
that are NOT. So the honest result: deno_core (the engine-embedding core) can boot and run code on Hermes, but
a COMPLETE Deno runtime cannot without upstream Hermes gaining async generators or a large rewrite of vendored
Deno source. rusty_v8 now 89/267.


## PAYOFF: deno_core boots and runs 1+1 on the Hermes backend

The Deno-boot grind reached its milestone. An actual deno_core::JsRuntime::new SUCCEEDS on the Hermes
backend and execute_script("1 + 1") runs cleanly (BOOT OK; value handle returned). D8's external-memory
BackingStore subsystem closed the last step of JsRuntime::new_inner. So: a third JS engine backend for
v8x, built from scratch this session, now boots deno_core and runs JavaScript.
Honest bound: this is deno_core (the runtime core). A COMPLETE Deno runtime remains blocked by the D7
ceiling - Deno's ext/ layer uses real async generators (for await over sockets, Blob.stream, Node streams)
that Hermes's compiler does not support and that are not source-transformable. deno_core-on-Hermes is the
deliverable; full-Deno-on-Hermes would need upstream Hermes async-generator support or a large rewrite of
vendored Deno source.
