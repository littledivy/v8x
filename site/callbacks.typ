#import "./shim/html.typ": *

#set document(
  title: "callbacks & exceptions — v8x",
  description: "How native callbacks cross the engine C frame into Rust: trampolines, IsoState, exception side state, and template records.",
)

#show: html-shim

#crumb(3, [callbacks & exceptions])

= Callbacks & exceptions

Every native callback crosses an engine C frame before reaching Rust. The
trampoline does five things, in order:

```text
engine calls C trampoline
  1. restore thread-local (isolate, context)    # many ABI fns get only a value ptr
  2. intern receiver + args                     # quickjs: arena slots
  3. build the FunctionCallbackInfo pointer layout rusty_v8 expects
  4. call the Rust callback                     # panics caught at this boundary
  5. translate the return-value slot back to an engine value
```

== Exceptions ride side state

- JSC: every throwing call records the pending exception (+ context) in
  `IsoState`
- QuickJS: `JS_TAG_EXCEPTION` → `JS_GetException`

`TryCatch`, `MaybeLocal`, and the message functions read that stored state —
they never ask the engine directly.

== IsoState: where the implicit V8 machinery lives

One struct behind the opaque `v8::Isolate` pointer:

#table(
  columns: 2,
  [*backend*], [*IsoState owns*],
  [JSC], [context group, global contexts, entered-context stack, protected
    locals, pending exception, weak records, GC callbacks],
  [QuickJS], [runtime, contexts, handle arena, persistent cells, module
    tables, snapshot state, promise hooks, interrupt state, memory counters],
)

Ordering quirk: QuickJS sizes each context's class table at
`JS_NewContext` — external-object and named-handler classes must register
*before* the first context exists.

== Templates

Neither engine has V8's template concept. The template *description*
(properties, accessors, callbacks, internal-field count) lives in a native
adapter record; instantiation reads it and builds the real engine object.
Internal fields go in backend-owned records tied to the engine object —
they hold raw native pointers and stay invisible to JS enumeration.
Function templates store the Rust callback + data and install the
trampoline above when materialized.

#next("modules", [Modules — identity across compile/instantiate/evaluate])
