#import "./shim/html.typ": *

#set document(
  title: "handles · v8x",
  description: "One pointer-shaped Local<T>, two ownership models: JSC protected pointers vs QuickJS one-refcount arena slots.",
)

#show: html-shim

#crumb(2, [handles])

= Handles

Both backends expose the same pointer-shaped `Local<T>`. They keep that
promise in very different ways.

#fig("static/handles.svg", "one Local<Value>, two ownership translations: JSC protects a JSValueRef pointer and unprotects at scope pop; QuickJS boxes a 16-byte JSValue into an owned slot and frees it at scope pop", width: "440")

== JSC: the value is the handle

`JSValueRef` is already a pointer, so it is used directly. Protect the
value, remember it, unprotect at scope exit:

```rust
// intern
JSValueProtect(ctx, v);
iso.locals.push((v, ctx));
return v as *const T;

// HandleScope::drop: pop to the watermark saved at scope entry
while iso.locals.len() > scope.watermark {
    let (v, ctx) = iso.locals.pop();
    JSValueUnprotect(ctx, v);
}
```

== QuickJS: the slot is the handle

A `JSValue` is 16 bytes of payload and tag, so the bits cannot themselves
be a pointer. Each value moves into a boxed arena slot and the slot address
becomes the handle. Every slot owns exactly one reference count:

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

Duplicating a fresh value would leak a reference. Moving a borrowed one
would free a value some other owner still holds. The one-count-per-slot
rule prevents both.

== Globals

```rust
// JSC: same pointer, protection-counted in a side map
protect_count[(v, ctx)] += 1;                                  // Global::new
if --protect_count[(v, ctx)] == 0 { JSValueUnprotect(ctx, v); } // Global::drop

// QuickJS: first field is a JSValue, so &cell.value doubles as a Local
struct PersistentCell { value: JSValue, ctx: *mut JSContext, iso: *mut IsoState }
```

== Weak handles

QuickJS keeps a native `WeakRef` next to the persistent cell, sweeps after
`JS_RunGC`, and fires the Rust callback for dead targets. JSC's public C
API has no per-object weak reference, so that backend records callbacks and
drains them after an explicit collection request.

Handle validity is exact on both backends. Collection timing is not: a
reference count reaching zero, a cycle pass, a tracing collection, and a V8
major GC are four different events, and v8x does not pretend otherwise.
Code that depends on finalizer order stays engine-sensitive.

#next("callbacks", [Callbacks and exceptions: trampolines and side state])
