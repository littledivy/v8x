#import "./shim/html.typ": *

#set document(
  title: "closing gaps · v8x",
  description: "Every v8__* symbol is classified: forwarded, emulated, normalized, patched, restricted, or unsupported.",
)

#show: html-shim

#crumb(7, [closing gaps])

= Closing gaps

Not every V8 behavior has an engine equivalent. Every `v8__*` symbol ends
up in one of six classes:

#table(
  columns: 2,
  [*class*], [*meaning*],
  [forwarded], [the engine has an equivalent public API; the symbol calls it],
  [emulated], [rebuilt in adapter side state: handles, contexts, templates,
    module maps],
  [normalized], [the source or the observable result is adjusted to match V8],
  [patched], [the engine is patched to expose state its public API hides],
  [restricted], [supported under a stated limit, like system JSC's
    closed-graph #link("modules")[module loading]],
  [unsupported], [declared unavailable, like #link("snapshots")[snapshots]
    on system JSC],
)

The first four are an escalation order: forward when the API exists,
emulate when it doesn't, normalize when behavior differs, and patch only
when required state is otherwise unreachable.

== The patches

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
