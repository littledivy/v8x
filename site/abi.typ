#import "./shim/html.typ": *

#set document(
  title: "the ABI cut — v8x",
  description: "v8x swaps the implementation behind rusty_v8's ~570 generated C symbols. Same Rust API, different engine.",
)

#show: html-shim

#crumb(1, [the ABI cut])

= The ABI cut

The seam already exists: rusty_v8 is a Rust API on top of \~570
`extern "C"` symbols. v8x keeps everything above the symbols and replaces
everything below.

#fig("static/architecture.svg", "stock runtime vs v8x runtime: same deno_core and rusty_v8 Rust API; the v8__* native ABI is implemented by V8 on the left, by v8x (JavaScriptCore / QuickJS) on the right", width: "640")

`v8::String::new` still calls `v8__String__NewFromUtf8` — same symbol, new
implementation.

What makes it hard: the symbols owe the caller more than signatures.

+ `Local<T>` is pointer-shaped and dies with its handle scope
+ `Global<T>` outlives scopes and can go weak → GC roots
+ callbacks expect the current isolate + context restored
+ modules keep identity across compile/instantiate/evaluate
+ snapshots mix JS graphs with native callbacks

ECMAScript specifies none of these. Each one is a page in this series.

== The linker is the todo list

A missing `v8__*` symbol is a link error, not a runtime surprise. The
workflow:

+ point a real workload (Deno, a test suite) at a backend
+ collect the undefined symbols from the link error
+ define them — a stub is a legal first move
+ the stub turns "won't link" into "test fails", which the
  #link("hill-climb")[hill climb] can measure

#next("handles", [Handles — one `Local<T>`, two ownership models])
