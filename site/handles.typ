#import "./shim/html.typ": *

#set document(
  title: "handles — v8x",
  description: "One pointer-shaped Local<T>, two ownership models: JSC protected pointers vs QuickJS one-refcount arena slots.",
)

#show: html-shim

#crumb(2, [handles])

= Handles

Same public `Local<T>`, two engine representations. The figure is the whole
story; the code below is how each side keeps its promise.

#fig("static/handles.svg", "one Local<Value>, two ownership translations: JSC protects a JSValueRef pointer and unprotects at scope pop; QuickJS boxes a 16-byte JSValue into an owned slot and frees it at scope pop", width: "440")

== JSC: the value is the handle

`JSValueRef` is already a pointer. Protect it, remember it, unprotect at
scope exit:

```rust
// intern
JSValueProtect(ctx, v);
iso.locals.push((v, ctx));
return v as *const T;

// HandleScope::drop — pop to the watermark saved at scope entry
while iso.locals.len() > scope.watermark {
    let (v, ctx) = iso.locals.pop();
    JSValueUnprotect(ctx, v);
}
```

== QuickJS: the slot is the handle

A `JSValue` is 16 bytes of payload + tag — those bits can't be a pointer.
Box it; the slot address is the handle. *Each slot owns exactly one
refcount:*

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

Break the rule either way and you lose: dup a fresh value → leak one count;
move a borrowed one → free somebody else's value.

== Globals

```rust
// JSC: same pointer, protection-counted in a side map
protect_count[(v, ctx)] += 1;                                  // Global::new
if --protect_count[(v, ctx)] == 0 { JSValueUnprotect(ctx, v); } // Global::drop

// QuickJS: first field is a JSValue, so &cell.value doubles as a Local
struct PersistentCell { value: JSValue, ctx: *mut JSContext, iso: *mut IsoState }
```

== Weak handles

- QuickJS: native `WeakRef` beside the cell; sweep after `JS_RunGC`, fire
  the Rust callback for dead targets
- JSC: no per-object weak reference in the public C API — callbacks drain
  after an explicit GC request

Handle *validity* is exact on both. Collection *timing* is a backend
property: refcount-zero ≠ cycle pass ≠ tracing ≠ V8 major GC. Code that
depends on finalizer order stays engine-sensitive.

#next("callbacks", [Callbacks & exceptions — trampolines and side state])
