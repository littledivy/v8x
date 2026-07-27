# C3: Hermes backend runs a real script through the v8 C-ABI (hello world)

**Result: YES.** A v8x-level smoke test drives the vendored rusty_v8 Rust
surface (Isolate -> HandleScope -> Context -> String -> Script::compile ->
Script::run -> read string) and gets `"hello world"` back, executed on a real
libhermes through the v8 C-ABI. The test asserts the string and prints it:

```
hermes backend ran: 'hello' + ' ' + 'world' = "hello world"
test result: ok. 3 passed; 0 failed
```

This is the first time a script runs THROUGH the engine_hermes backend (not just
through the standalone C2 eval shim): the source is compiled and run by the
`v8__Script__Compile`/`v8__Script__Run` symbols our backend defines, and the
result is read back by the same `String`/`Value` read path the vendored surface
uses for every real V8 string.

## Test command and result

```bash
cargo test --no-default-features --features hermes,link_hermes \
  --lib hermes:: -- --nocapture
```

- `hermes::hello_world::hermes_backend_runs_hello_world` ... ok (the C3 proof)
- `hermes::tests::hermes_smoke_eval_40_plus_2` ... ok (C2, still green)
- `hermes::tests::hermes_smoke_catches_js_error` ... ok (C2, still green)

Use `--lib hermes::` (not the bare command in the earlier plan): the bare
`cargo test ... --features link_hermes` also builds the vendored integration
target `rv8_test_api`, which references hundreds of still-stubbed symbols and
fails to link, and the vendored crate's own inline `#[test]`s (array-buffer,
sandbox) hit stubs and abort the shared lib-test process. Scoping to
`--lib hermes::` runs exactly our three tests. This is a harness-scoping detail,
not a backend limitation; the hello-world path itself is complete.

## The handle-table / scope design

`jsi::Value` is a move-only 16-byte C++ struct that can only be created, copied,
and destroyed through its `Runtime`, and must not outlive it (the C2 lifetime
rule). So unlike the QuickJS backend, whose arena of boxed `JSValue`s lives in
Rust, the Hermes arena lives on the **C++ side**, inside the runtime wrapper:

- `src/hermes/hermes_shim.cpp` owns a `RuntimeWrapper { unique_ptr<jsi::Runtime>
  rt; vector<jsi::Value> handles; }`. The `rt` is declared first so it is
  destroyed last (Values die while the Runtime is alive).
- A v8 `Local` is an index into `handles`, handed to Rust as the tagged pointer
  `((index << 1) | 1)` so every live handle is a non-null `*const Data` and slot
  0 is still distinguishable from a null handle. `slot_of(ptr)` recovers it.
- A `HandleScope` is a watermark: `CONSTRUCT` records `handles.size()`,
  `DESTRUCT` calls `handles.resize(watermark)`, releasing every `jsi::Value`
  created since (while the Runtime is alive). This is QuickJS's
  handle-scope-pop, with the storage on the C++ side.
- One `HermesRuntime` per isolate, bound to the creating thread (a C2 rule). A
  thread-local tracks the current isolate and context, exactly like the QuickJS
  backend. There is one context per runtime, so a `*const Context` handle is
  just the isolate pointer reused.

The extern "C" bridge entry points (each wrapped in the C2 catch-all): runtime
new/free, `handles_len`/`handles_truncate` (scope watermark), `global`,
`string_new_utf8`, `run` (reads a source slot, `evaluateJavaScript`, pushes the
result slot), `value_to_utf8` (coerces via `Value::toString` and copies UTF-8
out), and `value_is_string`.

## v8__* symbols made real (in src/hermes/core.rs)

Replacing the auto-generated stubs (the stubs are now gated behind
`cfg(not(feature = "link_hermes"))` so the pure-Rust stub build is unchanged and
there is no duplicate-symbol collision):

- Isolate: `New`, `Dispose`, `Enter`, `Exit`, `GetCurrent`, `GetData`,
  `SetData`, `GetNumberOfDataSlots`, `GetCurrentContext`.
- Scope: `HandleScope__CONSTRUCT`/`DESTRUCT`, `EscapeSlot__reserve`/`escape`.
- Context: `New`, `Enter`, `Exit`, `Global`.
- String: `NewFromUtf8`, `NewFromOneByte`, `Length`, `Utf8Length`,
  `WriteUtf8_v2`, and the `ValueView` quintet
  (`CONSTRUCT`/`DESTRUCT`/`is_one_byte`/`data`/`length`) that
  `String::to_rust_string_lossy` actually reads through.
- Script: `Compile` (carries the source string's slot as the Script handle),
  `Run` (`evaluateJavaScript` via the shim).
- Value: `ToString`.
- Lifecycle needed just to bring an isolate up under the vendored
  `V8::initialize` + `Isolate::new(CreateParams::default())` path:
  `Platform__NewDefaultPlatform`, `V8__Initialize`/`InitializePlatform`/
  `Dispose`/`DisposePlatform`, `Platform__NotifyIsolateShutdown`, the
  `std__shared_ptr__v8__Platform__*` helpers, `CreateParams__SIZEOF`/`CONSTRUCT`,
  `ArrayBuffer__Allocator__NewDefaultAllocator`/`DELETE`, and the
  `std__shared_ptr__v8__ArrayBuffer__Allocator__*` helpers. These are inert
  markers (Hermes owns its own heap and the hello-world path never touches an
  ArrayBuffer); they mirror the `[obj, refcount]` / `[ptr, refcount]`
  shared-pointer word layout the QuickJS backend uses so the vendored
  `SharedPtrBase` bit-layout matches.

Everything else stays the existing `unimplemented!()` stub; only the symbols the
hello-world path touches were made real. I traced the read path empirically:
`Value::to_rust_string_lossy` -> `Value::ToString` -> `String::
to_rust_string_lossy` -> `ValueView` (not `WriteUtf8`), so `ValueView` is the
load-bearing read symbol, and I implemented `WriteUtf8_v2`/`Utf8Length` too for
the direct `String::to_rust_string` path.

## The one real correctness compromise (EscapeSlot__escape)

`EscapableHandleScope::escape` must move an escaping handle's value into a slot
that survives the child scope's watermark truncation. The QuickJS backend does
this by `JS_DupValue`ing the value into a pre-reserved parent slot. Our shim
does not yet expose a "duplicate slot N into a new slot" primitive, so
`escape()` currently re-materializes the escaping value **as a string**: it
reads the value's UTF-8 through `value_to_utf8` and interns a fresh string
handle above the watermark. For the hello-world path the escaping value is the
script result (a string), so this round-trips exactly. It is **lossy for
non-string Values** (a number/object would come back as its string form). This
is a known C3 limitation, flagged here and in the code; the clean fix is a
`handles_dup(rtw, slot) -> new_slot` shim entry that copies the `jsi::Value`
via its Runtime, which is a one-function follow-up.

## Regressions

- `cargo build --no-default-features --features hermes` (pure-Rust stub backend)
  compiles clean, unchanged: every made-real symbol's stub is gated behind
  `cfg(not(feature = "link_hermes"))`.
- `cargo build --no-default-features --features quickjs` (default backend)
  compiles clean.

## Recommended next step

Two candidates, in priority order:

1. **De-risk object identity (the deepest unchanged risk from C0).** JSI hands
   out no raw pointer, so two `Local`s to the same JS object are different table
   slots with different tagged pointers. Every V8 identity/hash site
   (`Value::SameValue`, `Object` identity, `Map`/`Set` keys, `Global` slot
   dedup) must reroute to `jsi::Runtime::strictEquals` or canonicalize to one
   interned slot per object. A small experiment: intern the same object twice,
   confirm the two slots are not pointer-equal, then wire `strictEquals` and
   show a `Set` with one logical member. This gates any broad surface work.
2. **Widen the surface toward the rusty_v8 harness.** Add the `handles_dup`
   shim to make `EscapeSlot__escape` exact, then `Value::Is*`/`To*`,
   `Object`/`Array` basics, and register the 4th backend in
   `tests/harness/config.json` with an empty baseline once `test_api` links, to
   start hill-climbing like the other backends.
