#import "./shim/html.typ": *

#set document(
  title: "modules · v8x",
  description: "Canonical module wrappers keep V8 module identity stable across compile, instantiate, and evaluate on JSC and QuickJS.",
)

#show: html-shim

#crumb(4, [modules])

= Modules

`deno_core` compiles a module, inspects its requests, instantiates it with
a resolver, sets `import.meta`, and intercepts dynamic `import()`. It
expects the same wrapper identity back from every one of those hooks, so
each backend keeps a canonical wrapper:

#table(
  columns: 2,
  [*backend*], [*how identity is kept*],
  [QuickJS], [module state, synthetic exports, and namespaces are keyed by
    the wrapper object's payload pointer; a name map retains the canonical
    wrapper],
  [vendored JSC], [native module records map to the same Rust-facing
    wrapper],
  [system JSC], [no public module hooks, so only closed graphs, flattened
    by a bundler before execution; fits `deno compile` and desktop apps,
    rules out open module loading],
)

The system-JSC row is a deliberate trade. A closed-graph restriction is
stronger than a partial emulation of hooks Apple's framework does not
expose, and it is much easier to test.

#next("snapshots", [Snapshots: record and replay])
