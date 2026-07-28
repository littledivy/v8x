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


## RETRACTION (E0): the async-generator ceiling was wrong

The paragraph directly above is retracted. On the user's "attempt full Deno anyway" call I
re-examined the claim empirically and it does not hold. Hermes (260318099.0.1) rejects only the
`async function*` / `async *method` **declaration syntax**; it compiles regular generators
(`function*`), async/await, `Symbol.asyncIterator`, and - the construct D7 wrongly flagged - `for await`
consumption. So async generators lower cleanly to the standard ES2017 form (a `function*` wrapped by the
tslib `__asyncGenerator`/`__await` helpers), and hermesc compiles that lowered form to HBC with no error.
The lowering is a source-to-source pass we can insert at v8x's OWN compile boundary (`Script::compile` /
`CompileModule` in `src/hermes/`), touching zero vendored Deno or vendored-test source. Evidence and
probe table in `experiments/E0-asyncgen-ceiling-reexamined.md`.

Corrected framing: async generators are a **scalable wall, not a ceiling**. Full Deno on Hermes is now a
grind over two independent surfaces - (1) the syntax-lowering pass, proven tractable, and (2) the large
but ordinary op / Web-API surface of the full Deno runtime beyond deno_core - not a fundamental
engine block. The E-series pursues both.

E1 landed the lowering pass (src/hermes/lower.rs, oxc ES2018 transform gated behind engine_hermes, wired
into Script::compile + module eval); async generators + for-await run end-to-end through the backend.
E2 then tested it against REAL Deno and found+fixed a fatal gap: the lowering must give lowered async
generators the %AsyncGeneratorFunction.prototype% intrinsic chain, or Deno's 00_primordials.js throws
while pinning %AsyncGenerator%. (This also corrected the record: the earlier D8 "1+1" milestone had run on
a stale pre-lowering binary.) With the fix, deno_core boots and runs 1+1 with the real lowering active,
async iteration inside the booted runtime yields "1,2,3,4", and ext/web-shaped async-gen streaming yields
"1,2,3,4,5". The remaining path to full Deno is the ordinary op + Web-API-global surface; the immediate
blocker for the whole deno_runtime crate is a non-Hermes deno_napi compile error, while deno_web builds
clean, so the next target is a minimal deno_webidl + deno_web worker on Hermes.

E3 stood that worker up. deno_webidl + deno_web boot on Hermes and all 24 of ext/web's lazy JS files
(00_infra.js through 18_css_stylesheet.js, including the ~8000-line 06_streams.js and the async-generator
09_file.js) evaluate without throwing. Getting there needed six real backend gaps closed: script-compiler
CompileFunction, the Global handle refcount/pin lifetime, capturing Hermes JSIExceptions (not just
JSErrors), the error-surface shims (Exception::CreateMessage, String::Empty, Object::GetPrototype), and a
fix so the lowering pass handles deno_core's `"use strict"; return (IIFE)` ext-script wrapper. Functional
probes pass: atob/btoa, Event, ReadableStream construction. So ext/web is hydrated and partially functional
on Hermes. The next walls are the v8::Private subsystem (used by the error formatter and callsite metadata)
and the ArrayBuffer/TypedArray ABI that TextEncoder/structured-clone need. This is exactly the ordinary
op/API grind the retraction predicted, not a fundamental block.

E4 closed the first two of those. v8::Private is implemented (a hidden non-enumerable per-object key), so a
thrown error inside ext/web now FORMATS and propagates as a normal JS error instead of panicking. The
TypedArray/ArrayBufferView ABI now reports JS-created typed arrays correctly to deno_core's op layer, and
with v8::Symbol added, TextEncoder/TextDecoder round-trip real text through ext/web's JS
(new TextDecoder().decode(new TextEncoder().encode("héllo")) === "héllo"). So ext/web is genuinely (not
cosmetically) functional for encoding and error handling. Two ext/web features remain deferred, each a named
next wall: structuredClone needs v8::ValueSerializer/Deserializer, and a real ReadableStream chunk read needs
the deno_core event-loop / microtask drive fully wired. Backend suite 37/37.

E5 closed both. The ReadableStream "Pending" turned out to be a Promise::State observation lag, not a drain
gap (a state-read armed a lazy recorder whose reaction only fired on a later drain; the sole read happened
after all drains, so it stayed Pending forever). The fix drains the microtask queue once on a Pending
state-read. With it, a ReadableStream that was enqueued a chunk delivers through real ext/web 06_streams.js +
the deno_core event loop: reader.read() resolves to {value: 42, done: false}. A 25-symbol
v8::ValueSerializer/Deserializer (a recursive JSI-value walk) then makes structuredClone round-trip
primitives, arrays, and plain objects, and refuse BigInt/Symbol/Date/Map/Set/TypedArray/cycles with a clean
DataCloneError. Backend suite 39/39. So ext/web is substantially functional on Hermes: encoding, error
handling, structured clone, and microtask-driven stream reads. The honest remaining boundary is a promise
resolved LATER by a deferred op (a timer or a socket op) settling on run_event_loop, which is the bridge to
for-await over a socket and the target of the network-stack cycles.

E6 crossed that boundary. A genuinely-deferred async op (it awaits a real tokio sleep and is Pending on its
first poll) now settles its JS promise through the real deno_core run_event_loop: await op_delayed(41) === 42,
with run_event_loop taking the real ~7ms. The bug in the way was subtle and load-bearing: v8__Function__Call
could not decode a Global<Function> stored as a pin-handle (a different tagged shape than a value slot), so
deno_core's __eventLoopTick callback silently did nothing and every deferred-op promise hung. With the
handle-decode fix, the op -> promise-resolve -> event-loop path works. On top of that, console.log produces
real output through ext/web's inspector op (console.log("hi", {a:1}, [2,3]) -> "hi { a: 1 } [ 2, 3 ]") and URL
parses through op_url_parse (new URL("https://a.b/p/q?x=1#h") -> pathname "/p/q", searchParams x "1"). Backend
suite 41/41. So the real async-I/O foundation is in place; the network stack (ext/net, then fetch), where
for-await over a socket lives, is the next target and reuses exactly this proven op/promise/loop plumbing.

E7 took the network stack to a precise, honest boundary. Real deno_net (ext/net) builds and boots on Hermes;
op_net_listen returns a real listener; accept/connect dispatch real promises; and the `for await (const conn
of listener)` construct itself RUNS on Hermes - it reaches the loop, dispatches op_net_accept, and suspends
awaiting it. What does NOT happen is completion: socket op promises never settle through run_event_loop, so no
byte round-trips and for-await yields no connection. The root cause was isolated with a reproducing probe (and
corroborated independently): this scratch deno checkout uses a custom libuv-compat reactor (libs/core/uv_compat),
and while run_event_loop parks on it, kqueue I/O-readiness wakes and spawn_blocking wakes are not serviced -
only timer wakes are (which is exactly why E6's timer-backed op settled). Raw tokio sockets and a raw
spawn_blocking run fine on the very same runtime OUTSIDE run_event_loop. So the residual blocker is a
runtime-integration bug in the checkout's event loop, NOT the Hermes engine and NOT the op->promise->
__eventLoopTick bridge E6 proved. En route E7 also fixed a genuine Hermes lowering bug: the async-generator
pass was targeting ES2017 and thereby also rewriting ES2022 #private fields into helpers we do not provide;
it now lowers async generators only. Backend suite 42/42. The next cycle fixes the uv_compat reactor so the
op-driver's spawned poll_task is serviced by the reactor the loop parks on, which unblocks ext/net, the raw-op
path, and fetch at once.

E8 did that and closed the loop: `for await (const conn of listener)` now accepts a REAL connection and a byte
round-trips client -> server -> client over a real loopback OS socket, through real ext/net, driven by
run_event_loop on Hermes (both E7 assertions PASS, verified independently). This is the exact construct the
overnight spike declared impossible, and it works. The E7 "uv_compat reactor" diagnosis turned out wrong; the
real causes were (A) a probe-harness bug (a fresh per-test tokio runtime orphaned the op-driver's spawned
poll_task) and (B) a genuine Hermes backend gap: v8__Uint32__Value and v8__Int32__Value were null stubs with the
wrong C-ABI signature, so Uint32::value()/Int32::value() always read 0. Those accessors are the first branch of
deno_core's promiseId decode, so every non-zero async-op promiseId was silently zeroed and any second concurrent
deferred op resolved into "Missing promise @ 0" (E6's single timer op only worked because its id was 0).
Implementing both via ECMAScript ToUint32/ToInt32 fixed it. Backend suite 43/43. So the arc is complete end to
end: async generators lower and run, deno_core boots, ext/web is functional, real deferred-op async settles
through the event loop, and for-await over a real socket delivers bytes. The overnight "full Deno is impossible
on Hermes" conclusion is retired. The next target is fetch (deno_fetch + hyper + TLS), which rides the same
now-proven op/promise/event-loop path.
