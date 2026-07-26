#import "./shim/html.typ": *

#set document(
  title: "internals · v8x",
  description: "How v8x retargets the rusty_v8 native ABI onto JavaScriptCore and QuickJS, one subsystem per page.",
)

#show: html-shim

= Internals

The idea is small: keep rusty_v8's Rust layer and reimplement its \~570
`v8__*` C symbols on each engine. Everything else follows from what those
symbols promise the caller. One page per subsystem:

+ #link("architecture")[Architecture]: the boundary, and why missing
  symbols are link errors
+ #link("handles")[Handles]: one `Local<T>`, two ownership models
+ #link("callbacks")[Callbacks and exceptions]: trampolines, side state,
  templates
+ #link("modules")[Modules]: identity across compile, instantiate, evaluate
+ #link("snapshots")[Snapshots]: record and replay, because engines cannot
  serialize native state
+ #link("wasm")[WebAssembly]: WAMR behind V8's Wasm object API
+ #link("gaps")[Closing gaps]: forward, adapt, normalize, patch

The pages read best in order. Each one links to the next.
