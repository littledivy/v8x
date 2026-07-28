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
