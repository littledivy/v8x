#import "./shim/html.typ": *

#set document(
  title: "callbacks & exceptions · v8x",
  description: "How native callbacks cross the engine C frame into Rust: trampolines, IsoState, exception side state, and template records.",
)

#show: html-shim

#crumb(3, [callbacks and exceptions])

= Callbacks and exceptions

Every native callback crosses an engine C frame before it reaches Rust.
The trampoline does five things, in order:

+ restore the thread-local isolate and context; many ABI functions receive
  only a value pointer, so this state must already be in place
+ intern the receiver and arguments, which on QuickJS allocates arena slots
+ build the exact `FunctionCallbackInfo` pointer layout rusty_v8 expects
+ call the Rust callback, catching panics so they never unwind through
  engine frames
+ translate the return-value slot back into an engine value

== Exceptions live in side state

JSC records the pending exception and its context in `IsoState` on every
call that can throw. QuickJS turns `JS_TAG_EXCEPTION` into
`JS_GetException`. `TryCatch`, `MaybeLocal`, and the message functions read
that stored state; they never ask the engine directly.

== IsoState

The implicit V8 machinery lives in one struct behind the opaque
`v8::Isolate` pointer:

#table(
  columns: 2,
  [*backend*], [*IsoState owns*],
  [JSC], [context group, global contexts, entered-context stack, protected
    locals, pending exception, weak records, GC callbacks],
  [QuickJS], [runtime, contexts, handle arena, persistent cells, module
    tables, snapshot state, promise hooks, interrupt state, memory counters],
)

One ordering rule: QuickJS sizes each context's class table at
`JS_NewContext`, so external-object and named-handler classes register
before the first context exists.

== Templates

Neither engine has V8's template concept. The template description
(properties, accessors, callbacks, internal-field count) lives in a native
adapter record, and instantiation reads it to build the real engine object.
Internal fields go in backend-owned records tied to the engine object; they
hold raw native pointers and stay invisible to JavaScript enumeration.
Function templates store the Rust callback and its data, then install the
trampoline above when materialized.

#next("modules", [Modules: identity across compile, instantiate, evaluate])
