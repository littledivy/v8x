#import "./shim/html.typ": *

#set document(
  title: "internals · v8x",
  description: "What makes a runtime engine-specific, and how v8x rebuilds each obligation on JavaScriptCore and QuickJS.",
)

#show: html-shim

= Internals

JavaScript is portable; runtimes are not. What ties a runtime to its engine
is everything it asks for beyond the language: who owns a value and for how
long, how native code gets called, what a module is, what a snapshot
captures. `deno_core` asks for all of it through rusty_v8, and v8x answers
with a different engine underneath. One page per obligation:

+ #link("architecture")[Architecture]: the boundary v8x preserves
+ #link("handles")[Handles]: value ownership across two garbage collectors
+ #link("callbacks")[Callbacks and exceptions]: crossing the native
  boundary without losing state
+ #link("modules")[Modules]: keeping identity across compile, instantiate,
  evaluate
+ #link("snapshots")[Snapshots]: booting from state no engine can
  serialize alone
+ #link("wasm")[WebAssembly]: a second runtime behind the same object API
+ #link("gaps")[Closing gaps]: what gets forwarded, emulated, patched, or
  refused

The pages read best in order. Each one links to the next.
