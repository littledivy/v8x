# E0 — The async-generator "ceiling" re-examined (and overturned)

**Directive:** user chose "attempt full Deno anyway"; first task is to challenge the
D7/D8 conclusion that a complete Deno runtime is *fundamentally* blocked on Hermes
because it uses async generators that are "not source-transformable."

**Result: that conclusion was wrong.** The gap is real but narrow and transformable.

## What Hermes 260318099.0.1 (HBC 99) actually accepts

Probed by feeding each construct to `vendor/hermes/bin/hermesc -emit-binary`
(the ceiling is a *compile-time* syntax rejection, so hermesc accept/reject is
the exact signal):

| construct | hermesc |
|---|---|
| `async function f(){ await x }` | OK |
| `function* g(){ yield 1 }` (regular generator) | OK |
| `for (const x of g())` | OK |
| `for await (const x of it)` | **OK** |
| `Symbol.asyncIterator` | OK |
| `yield*` delegation | OK |
| optional chaining / nullish / `#private` / `||=` etc. | OK |
| `async function* ag(){ yield 1 }` | **FAIL: "async generators are unsupported"** |
| `const o = { async *m(){} }` | **FAIL: "async generators are unsupported"** |

The *only* rejected construct is the `async function*` / `async *method`
**producer declaration syntax**. Everything the standard ES2017 downlevel of an
async generator needs is present natively:

- regular generators (`function*`)
- `async`/`await`
- `Symbol.asyncIterator`
- `for await` **consumption** (D7 wrongly listed this as a blocker; it compiles)

## The transform target is valid Hermes

Hand-lowered an async generator the way TypeScript (`target: ES2017`) / tslib do
it — a regular `function*` wrapped by the `__asyncGenerator` / `__await` runtime
helpers, consumed by native `for await`:

```
async function* ag(){ yield 1; yield await Promise.resolve(2); }
```

lowers to a `function ag(){ return __asyncGenerator(this, arguments, function* ag_1(){ ... }) }`
using only primitives in the table above. `hermesc -emit-binary` compiles the
lowered form to 4412 bytes of HBC with no error
(`scratchpad/lowered.js` → `lowered.hbc`).

## Corrected conclusion

The D7/D8 framing ("deno_core yes, full Deno no, because async generators are not
transformable") is retracted. Async generators lower cleanly to constructs Hermes
supports, and the lowering can be inserted as a **source-to-source pass at v8x's
own compile boundary** (`Script::compile` / `CompileModule` in `src/hermes/`),
touching zero vendored Deno or vendored-test source.

So async generators are a *scalable wall*, not a ceiling. Full Deno on Hermes is
now a matter of grinding two independent surfaces:

1. the syntax-lowering pass (E1: land it in the backend, prove an async generator
   runs *through the backend*, not just compiles) — proven tractable here;
2. the large but ordinary op/Web-API surface of the full Deno runtime beyond
   deno_core (ext/web, ext/net, ext/fetch, ...), which is grind, not a blocker.

Next: E1 implements the lowering pass and proves end-to-end async iteration on the
Hermes backend.
