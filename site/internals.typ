#import "./shim/html.typ": *

#set document(
  title: "internals — v8x",
  description: "How v8x retargets the rusty_v8 native ABI onto JavaScriptCore and QuickJS: handle adapters, isolate side state, callback trampolines, snapshot replay, and WebAssembly via WAMR.",
)

#show: html-shim

= Internals

== The trick: keep the Rust, replace the C

rusty_v8 is two layers: a Rust API surface, and \~570 `extern "C"` bindings
(`v8__Isolate__New`, `v8__String__NewFromUtf8`, …) that a C++ file implements
against real V8. `v8x` vendors the Rust layer *verbatim* — only 3 files
diverge from upstream, enforced by a sync tool — and defines the same native
symbols on a different engine. A call to `v8::String::new` still reaches
`v8__String__NewFromUtf8`; only what's behind the symbol changes.

#table(
  columns: 2,
  [*layer*], [*where it lives*],
  [vendored rusty_v8 Rust source (unmodified API)], [`vendor/rusty_v8/`],
  [JSC backend: `v8__*` implemented on JavaScriptCore's C API], [`src/jsc/`],
  [QuickJS backend: `v8__*` implemented on quickjs-ng], [`src/quickjs/`],
)

The hard part is that these symbols form a link boundary with *heap and host
obligations*, not just function signatures. `Local<T>` is pointer-shaped and
tied to a handle scope. `Global<T>` outlives scopes and can go weak. Native
callbacks expect the current isolate and context. Modules keep identity
across compile/instantiate/evaluate. Snapshots mix JS graphs with native
callbacks. ECMAScript specifies none of this — so the work splits into
*handle adapters* (value representation and ownership) and *adapter side
state* (the implicit V8 state the engine doesn't have).

== Isolate side state

An `IsoState` lives behind the opaque `v8::Isolate` pointer. Both backends
keep the current isolate and context in thread-local cells, because many ABI
functions receive only a value pointer. Constructing a handle scope selects
the isolate, refreshes the current context, and records a local-handle
watermark; every native callback restores the thread-locals before it calls
back into Rust.

What `IsoState` owns differs per engine. JSC: a context group, global
contexts, an entered-context stack, protected locals, pending exception,
weak records, GC callbacks. QuickJS: the runtime, contexts, the local-handle
arena, persistent cells, module tables, snapshot state, promise hooks,
interrupt state, memory counters.

One QuickJS quirk worth knowing: external-object and named-handler classes
must be registered *before* `JS_NewContext`, because QuickJS sizes each
context's class table from the runtime's class count at creation. So the
bootstrap context becomes the first V8 context, and each later V8 context
gets a fresh QuickJS context.

== Handle representations

The two collectors force two different `Local<T>` representations behind
the same pointer-shaped public type:

```text
JavaScriptCore: Local<T> = JSValueRef             (protected engine pointer)
QuickJS:        Local<T> = &ArenaSlot { JSValue }  (slot owns one refcount)
```

*JSC — direct pointers.* `JSValueRef` already is a pointer, so it's used
as-is. Interning a value calls `JSValueProtect`, records (value, owning
context) in the isolate's local vector, and casts the address to
`*const T`. Scope destruction pops back to the watermark, calling
`JSValueUnprotect` per entry. No wrapper allocation; validity comes from the
protection record. The tracing collector gets an explicit root and keeps its
own collection schedule.

*QuickJS — arena slots.* A `JSValue` is 16 bytes of payload + tag; returning
those bits as a pointer would turn payload into an address. Instead each
value moves into a boxed arena slot and the slot's address is the handle.
Ownership is the invariant: a *fresh* engine return value moves into the
slot; a *borrowed* value is `JS_DupValue`'d first. Each slot owns exactly
one reference count. Scope exit walks the arena back to its watermark,
`JS_FreeValue`s each slot, and frees the box. Duplicating a fresh result
would leak a count; moving a borrowed one would free somebody else's value —
the one-reference-per-slot rule prevents both.

== Globals and weak handles

JSC globals keep the engine pointer as the handle; a thread-local map holds
(protecting context, protection count) per pointer, protecting on create and
unprotecting when the count hits zero. QuickJS globals allocate a
`PersistentCell { value, context, isolate }` and return the address of the
first field — which is a `JSValue`, so the same address still reads as a
local handle.

Weak handles need collector-specific paths. QuickJS creates a native
`WeakRef` beside the cell and checks targets after `JS_RunGC`, firing the
Rust callback for the dead ones. JSC's public C API has no per-object weak
reference, so that backend records callbacks and drains them after an
explicit collection request — reachability timing differs from V8, and
that's a documented backend property, not a bug to paper over.

More generally: handle *validity* has a precise adapter invariant, but
collection *timing* doesn't. Refcount-zero, a QuickJS cycle pass, JSC
tracing, and a V8 major GC are not the same event, and `v8x` doesn't pretend
they are. Code that depends on finalizer order stays engine-sensitive.

== Callbacks and exceptions

A native callback crosses an engine C frame before reaching Rust. The
trampoline restores the thread-local isolate + context, interns receiver and
arguments (the QuickJS path allocates arena slots for each), builds the
exact pointer layout `FunctionCallbackInfo` expects, calls the Rust
function, then translates the return-value slot back into an engine value.
Rust panics are caught at the C boundary rather than unwinding through
engine frames.

Exceptions ride adapter side state: JSC records the pending exception and
its context in `IsoState`; QuickJS consumes `JS_TAG_EXCEPTION` via
`JS_GetException`. `TryCatch`, `MaybeLocal`, and the message functions all
read that stored state. Message text, source locations, and stack frames
come from whichever engine produced them — where the public API doesn't
expose enough (async stack data, promise-handled state), the QuickJS build
patches the engine.

== Templates and internal fields

V8 templates describe properties, accessors, callbacks, and internal fields
before any object exists. Neither engine has that concept, so the template
*description* is stored in a native adapter record and instantiation reads
it to build the real engine object or function. Internal fields can't be
ordinary JS properties (hosts stash native pointers there and expect
enumeration to skip them), so they live in backend-owned records associated
with the engine object. Function templates store the Rust callback + data
and install the trampoline when materialized.

== Module identity

V8 lets the host compile a module, inspect its requests, instantiate with a
resolver, set `import.meta`, and intercept dynamic `import()`. Each backend
gets a canonical module wrapper so `deno_core` always sees the identity it
stored: QuickJS keys module state, synthetic exports, and namespaces by the
wrapped object's payload pointer, with a name map retaining the canonical
wrapper; vendored JSC maps native module-record pointers to the same
Rust-facing wrapper.

Apple's *system* JavaScriptCore framework exposes a much narrower module
API, so that backend takes a different deal: closed module graphs only,
flattened by a bundler before execution. That restriction is stronger — and
much easier to test — than a partial emulation of the missing hooks. It fits
bundled apps (`deno compile`, desktop) and excludes open module loading.

== Snapshots are record/replay

V8 startup snapshots have no engine equivalent. QuickJS can serialize
objects and bytecode, but its encoding omits Rust callbacks, template
metadata, and embedder slots — exactly the things a `deno_core` snapshot
needs. So a `v8x` snapshot is one serialized global-object graph *plus a
replay log* that restores the native side.

Per context the record holds: the global graph (serialized as one graph so
repeated references keep identity), template descriptions, lexical globals,
embedder-data and context-data slots, and a bitmap of values already rooted
in the graph. Host functions are tagged `HOSTDATA` records storing their
index in the rusty_v8 external-reference table. Restore initializes a
context, *disables GC*, reads the graph, rebuilds template metadata,
reconnects module exports, restores lexical bindings and embedder slots,
then re-enables collection with a threshold ≥1.5× the current allocation —
GC stays off precisely while JS graph edges exist but native metadata is
still disconnected.

System JSC exposes neither heap snapshots nor a public bytecode-cache API;
snapshot creation is marked unavailable there.

== WebAssembly is a second runtime boundary

The ABI also reaches V8's WebAssembly objects — `deno_core` expects
constructors and accessors for modules, instances, memories, tables, and
globals. quickjs-ng has no compatible Wasm surface, so the QuickJS backend
embeds #link("https://github.com/bytecodealliance/wasm-micro-runtime")[WAMR]
and wraps its native objects in JS objects. That creates a second ownership
boundary: a JS wrapper must retain its native Wasm object until the engine
collects the wrapper; imported functions cross Wasm → QuickJS callback → the
same Rust trampoline as ordinary functions; memory objects keep the JS
`ArrayBuffer` view in sync as native linear memory grows.

== Engine patches

Where a public engine API hides host state, the build carries small patches
(in `patches/`): QuickJS patches expose module requests, call-site and
async-stack data, promise-handled state, native weak targets, extra
serializable object kinds, larger ArrayBuffers, and coverage boundaries;
WAMR patches cover linear-memory reservation, imported globals, reference
types, and module errors. Small expose-information patches rebase easily and
get PR'd upstream (several are landed or open against quickjs-ng); the patch
stack shrinks over time.

== The linker is the todo list

A missing `v8__*` symbol is a link error, not a runtime surprise. That makes
the linker a free completeness checker: point a real workload (Deno, a test
suite) at a backend, collect the undefined symbols, implement them. Stubs
are legal first moves — a stubbed symbol turns a build failure into a
failing test, which is progress the #link("hill-climb")[hill climb] can
measure.
