#import "./shim/html.typ": *

#set document(
  title: "modules — v8x",
  description: "Canonical module wrappers keep V8 module identity stable across compile, instantiate, and evaluate on JSC and QuickJS.",
)

#show: html-shim

#crumb(4, [modules])

= Modules

`deno_core` compiles a module, inspects its requests, instantiates it with
a resolver, sets `import.meta`, intercepts dynamic `import()` — and expects
the *same* wrapper identity back from every hook. Each backend keeps a
canonical wrapper:

```text
QuickJS:      module state, synthetic exports, namespaces
              keyed by the wrapper object's payload pointer (+ name map)
vendored JSC: native module record ptr  →  canonical Rust-facing wrapper
system JSC:   no public module hooks → closed graphs only, pre-flattened
              by a bundler (deno compile / desktop; no open loading)
```

The system-JSC row is a deliberate trade: a closed-graph restriction is
stronger — and far easier to test — than a partial emulation of hooks
Apple's framework doesn't expose.

#next("snapshots", [Snapshots — record/replay])
