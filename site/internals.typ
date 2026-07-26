#import "./shim/html.typ": *

#set document(
  title: "internals — v8x",
  description: "How v8x retargets the rusty_v8 native ABI onto JavaScriptCore and QuickJS: handle adapters, isolate side state, callback trampolines, snapshot replay, and WebAssembly via WAMR.",
)

#show: html-shim

= Internals

== The cut

```text
  your crate / deno_core / Deno        │ unchanged
  ─────────────────────────────────────┤
  vendor/rusty_v8   (Rust API surface) │ vendored verbatim, 3 files diverge
  ─────────────────────────────────────┤
        ~570 extern "C" v8__* symbols  │ ← the ABI boundary
  ─────────────────────────────────────┤
  src/jsc/            src/quickjs/     │ exactly one linked per build
  JavaScriptCore      quickjs-ng + WAMR│
```

`v8::String::new` still calls `v8__String__NewFromUtf8`; only what's behind
the symbol changes. The catch: these symbols carry *heap and host
obligations*, not just signatures — handle lifetimes, GC roots, current
context, module identity, snapshots. ECMAScript specifies none of it.

== Handle scopes

Two collectors, two `Local<T>` representations behind the same
pointer-shaped public type.

JSC — `JSValueRef` is already a pointer, use it directly. Root it, remember
it, unroot at scope exit:

```rust
// intern: Local<T> == the JSValueRef itself
JSValueProtect(ctx, v);
iso.locals.push((v, ctx));
return v as *const T;

// HandleScope::drop — pop to the watermark saved at scope entry
while iso.locals.len() > scope.watermark {
    let (v, ctx) = iso.locals.pop();
    JSValueUnprotect(ctx, v);
}
```

QuickJS — a `JSValue` is 16 bytes of payload + tag; those bits can't *be* a
pointer. Box it in an arena slot; the slot's address is the handle, and each
slot owns exactly **one** refcount:

```rust
// fresh engine result: move it into the slot
fn intern_fresh(v: JSValue) -> Local<T>    { arena.push(slot(v)) }
// borrowed value: take our own count first
fn intern_borrowed(v: JSValue) -> Local<T> { arena.push(slot(JS_DupValue(ctx, v))) }

// HandleScope::drop
while arena.len() > scope.watermark {
    JS_FreeValue(ctx, arena.pop().value);
}
```

Dup a fresh value → leak one count. Move a borrowed one → free somebody
else's value. The one-count-per-slot rule is the whole invariant.

== Globals and weak handles

```rust
// JSC: same pointer as the handle, protection-counted in a side map
protect_count[(v, ctx)] += 1;            // Global::new
if --protect_count[(v, ctx)] == 0 { JSValueUnprotect(ctx, v); }

// QuickJS: first field is a JSValue, so &cell.value doubles as a Local
struct PersistentCell { value: JSValue, ctx: *mut JSContext, iso: *mut IsoState }
```

Weak handles: QuickJS keeps a native `WeakRef` next to the cell and sweeps
after `JS_RunGC`, firing the Rust callback for dead targets. JSC's public C
API has no per-object weak reference — callbacks drain after an explicit GC
request instead. Handle *validity* is exact; collection *timing* is a
backend property (refcount-zero ≠ cycle pass ≠ tracing ≠ V8 major GC).

== Callback trampoline

```text
engine calls C trampoline
  → restore thread-local (isolate, context)     # many ABI fns get only a value ptr
  → intern receiver + args                      # quickjs: arena slots
  → build the FunctionCallbackInfo pointer layout rusty_v8 expects
  → call the Rust callback                      # panics caught at this boundary
  → translate the return-value slot back to an engine value
```

Exceptions ride side state: JSC records the pending exception in `IsoState`;
QuickJS turns `JS_TAG_EXCEPTION` into `JS_GetException`. `TryCatch`,
`MaybeLocal`, and message functions read that stored state.

== Isolate side state

All of the implicit V8 machinery lives in an `IsoState` behind the opaque
`v8::Isolate` pointer:

#table(
  columns: 2,
  [*backend*], [*IsoState owns*],
  [JSC], [context group, global contexts, entered-context stack, protected
    locals, pending exception, weak records, GC callbacks],
  [QuickJS], [runtime, contexts, handle arena, persistent cells, module
    tables, snapshot state, promise hooks, interrupt state, memory counters],
)

One ordering quirk: QuickJS sizes each context's class table at
`JS_NewContext`, so external-object and named-handler classes register
*before* the first context exists.

== Templates and internal fields

Neither engine has V8's template concept, so the template *description*
(properties, accessors, callbacks, internal-field count) lives in a native
adapter record; instantiation reads it and builds the real engine object.
Internal fields go in backend-owned records tied to the engine object — they
hold raw native pointers and must stay invisible to JS enumeration.

== Module identity

`deno_core` stores a module wrapper and expects the *same* identity back
from every hook:

```text
QuickJS:      module state, synthetic exports, namespaces
              keyed by the wrapper object's payload pointer (+ name map)
vendored JSC: native module record ptr  →  canonical Rust-facing wrapper
system JSC:   no public module hooks → closed graphs only, pre-flattened
              by a bundler (deno compile / desktop; no open loading)
```

== Snapshots are record/replay

QuickJS can serialize objects and bytecode, but not Rust callbacks, template
metadata, or embedder slots. So a snapshot is one serialized global graph
*plus a replay log* for the native side:

```text
context record = global object graph      # one graph, identity preserved
               + template descriptions
               + lexical globals
               + embedder/context slots
               + rooted-in-graph bitmap
host function  = HOSTDATA { index into rusty_v8 external-ref table }

restore: new context → GC OFF → read graph → rebuild templates
         → reconnect module exports → restore slots
         → GC ON (threshold ≥ 1.5× live size)
```

GC is off exactly while JS graph edges exist but native metadata is still
disconnected. System JSC has no heap-snapshot or bytecode-cache API —
snapshot creation is unavailable there.

== WebAssembly

quickjs-ng has no Wasm surface, so the QuickJS backend embeds
#link("https://github.com/bytecodealliance/wasm-micro-runtime")[WAMR] behind
V8's Wasm object API — a second ownership boundary:

```text
JS wrapper ──retains──▶ native WAMR object      (until engine collects wrapper)
wasm import call ──▶ QuickJS callback ──▶ same Rust trampoline as JS fns
memory.grow ──▶ JS ArrayBuffer view re-synced to new linear memory
```

== Engine patches

Where the public engine API hides host state, `patches/` fills the gap:
QuickJS — module requests, call-site/async-stack data, promise-handled
state, native weak targets, extra serializable kinds, large ArrayBuffers;
WAMR — linear-memory reservation, imported globals, reference types, module
errors. Small expose-information patches rebase easily and get PR'd
upstream; the stack shrinks over time.

== The linker is the todo list

A missing `v8__*` symbol is a link error, not a runtime surprise — a free
completeness checker. Point a real workload at a backend, collect undefined
symbols, implement them. Stubs are legal first moves: a stub turns a build
failure into a failing test, which the #link("hill-climb")[hill climb] can
measure.
