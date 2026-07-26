#import "./shim/html.typ": *

#set document(
  title: "architecture · v8x",
  description: "The rusty_v8 binding is the portability boundary: everything above it is unchanged, everything below it is per-engine.",
)

#show: html-shim

#crumb(1, [architecture])

= Architecture

An engine executes JavaScript. A runtime needs much more than that: it
owns values across garbage collection, calls native code from JavaScript,
resolves modules, boots from snapshots. rusty_v8 encodes those needs as a
Rust API over a generated native boundary, and V8 has always been the only
thing on the other side.

v8x treats that boundary as a portability layer:

#fig("static/architecture.svg", "layered diagram: host runtime and the rusty_v8 Rust API sit above the generated native ABI line; each build selects one v8x adapter below it, JavaScriptCore (tracing GC, protected pointers) or QuickJS (reference counts, tagged values)", width: "680")

Above the line nothing changes: the same crate, types, and lifetimes, so
`deno_core` compiles as-is. Below the line each build pairs one engine with
an adapter. The engine executes JavaScript; the adapter supplies what the
engine does not have, which is V8's model of ownership, context, identity,
and startup state.

The two backends make the split concrete. JavaScriptCore brings a tracing
collector, so the adapter roots values by protecting pointers. QuickJS
brings reference counting and tagged values, so the adapter owns counts in
slots it controls. Same API above, two disciplines below. The rest of the
series is about keeping V8's promises on top of each.

== Missing pieces surface at link time

The boundary has a useful mechanical property: a workload that needs an
unimplemented piece fails to link, naming exactly what is missing. Deno or
a test suite becomes a completeness probe. A stub is a fine first
implementation, because it converts "won't build" into a failing test that
the #link("./")[hill climb] can count.

#next("handles", [Handles: value ownership across two garbage collectors])
