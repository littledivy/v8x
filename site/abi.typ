#import "./shim/html.typ": *

#set document(
  title: "the ABI cut · v8x",
  description: "v8x swaps the implementation behind rusty_v8's ~570 generated C symbols. Same Rust API, different engine.",
)

#show: html-shim

#crumb(1, [the ABI cut])

= The ABI cut

The seam already exists. rusty_v8 is a Rust API on top of \~570
`extern "C"` symbols, and v8x keeps everything above the symbols while
replacing everything below them.

#fig("static/architecture.svg", "stock runtime vs v8x runtime: same deno_core and rusty_v8 Rust API; the v8__* native ABI is implemented by V8 on the left, by v8x (JavaScriptCore / QuickJS) on the right", width: "640")

A call to `v8::String::new` still reaches `v8__String__NewFromUtf8`. The
symbol is the same; the implementation behind it is new.

The difficulty is that these symbols promise more than their signatures:

+ `Local<T>` is pointer-shaped and dies with its handle scope
+ `Global<T>` outlives scopes and can go weak, which means GC roots
+ callbacks expect the current isolate and context to be restored
+ modules keep their identity across compile, instantiate, and evaluate
+ snapshots mix JavaScript graphs with native callbacks

ECMAScript specifies none of this. Each item is a page in this series.

== The linker is the todo list

A missing `v8__*` symbol is a link error, not a runtime surprise. That
turns the linker into a completeness checker:

+ point a real workload (Deno, a test suite) at a backend
+ collect the undefined symbols from the link error
+ define them; a stub is a fine first move
+ the stub turns "won't link" into "test fails", which the
  #link("hill-climb")[hill climb] can measure

#next("handles", [Handles: one `Local<T>`, two ownership models])
