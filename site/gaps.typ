#import "./shim/html.typ": *

#set document(
  title: "closing gaps · v8x",
  description: "The escalation ladder for V8 semantics an engine doesn't expose: forward, adapt, normalize, or patch the engine.",
)

#show: html-shim

#crumb(7, [closing gaps])

= Closing gaps

When a `v8__*` symbol needs behavior the engine does not expose, there are
four moves, tried in order:

#fig("static/gaps.svg", "how v8x closes a semantic gap: 1 forward to an equivalent public API; 2 rebuild in adapter state; 3 normalize the source or observable result; 4 patch the engine to expose hidden state", width: "440")

+ forward: the engine has an equivalent public API, so call it
+ adapter state: rebuild the behavior in v8x's own records (handles,
  contexts, templates, module maps)
+ normalize: adjust the source or the observable result to match V8
+ patch: expose what the public API hides

Patches live in `patches/` and are applied to the submodules at build time:

#table(
  columns: 2,
  [*engine*], [*what the patches expose*],
  [quickjs-ng], [module requests, call-site and async-stack data,
    promise-handled state, native weak targets, extra serializable kinds,
    large ArrayBuffers, coverage boundaries],
  [WAMR], [linear-memory reservation, imported globals, reference types,
    module errors],
)

The policy is small expose-information patches only. They rebase easily and
can be sent upstream; several are landed or open against quickjs-ng. When
an engine grows an equivalent public hook, the patch is deleted. The stack
shrinks over time.

#next("hill-climb", [The hill climb: how progress is measured])
