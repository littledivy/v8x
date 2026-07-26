#import "./shim/html.typ": *

#set document(
  title: "snapshots — v8x",
  description: "V8 startup snapshots rebuilt as record/replay: one serialized global graph plus a log that restores the native side.",
)

#show: html-shim

#crumb(5, [snapshots])

= Snapshots

QuickJS can serialize objects and bytecode — but not Rust callbacks,
template metadata, or embedder slots. So a snapshot is one serialized
global graph *plus a replay log* for the native side:

```text
context record = global object graph      # one graph, identity preserved
               + template descriptions
               + lexical globals
               + embedder/context slots
               + rooted-in-graph bitmap
host function  = HOSTDATA { index into rusty_v8 external-ref table }
```

Restore order matters:

+ create a fresh context
+ *disable GC*
+ read the global graph
+ rebuild template metadata, reconnect module exports
+ restore lexical bindings + embedder slots
+ re-enable GC, threshold ≥ 1.5× live size

GC stays off for exactly the window where JS graph edges exist but native
metadata is still disconnected — a collection there would sweep objects the
replay log is about to wire up.

System JSC exposes neither heap snapshots nor a bytecode-cache API:
snapshot creation is unavailable on that backend.

#next("wasm", [WebAssembly — WAMR behind V8's Wasm API])
