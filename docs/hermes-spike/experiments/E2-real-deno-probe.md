# E2 — the lowering vs REAL Deno primordials + a full-runtime probe

E1 proved async generators run through the backend on hand-written scripts. E2
tests the lowering against Deno's REAL `00_primordials.js` and probes how far the
full runtime is. Work done in the deno checkout `/Users/divy/gh/deno-v8x-rebase`
(branch `v8x-rebase-rc`) whose `v8` alias resolves to this repo, so it exercises
the live backend + lowering.

## GATE 1 — real primordials: the E1 lowering REGRESSED the boot; fixed here

Important honesty correction: the D8 "deno_core runs 1+1" milestone was recorded
against a **stale pre-E1 binary** still carrying the removed D7 async-gen hack. A
clean rebuild against E1's actual oxc lowering fails at
`JsRuntime::try_new -> Failed to execute ext:core/00_primordials.js`.

Root cause (exactly E1's flagged `%AsyncGenerator%` identity risk, and it is
fatal, not cosmetic): `00_primordials.js:285` runs
`Reflect.getPrototypeOf(async function*(){})` then
`copyPrototype(original.prototype, ...)` -> `Reflect.ownKeys(original.prototype)`.
Under E1's lowering the async-gen function is a plain wrapper whose `[[Prototype]]`
is `Function.prototype`, so `getPrototypeOf(...)` returns `Function.prototype`,
`.prototype` is `undefined`, and `Reflect.ownKeys(undefined)` throws
`TypeError: target is not an object`.

Fix (backend, commit `b746874`), in `src/hermes/lower.rs` `babelHelpers`: build a
`%AsyncGeneratorFunction.prototype%` object whose own `prototype` is the real
`%AsyncGeneratorPrototype%` and whose `[[Prototype]]` is `Function.prototype`
(load-bearing: oxc emits `_ag.apply(this, arguments)` on the wrapper, so a bare
`{}` made every async gen reject `undefined is not a function`), then
`Object.setPrototypeOf` each lowered wrapper onto it.

Verified (rebuild + run; the example needs `DYLD_FRAMEWORK_PATH` pointed at
`vendor/hermes` since the example binary carries no LC_RPATH for the framework):

```
GATE1 BOOT OK: JsRuntime::new succeeded
GATE1 1+1 executed OK (value handle returned)
```

v8x hermes lib suite: 34 passed, 0 failed (incl. `boot_async_generator_primordials_capture`).

## GATE 2 — async iteration INSIDE booted deno_core: PASS

A script defining and consuming an async generator with `for await`
(`yield await` + `yield*`), driven through the deno_core event loop
(`with_event_loop_promise`):

```
GATE2 async-gen result: "1,2,3,4"   (asserted)
```

## GATE 3 — probe toward the full runtime: PARTIAL, wall named

- Full `deno_runtime` does NOT build, but the wall is **not Hermes**: `deno_napi`
  fails with 8x `error[E0282]: type annotations needed` on `INT_MAX as _`
  (`ext/napi/js_native_api.rs:1874/2025`, `util.rs:118`) — a pre-existing
  Deno-source/toolchain issue; `runtime/Cargo.toml` depends on `deno_napi`
  directly.
- `deno_web` builds clean on Hermes (exit 0), so a minimal
  `deno_webidl + deno_web (+ deno_console)` worker (no node/napi) is the tractable
  target.
- ext/web-shaped smoke (an `async *stream()` + `async function* toIterator` with
  `yield*`, matching the `ext/web/09_file.js` Blob shape), run inside booted
  deno_core: `GATE3 ext-web-shape result: "1,2,3,4,5"` (asserted).
- The E2 fix directly unblocks real ext/web: `ext/web/09_file.js` imports
  `AsyncGeneratorPrototypeNext` from primordials; probed in the booted runtime,
  `primordials.AsyncGeneratorPrototype{Next,Return,Throw}` are all `function`
  (absent before E2).

## Verdict / next

Async generators are handled against REAL Deno primordials and real-shaped ext/web
streaming, inside a booted deno_core. The remaining path to full Deno is the
ordinary op + Web-API-global surface. The immediate MECHANICAL blocker for the
whole `deno_runtime` is the non-Hermes `deno_napi` E0282; the tractable
Hermes-specific target is a minimal `deno_webidl + deno_web (+ deno_console)`
worker. E3 stands that up and names the first `op_*`/global wall the way D4-D8
named each `v8__*`.

## Scratch-sandbox notes (deno checkout, not pushed)

- `hermes_boot.rs` extended to the 3 gates above (166 lines).
- `Cargo.lock` regenerated (`cargo generate-lockfile`): the committed lock
  consumed v8x from crates.io (pre-oxc) and could not resolve once the local path
  dep pulled the oxc subtree.
- Runtime needs `DYLD_FRAMEWORK_PATH=/Users/divy/gh/v82jsc/vendor/hermes` for the
  example binary (no LC_RPATH baked in).
- Two full `deno_core` rebuilds (oxc + v8x, ~260 crates each) exhausted the disk
  mid-cycle (ENOSPC); the scratch `target/` was later removed to reclaim space.
