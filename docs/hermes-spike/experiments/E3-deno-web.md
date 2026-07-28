# E3 — deno_webidl + deno_web init runs end-to-end on Hermes

Goal: stand up `deno_webidl` + `deno_web` on `deno_core` on the Hermes backend,
get their init JS to EXECUTE, and name the first wall. Result: **all 24 of
ext/web's init JS files execute without throwing**, and the first wall in actual
API USE is named (the `v8::Private` subsystem). This blew past "name the first
wall" — the whole ext/web init surface hydrates.

## Architecture note (this Deno version): init is lazy, not eager

`deno_web`'s `extension!` macro in this checkout ships ALL its JS as
`lazy_loaded_js` / `lazy_loaded_esm`, not eager `esm`/`js`. So NOTHING runs at
`JsRuntime::new`. The runtime hydrates each file on demand through
`Deno.core.loadExtScript("ext:deno_web/NN_*.js")`; each file pulls its own deps
the same way (e.g. `00_infra.js` calls `loadExtScript("ext:deno_web/00_url.js")`).
The probe drives the 24 files in numeric order; each `loadExtScript` runs the
file through the Hermes compile boundary (`compile_function`) and evaluates it.

Probe: `libs/hermes_web_probe` (a new workspace crate, bin `hermes_web`) in the
deno sandbox. Registers `deno_webidl::init()` + `deno_web::init(BlobStore,
None, false, InMemoryBroadcastChannel)`, boots, then drives every ext file.

Run: `DYLD_FRAMEWORK_PATH=/Users/divy/gh/v82jsc/vendor/hermes ./target/debug/hermes_web`

## (1) How far init got: ALL of it

```
BOOT OK: JsRuntime::new with deno_webidl + deno_web succeeded
  ok  ext:deno_webidl/00_webidl.js
  ok  ext:deno_web/00_infra.js ... 18_css_stylesheet.js   (all 24)
ALL 24 deno_web lazy_loaded_js files evaluated without throwing
```

Functional probes prove the init did real work (not silent no-ops):

| probe | result |
|---|---|
| `atob`/`btoa` exports | `["atob","btoa"]` |
| `new Event("test",{cancelable:true})` | `"test:true"` |
| `new ReadableStream(...).getReader` | `"function"` |
| `new TextEncoder().encode("héllo")` | THREW (see wall #2 below) |

Event + WebIDL + the 8000-line `06_streams.js` (ReadableStream) are functional.

## (2) The walls, named concretely (each was a backend gap)

Driving the files surfaced six backend gaps in sequence. Each was a real gap
(null-returning shim stub or an incorrect lifetime/exception behavior), fixed in
`src/hermes/` — none was deno-layer wiring.

1. **`v8__ScriptCompiler__CompileFunction` — null stub.** The primitive
   `loadExtScript` uses to compile every ext file. Returned null, so
   `compile_function` returned `None` and deno_core panicked at
   `modules/map.rs:3434` (`tc_scope.exception().unwrap()` on `None`).

2. **Global pin lifetime (refcount) — `v8__Local__New` -> null -> panic.**
   After CompileFunction landed, the 2nd `loadExtScript` panicked at
   `handle.rs:147`. Root cause: a v8 `Global` is refcounted (`Global::clone`
   re-wraps the SAME pin handle; each `Drop` calls `Global::Reset` -> `unpin`).
   deno_core clones `captured_bootstrap` per load; the clone's drop freed pin 2,
   so the next `Local::new(captured_bootstrap)` read a dangling pin.

3. **Compile errors escaped the TryCatch.** A Hermes parse error is a
   `jsi::JSIException`, NOT a `jsi::JSError` (no JS value), so `v8x_hermes_run`'s
   `catch(const jsi::JSError&)` missed it -> `HasCaught()` false ->
   `Exception()` empty -> `map.rs:3434` unwrap panic (again, different cause).

4. **`v8__Exception__CreateMessage` — null stub.** `from_v8_exception` unwraps
   `create_message(...)` (`exception.rs:430`).

5. **`v8__String__Empty` — null stub.** The error formatter builds an empty
   string via `String::empty` (`string.rs:465`), treated as infallible.

6. **`v8__Object__GetPrototype` — null stub.** `is_instance_of_error`
   (`error.rs:1395`) walks the prototype chain to brand thrown exceptions.

### The load-bearing subtlety (wall #1's real teeth)

Once the stubs were filled, `ext:deno_web/09_file.js` still failed with the
EXACT E1 syntax wall: `Compiling JS failed: 125:1: async generators are
unsupported`. deno_core wraps each ext script as `"use strict"; return
(<IIFE>);` and hands THAT to `compile_function` as a function BODY. The first
CompileFunction impl ran the async-generator lowering on the bare body, but oxc
rejects a top-level `return` (valid only inside a function), so it silently
passed the source through unchanged and Hermes saw the raw `async function*`.
Fix: wrap into `(function (<params>) { <body> })` FIRST, then lower the whole
expression. `09_file.js` (Blob/File, `async *stream()` + `async function*
toIterator`) then compiled and ran — the E1 lowering carrying real ext/web.

## (3) Classification: all backend gaps, zero deno-layer stubs

Every wall was a genuine v8x Hermes backend gap. No op was stubbed at the deno
layer; no Deno test file was touched. The probe crate and its Cargo wiring are
the only sandbox additions.

## (4) Backend fixes (v8x, branch `hermes-backend-spike`, NOT pushed)

Commit **`7e3752e`** — `hermes: implement CompileFunction + error-surface shims
so deno_web init runs (E3)`:

- `src/hermes/modules.rs` — real `v8__ScriptCompiler__CompileFunction`
  (wrap-then-lower, eval to Function via `v8x_hermes_run`).
- `src/hermes/hermes_shim.cpp` — pin refcount (`pin_refs` +
  `v8x_hermes_pin_addref`, refcounted `unpin`); `capture_message` for non-JSError
  exceptions; `v8x_hermes_object_get_prototype`; `v8x_hermes_run` catches
  `jsi::JSIException`/`std::exception`.
- `src/hermes/core.rs` — `global_new` addrefs a re-wrapped pin;
  `v8__Exception__CreateMessage`, `v8__String__Empty`, `v8__Object__GetPrototype`;
  `v8x_hermes_pin_addref` / `v8x_hermes_object_get_prototype` extern decls.
- `src/hermes/shims.rs` — removed the six null stubs now implemented.
- `src/hermes/lower.rs` — `wraps_before_lowering_preserves_top_level_return`
  regression test for the wrap-before-lower order.

Backend suite: **35 passed, 0 failed** (`hermes,link_hermes`). No regressions.

Sandbox (deno checkout `v8x-rebase-rc`, NOT pushed): new crate
`libs/hermes_web_probe` (bin `hermes_web`), added to workspace members.

## (5) Recommended E4 target: the `v8::Private` subsystem

The first wall in actual ext/web API USE (not init) is `v8::Private`:
`error.rs` uses `Private::for_api` + `Object::get_private`/`set_private` for
error callsite metadata AND the error formatter needs it, so any thrown error
inside ext/web currently cannot be reported (it re-panics at `private.rs:58`,
`v8__Private__ForApi` null stub; `v8__Object__Get/SetPrivate` also stubs). E4:
implement Private (map `for_api(name)` to a stable registered Symbol, back
get/set_private with Symbol-keyed properties) so exceptions surface cleanly,
then chase the first op-marshalling wall — already sighted:
`new TextEncoder().encode(...)` throws `expected ArrayBuffer or ArrayBufferView`
from `op_encoding_encode_into`, a TypedArray/ArrayBuffer ABI detail in the
backend or op glue.

## (6) Disk at end

`df -h /`: 9.0Gi avail (66% used). One probe rebuild per iteration with
`CARGO_INCREMENTAL=0`; no ENOSPC. `target/debug/incremental` never created.
