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

```text
QuickJS:      module state, synthetic exports, namespaces
              keyed by the wrapper object's payload pointer (+ name map)
vendored JSC: native module record ptr  →  canonical Rust-facing wrapper
system JSC:   no public module hooks → closed graphs only, pre-flattened
              by a bundler (deno compile / desktop; no open loading)
```

The system-JSC row is a deliberate trade. A closed-graph restriction is
stronger than a partial emulation of hooks Apple's framework does not
expose, and it is much easier to test.

#next("snapshots", [Snapshots: record and replay])
