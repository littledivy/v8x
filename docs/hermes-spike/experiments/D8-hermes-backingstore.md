# D8: external-memory BackingStore -> deno_core boots + runs 1+1 on Hermes

## Result (the milestone)

deno_core::JsRuntime::new SUCCEEDS on the Hermes backend and execute_script("1 + 1")
runs. Verified by rebuilding the deno checkout's hermes_boot example against the
committed BackingStore work and running it:

    BUILD_EXIT=0
    BOOT OK: JsRuntime::new succeeded
    1+1 executed OK (value handle returned)
    RUN_EXIT=0

(Run: cargo build -p deno_core --example hermes_boot in ~/gh/deno-v8x-rebase, then
DYLD_FRAMEWORK_PATH=/Users/divy/gh/v82jsc/vendor/hermes target/debug/examples/hermes_boot.)

## What landed

The external-memory BackingStore subsystem (commit 798c4fe): v8__ArrayBuffer__
NewBackingStore__with_data + the empty/with_backing_store/accessor chain, modeled on
JSI's MutableBuffer + createArrayBuffer, with the v8 deleter driving external-memory
free at teardown (C2-owned). This was the last step of JsRuntime::new_inner
(store_js_callbacks). The isolated 1+1 boot probe already passed; BackingStore closed
the full deno_core boot.

## Process note

The D8 agent implemented + committed the BackingStore (798c4fe) then crashed on a
transient API 529 before reporting or writing this doc. The boot was re-run manually
(deterministic, no agent) to confirm the result. Its uncommitted baseline --update was
discarded and re-derived separately.

## Honest scope (ties to D7)

This is deno_core (the engine-embedding runtime core) booting + running code. A COMPLETE
Deno runtime is still blocked by the D7 ceiling: Deno's ext/ layer uses real async
generators (async function* / for await over sockets / Blob.stream) that Hermes's compiler
does not support and that are not source-transformable. deno_core-on-Hermes is the milestone;
full-Deno-on-Hermes needs upstream Hermes async generators or a large vendored-source rewrite.
