#import "./shim/html.typ": *

#set document(
  title: "v8x",
  description: "v8x makes rusty_v8 engine agnostic: a drop-in v8 ABI compatibility layer over JavaScriptCore and QuickJS.",
)

#show: html-shim

= v8x

`v8x` makes rusty_v8 engine agnostic. It is a drop-in replacement for the
`v8` crate that implements the same Rust API on top of a pluggable JavaScript
engine:

```diff
-v8 = "149.4.0"
+v8 = { package = "v8x", version = "149.4.0", features = ["jsc"] }
```

Anything built on rusty_v8 — including `deno_core` and Deno itself — compiles
unchanged and runs on the engine you picked.

== Supported engines

#table(
  columns: 3,
  [*engine*], [*feature*], [*platforms*],
  [JavaScriptCore (WebKit JSCOnly, built from source)], [`jsc`], [macOS],
  [JavaScriptCore (Apple's system framework)], [`system_jsc`], [macOS],
  [QuickJS-ng (vendored, static)], [`quickjs`], [any],
)

Exactly one engine is active at a time. Why bother? Binary size, mostly:

#table(
  columns: 3,
  [*engine*], [*deno binary*], [*engine size*],
  [V8 14.9], [78.7 MB], [\~40 MB static],
  [JSC (vendored)], [80.7 MB], [\~48 MB static],
  [system JSC], [54.2 MB], [0 — ships with the OS],
  [quickjs-ng], [56.1 MB], [\~1 MB static],
)

== What runs today

- `deno_core` compiles and runs unchanged on all backends.
- Next.js 14 dev server with full SSR — including the native `@next/swc`
  napi addon — on both QuickJS and vendored JSC.
- Express, Hono, and `deno repl` (backed by a real V8-inspector/CDP
  implementation) on QuickJS.

Progress is tracked test-by-test on the #link("status/")[public dashboard]:
the vendored rusty_v8 test suite and `deno_core`'s test suite run as-is
against each backend, and CI ratchets the set of passing tests so it only
ever grows. #link("hill-climb")[How the hill climb works.]

#html.elem("div", attrs: (id: "chart", class: "chart"), "")
#html.elem("script", attrs: (src: "chart.js"), "")
#html.elem("script", "v8xChart(document.getElementById('chart'), 'status/')")

== How it's a drop-in

`v8x` vendors the real `v8` crate's Rust source verbatim and implements the
\~570 `v8__*` C ABI symbols that its bindings call — on JSC or QuickJS
instead of V8. The Rust surface is bit-for-bit the crates.io API; only the
native layer underneath changes. #link("internals")[Read the internals.]
