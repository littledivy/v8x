#import "./shim/html.typ": *

#set document(
  title: "v8x",
  description: "v8x makes rusty_v8 engine agnostic: run deno_core and Deno unchanged on JavaScriptCore or QuickJS.",
)

#show: html-shim

= v8x

`v8x` makes rusty_v8 engine agnostic. It is a drop-in replacement for the
`v8` crate that keeps the same Rust API and runs it on a different
JavaScript engine:

```diff
-v8 = "149.4.0"
+v8 = { package = "v8x", version = "149.4.0", features = ["jsc"] }
```

Anything built on rusty_v8, including `deno_core` and Deno itself, compiles
unchanged and runs on the engine you picked.

== Engines

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

== The hill climb

Compatibility is measured, not claimed. Two suites run unmodified against
every backend: the rusty_v8 integration tests, and `deno_core`'s own test
suite under nextest. When a test fails, the backend gets fixed, not the
test.

#html.elem("div", attrs: (id: "chart", class: "chart"), "")
#html.elem("script", attrs: (src: "chart.js"), "")
#html.elem("script", "v8xChart(document.getElementById('chart'), 'status/')")

Progress is ratcheted. Each backend and suite pair has a checked-in
baseline listing every test known to pass, and CI fails on a regression or
on unrecorded progress. The passing count can only move up. Live numbers
are on the #link("status/")[dashboard].

Contributing follows one loop: make a test pass, re-run the cell with
`--update` to record the new baseline, commit both, open a PR. The playbook
is in the repo's
#link("https://github.com/littledivy/v8x/blob/main/CLAUDE.md")[CLAUDE.md].

== Design

The work is not calling a different engine's API. It is preserving V8's
host semantics on engines that were never built for them: value ownership
across two garbage-collection models, module identity, exception state,
startup snapshots, WebAssembly. The #link("internals")[internals] pages
walk through each one.
