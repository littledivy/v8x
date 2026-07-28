# E1 — async-generator lowering, landed and proven end-to-end

**Directive:** implement the async-generator source-to-source lowering pass E0
argued was tractable, wire it into the Hermes compile boundary, and PROVE an
async generator runs end-to-end through the backend (not just compiles).

**Result: done.** The lowering pass lands, compiles clean, and an async
generator iterates to the correct values THROUGH the real Hermes backend.

## What Hermes rejects, and the downlevel

Hermes' compiler rejects exactly one construct at parse time (`error: async
generators are unsupported`): the async-generator *declaration* syntax, in both
spellings.

| construct | hermesc |
|---|---|
| `async function* ag(){}` | FAIL |
| `const o = { async *m(){} }` / `class C { async *m(){} }` | FAIL |
| regular `function*`, `async`/`await`, `yield*`, `Symbol.asyncIterator` | OK |
| `for await (const x of it)` consumption | OK |

So an async generator lowers to the standard ES2017 form: a regular `function*`
wrapped by runtime helpers (`_wrapAsyncGenerator` / `_awaitAsyncGenerator`,
`yield x` -> `yield yield ...`, `await x` -> `yield _awaitAsyncGenerator(x)`,
`yield* it` -> `yield* _asyncGeneratorDelegate(_asyncIterator(it))`), consumed by
native `for await`. hermesc accepts that form with no error.

## The pass (`src/hermes/lower.rs`)

`pub fn lower_async_generators(src: &str) -> Cow<str>`:

1. Cheap prefilter (`contains_async_generator`): a keyword-aware substring scan
   for `async function*` and `async *` (async generator method). Sources with no
   async generator return `Cow::Borrowed(src)` unchanged and never parse.
2. When present: parse with **oxc** (`oxc_parser`), run oxc's ES2018
   async-generator transform (`ESTarget::ES2017` + `HelperLoaderMode::External`),
   and codegen. The transform is Babel-parity: it handles function declarations,
   function expressions, object methods, class methods, `yield`, `yield await`,
   `yield*`, and `for await` in generator/async bodies.
3. Prepend the `babelHelpers` runtime once per unit (idempotent via a
   `typeof globalThis.babelHelpers === "undefined"` guard). External helper mode
   emits `babelHelpers.wrapAsyncGenerator(...)` calls; we supply that object with
   the canonical `@oxc-project/runtime` / Babel helper implementations inlined
   (`awaitAsyncGenerator`, `wrapAsyncGenerator`, `asyncIterator`,
   `asyncGeneratorDelegate`), so the compiled unit needs no module import.

### Transform engine: oxc, not swc

The directive recommended swc first. swc was attempted and abandoned: its crate
tree at the current versions imports `serde::__private`, removed in serde
1.0.229, forcing a global serde downgrade in the whole crate, AND it resolves two
incompatible `swc_common` versions that will not unify at the type level. oxc
0.90 resolves cleanly behind the `engine_hermes` feature with no serde conflict
and ships a dedicated `es2018/async_generator_functions` transform. It is gated
(`oxc = { ..., optional = true }`, pulled by `engine_hermes = ["dep:oxc"]`) so no
other backend pays for the ~260-crate oxc tree; the quickjs/jsc builds contain
zero oxc.

### The one oxc 0.90 defect we work around

oxc's `for await` downlevel **drops the loop body when it is a single unbraced
statement** (`for await (const x of it) f(x);` loses `f(x)`; it only preserves
`Statement::BlockStatement` bodies). Since Hermes runs `for await` natively we do
not want oxc to touch it, but oxc's async-generator pass and its for-await pass
are one monolithic transform. So a pre-pass (`BraceForAwait`, an oxc `VisitMut`)
wraps every non-block `for await` body in a block before the transform. This is
an AST edit via oxc's own builder, not a text hack. The end-to-end tests include
an unbraced for-await body specifically to exercise this workaround.

## Wired into the compile boundary

- `v8__Script__Compile` (`src/hermes/core.rs`): every script source flows through
  `lower_async_generators` before Hermes parses it. This **replaces** the old D7
  `rewrite_async_generator_literal` hack (which only stubbed one reflection
  literal into a synthetic prototype and would corrupt a real async generator);
  that function and its `span_async_generator_body` helper are removed.
- `v8__Module__Evaluate` path (`src/hermes/modules.rs`): the fully-formed module
  closure source (after import/export rewriting) is lowered the same way, so
  module bodies containing async generators are covered too.

## End-to-end proof (the milestone)

`src/hermes/mod.rs::hermes_async_generator`, run through the real backend
(`--features hermes,link_hermes`): Script::compile -> Script::run -> pump
microtask checkpoints until the result promise settles -> read the resolved
value.

| test | source | asserted result | outcome |
|---|---|---|---|
| `async_generator_runs_end_to_end` | `async function* ag(){ yield 1; yield await Promise.resolve(2); yield 3; }` consumed by `for await` | `"1,2,3"` | PASS |
| `async_generator_yield_star_and_methods` | `yield*` delegation + object method + class method async generators, mixed braced/unbraced `for await` | `"1,2,3,a,b,10"` | PASS |

Full suite: **34 passed, 0 failed** (`hermes,link_hermes`).

### A backend fix the proof forced (promise state tracking)

The first run compiled clean but the result promise stayed `Pending` forever. The
backend tracks `[[PromiseState]]` in a WeakMap populated only for promises that
pass through `makeResolver`/`record` (D1). A promise created purely in JS (the
async IIFE the lowering produces) was never recorded, so `Promise::State` read
`Pending` regardless of actual settlement. Fix (`src/hermes/hermes_shim.cpp`):
`getState` now lazily attaches the state recorder to any untracked promise on
first query (guarded by a `tracked` WeakSet), so a following `drainJobs` settles
it. This is panic-safe (inside the existing try/catch) and does not change
behavior for already-tracked promises. It generalizes the D1 promise surface to
JS-created promises, which the full Deno runtime will also need.

## Robustness limits (honest)

- The body rewrite is oxc's own ES2018 pass (Babel-parity), far more robust than
  the removed D7 hand-rewrite. It handles the async-generator forms Deno uses.
- It does NOT reproduce V8's exact intrinsic identity via
  `Reflect.getPrototypeOf(async function*(){})` on the *function object* (that
  reflects the lowered wrapper, whose prototype is `Function.prototype`). The
  usable `%AsyncGeneratorPrototype%` (next/return/throw/asyncIterator) is reached
  from an INSTANCE, which the updated `boot_async_generator_primordials_capture`
  test now asserts. deno_core's primordials capture that specifically does
  `Reflect.getPrototypeOf(asyncGenFn)` to pin `%AsyncGenerator%` will get a
  different object identity than under real V8; whether deno_core only needs the
  instance-reachable prototype (works) or the function-object reflection identity
  (does not) is the next thing to verify against real primordials.
- TypeScript / JSX are not enabled (the backend only sees plain JS here).
- If oxc fails to PARSE the input (a real syntax error unrelated to async
  generators), the pass returns the source unchanged so Hermes reports the real
  error rather than masking it.

## The single most important next obstacle for Deno's ext/ layer

Running Deno's `ext/` layer end-to-end is now a matter of grinding the op /
Web-API surface, not a syntax wall. The nearest concrete obstacle is the
**primordials `%AsyncGenerator%` intrinsic-identity capture** noted above: if
`ext:core/00_primordials.js`'s `Reflect.getPrototypeOf(async function*(){})`
capture is used to brand or `instanceof`-check async generators elsewhere in the
runtime, the lowered wrapper's differing prototype identity will diverge from V8.
The lowering must be validated against the actual primordials + the first ext
module that constructs and consumes an async generator (ext/web streams is the
likely first heavy user), driving it through microtask completion the same way
the E1 tests do.

## Files changed

- `src/hermes/lower.rs` — new module: the lowering pass, prefilter,
  `BraceForAwait` workaround, `babelHelpers` runtime, unit tests.
- `src/hermes/core.rs` — `v8__Script__Compile` routes source through
  `lower_async_generators`; removed the dead D7 `rewrite_async_generator_literal`
  / `span_async_generator_body`.
- `src/hermes/modules.rs` — module-evaluate path lowers the closure source.
- `src/hermes/hermes_shim.cpp` — `getState` records untracked promises on first
  query so JS-created promises settle.
- `src/hermes/mod.rs` — register `lower`; new `hermes_async_generator` end-to-end
  test module; updated `boot_async_generator_primordials_capture` to the real
  post-lowering shape.
- `Cargo.toml` — gated `oxc` dependency; `engine_hermes = ["dep:oxc"]`.
