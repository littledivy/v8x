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

Anything built on rusty_v8, including `deno_core` and Deno itself, compiles
unchanged and runs on the engine you picked.

== Supported engines

#table(
  columns: 3,
  [*engine*], [*feature*], [*platforms*],
  [JavaScriptCore (WebKit JSCOnly, built from source)], [`jsc`], [macOS],
  [JavaScriptCore (Apple's system framework)], [`system_jsc`], [macOS],
  [QuickJS-ng (vendored, static)], [`quickjs`], [any],
)

One engine is active at a time. The usual reason to swap is binary size:

#table(
  columns: 3,
  [*engine*], [*deno binary*], [*engine size*],
  [V8 14.9], [78.7 MB], [\~40 MB static],
  [JSC (vendored)], [80.7 MB], [\~48 MB static],
  [system JSC], [54.2 MB], [0, ships with the OS],
  [quickjs-ng], [56.1 MB], [\~1 MB static],
)

== What runs today

- `deno_core` compiles and runs unchanged on all backends.
- Next.js 14 with full SSR runs on QuickJS and on vendored JSC, native
  `@next/swc` addon included.
- Express, Hono, and `deno repl` run on QuickJS. The repl talks to a real
  V8-inspector/CDP implementation.

Progress is tracked test by test on the #link("status/")[dashboard]. The
vendored rusty_v8 suite and the `deno_core` suite run unmodified against
each backend, and CI ratchets the passing set so it only grows. See
#link("hill-climb")[how the hill climb works].

#html.elem("div", attrs: (id: "chart", class: "chart"), "")
#html.elem("script", attrs: (src: "chart.js"), "")
#html.elem("script", "v8xChart(document.getElementById('chart'), 'status/')")

== How it's a drop-in

`v8x` vendors the real `v8` crate's Rust source verbatim and implements the
\~570 `v8__*` C ABI symbols its bindings call, on JSC or QuickJS instead of
V8. The Rust surface is exactly the crates.io API, so nothing downstream
has to change. #link("internals")[Read the internals.]
