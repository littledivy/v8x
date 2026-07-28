# D1: Promises + microtask queue on the Hermes backend

Goal of this cycle: knock down the first two Deno-boot walls D0 found, Promises
and the microtask queue, so the two async boot probes go green and the
Promise-shaped rusty_v8 tests unlock. ES modules (the third wall) are left for
D2.

## TL;DR

- Both target boot probes are GREEN:
  `boot_promise_resolver_roundtrip` and `boot_microtask_enqueue_and_checkpoint`.
  `cargo test --no-default-features --features hermes,link_hermes --lib
  hermes_boot_probe` now reports 3 passed / 1 failed, the one failure being
  `boot_es_module_instantiate_evaluate` (the remaining ES-module wall, D2).
- rusty_v8 on hermes went from 77 to 81 passing (of 267). The four new passes
  are all Promise/microtask: `promise_resolved`, `promise_rejected`,
  `microtask_queue_new`, `set_promise_reject_callback`.
- No regressions: the other 15 hermes smoke tests (hello-world, identity,
  surface, HBC, signature) and every previously-baselined rusty_v8 test still
  pass.

## What was implemented

The whole subsystem is driven through a single cached JS helper object built
once per runtime (`ensure_promise_infra` in `src/hermes/hermes_shim.cpp`),
plus thin `v8__*` wrappers in `src/hermes/core.rs`.

`v8__*` symbols made real (were null/no-op stubs):

- `Promise::Resolver::New` / `GetPromise` / `Resolve` / `Reject`
- `Promise::State` / `Result` / `HasHandler` / `MarkAsHandled`
- `Promise::Then` / `Catch` / `Then2`
- `Isolate::EnqueueMicrotask`, `Isolate::PerformMicrotaskCheckpoint`
- `MicrotaskQueue::New` / `DESTRUCT` / `EnqueueMicrotask` /
  `PerformCheckpoint` / `IsRunningMicrotasks` / `GetMicrotasksScopeDepth`

### The setImmediate landmine (the one real surprise)

Hermes ships its ES6 Promise implementation as InternalBytecode, and that
polyfill schedules every reaction job through a GLOBAL `setImmediate`. This
Hermes build (v0.11.0) has no `RuntimeConfig` microtask-queue option, and the
bare JSI global does NOT provide `setImmediate`. So the first `resolve()` or
`.then()` threw `Property 'setImmediate' doesn't exist` and returned the
failure sentinel.

Fix: the promise infra, on first use, installs `globalThis.setImmediate` (and a
no-op `clearImmediate`) backed by its own FIFO job queue, plus a `drainJobs()`
that runs that queue to completion. `PerformMicrotaskCheckpoint` /
`MicrotaskQueue::PerformCheckpoint` call `drainJobs()` (and poke Hermes's own
`jsi::Runtime::drainMicrotasks()` for good measure). This is the standard
embedder move: without a host job scheduler, Hermes's Promise polyfill cannot
run reactions.

### How Promise state is tracked over JSI

JSI exposes no Promise API and no `[[PromiseState]]`/`[[PromiseResult]]`
accessor, so state is tracked in JS:

- A v8 `Resolver` is modelled as the 3-element JS array `[promise, resolve,
  reject]` the helper's `makeResolver()` returns. `GetPromise` is element 0;
  `Resolve`/`Reject` call element 1/2.
- State + result live in a closure-captured `WeakMap` keyed by the promise
  object, so nothing is added to the promise visible to JS. V8's
  `Promise::State` reflects a settled promise IMMEDIATELY after Resolve/Reject
  (before any reaction runs), so the resolve/reject wrappers record state
  SYNCHRONOUSLY (`setOnce`, first-write-wins so a double-settle is ignored like
  a real promise). A parallel async `.then` recorder also writes state, to
  cover promises settled by ordinary JS and observed after a drain.
- `HasHandler` reads a separate `WeakSet` of promises the USER attached a
  reaction to (via our `then`/`catch`/`then2` wrappers). The internal
  state-recorder `.then` deliberately does not go through those wrappers, so it
  never marks a promise handled. This is what makes a fresh resolver's promise
  report `has_handler() == false`, which `promise_resolved`/`promise_rejected`
  assert. `MarkAsHandled` adds to the same set.

### MicrotaskQueue object API

Hermes has one shared job queue per runtime (the setImmediate FIFO), so a
`MicrotaskQueue` handle is a small boxed marker (`MtqState`) whose
enqueue/checkpoint route to that same shared queue. This gives a real,
non-null, round-trippable pointer (the identity check in `microtask_queue_new`)
and working enqueue+drain without a second queue. `IsRunningMicrotasks`
reflects a flag set for the duration of a checkpoint;
`GetMicrotasksScopeDepth` returns 0.

## Constraints honored

- C2 (lifetime): all captured resolve/reject functions and the cached helper
  functions are held in Runtime-owned `std::unique_ptr<jsi::Value>` on the
  `RuntimeWrapper`, outliving any HandleScope; state lives in JS heap
  (WeakMap/WeakSet), not in a handle-table slot that a scope pop could truncate.
- C4 (identity): the WeakMap/WeakSet are keyed by the promise object itself, so
  the same promise read through two different Locals maps to the same state.
- C9 (exceptions): a throwing microtask/reaction is swallowed inside
  `drainJobs` (and at the C boundary in `drain_microtasks`), never unwinding
  across `extern "C"`, matching V8 discarding an exceptional job with no
  handler.

## Crash landmines neutralized

- `setImmediate` absence (above): would throw on the first resolve/then;
  installed before any promise is created.
- A rejected promise with no user handler: the internal recorder `.then` always
  attaches BOTH an onFulfilled and an onRejected handler, so Hermes never sees
  the rejection as unhandled. This avoids any unhandled-rejection abort during
  a drain.
- `drainJobs` and `drain_microtasks` are both capped (1e6 / 1e5 iterations) so a
  pathological re-enqueue loop cannot hang a test.

## Not done this cycle (deferred, higher risk)

- `microtasks` and `microtask_queue`: both rely on V8's Auto policy
  auto-flushing the microtask queue at the end of `eval("")`. Hermes runs jobs
  through the setImmediate FIFO we drain explicitly; there is no auto-flush hook
  on eval, so these stay red until a policy-driven auto-drain is wired.
- `promise_reject_callback_no_value`, `promise_hook`, `context_promise_hooks*`:
  need the promise-reject / promise-hook callbacks to actually fire from
  Hermes's promise machinery, which JSI does not surface. Separate subsystem.
- `set_microtasks_policy` / `get_microtasks_policy` remain stubs (the tests that
  need a real round-trip also need auto-flush, so wiring the policy alone buys
  nothing yet).

## The remaining wall

`boot_es_module_instantiate_evaluate` is still red:
`v8__ScriptCompiler__CompileModule` is a null stub. ES modules are confirmed as
the last boot wall before deno_core can attempt a `JsRuntime::new` +
`mod_evaluate`. That is D2, the headline high-risk step (no clean JSI analogue
for a SourceTextModule).

## Files touched

- `src/hermes/hermes_shim.cpp`: promise infra (`ensure_promise_infra`) + the
  `v8x_hermes_promise_*` / `v8x_hermes_*_microtask*` bridge functions.
- `src/hermes/core.rs`: the `v8__Promise__*`, `v8__Isolate__EnqueueMicrotask`,
  real `v8__Isolate__PerformMicrotaskCheckpoint`, and `v8__MicrotaskQueue__*`
  wrappers; extern "C" decls; import of `Promise`/`PromiseResolver`/
  `PromiseState`/`MicrotaskQueue`.
- `src/hermes/shims.rs`: removed the 18 now-real stubs.
- `tests/status/baselines/hermes/rusty_v8.txt`: 77 to 81 passing.
