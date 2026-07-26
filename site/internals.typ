#import "./shim/html.typ": *

#set document(
  title: "internals — v8x",
  description: "How v8x retargets the rusty_v8 native ABI onto JavaScriptCore and QuickJS, one subsystem per page.",
)

#show: html-shim

= Internals

One idea: keep rusty_v8's Rust layer, reimplement its \~570 `v8__*` C
symbols per engine. Everything else follows from what those symbols owe the
caller. One page per subsystem:

+ #link("abi")[*The ABI cut*] — where the seam is, why the linker is the
  todo list
+ #link("handles")[*Handles*] — one `Local<T>`, two ownership models
+ #link("callbacks")[*Callbacks & exceptions*] — trampolines, side state,
  templates
+ #link("modules")[*Modules*] — identity across compile/instantiate/evaluate
+ #link("snapshots")[*Snapshots*] — record/replay, since engines can't
  serialize native state
+ #link("wasm")[*WebAssembly*] — WAMR behind V8's Wasm object API
+ #link("gaps")[*Closing gaps*] — forward → adapt → patch, and what gets
  upstreamed

Read in order — each page ends with a link to the next.
