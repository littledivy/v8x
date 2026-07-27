# E6: A real post-bootstrap heap snapshot on QuickJS

## Question

Prior spike research concluded "AOT != heap snapshot": ahead-of-time compilation
captures code, not the initialized heap. This experiment tests whether that
limitation can be beaten on QuickJS by producing a real heap snapshot: serialize
the post-bootstrap global object graph into a blob and restore it into a fresh
runtime, sidestepping the replay-tape approach.

## Verdict (short)

Yes. QuickJS can produce a real heap snapshot today, and the v8x QuickJS backend
already contains the machinery to do it. `JS_WriteObject` with
`JS_WRITE_OBJ_REFERENCE` serializes the entire reachable object graph rooted at
the global, and `JS_ReadObject` restores it into a fresh runtime with structure
and identity intact. The one thing `JS_WriteObject` cannot serialize by value is
a native C function pointer (`JSCFunction`), and the vendored quickjs-ng has been
patched by v8x to handle exactly that with a reference-path scheme, not by value.

The prototype restores nested objects, prototype chains, frozen objects, `Map`,
a typed array, a `BigInt`, a cyclic reference, `Symbol.for` identity, and a
native function that is both callable after restore and carries a property added
to it on the heap after install. All 16 assertions pass. See
`e6-src/e6_snap.c`.

## What `JS_WriteObject` / `JS_ReadObject` can and cannot serialize

Header flags (`vendor/quickjs-ng/quickjs.h`):

| flag | effect |
|---|---|
| `JS_WRITE_OBJ_BYTECODE` | allow function bytecode and modules in the graph |
| `JS_WRITE_OBJ_REFERENCE` | encode arbitrary object graphs by reference (cycles, shared nodes) |
| `JS_WRITE_OBJ_SAB` | allow SharedArrayBuffer |
| `JS_WRITE_OBJ_STRIP_SOURCE` / `STRIP_DEBUG` | drop source / debug info |

With `REFERENCE` set, the writer walks the graph and emits a reference table, so
shared and cyclic nodes are preserved as identity rather than duplicated. The
serializable value and object types are: primitives (int, float, bool,
null/undefined), strings and ropes, symbols, BigInt, plain objects, arrays,
typed arrays and ArrayBuffers, `Map`/`Set`, `Date`, `RegExp`, frozen/sealed
objects (the extensible bit and per-property flags are preserved), function
bytecode, and modules.

The hard blocker is the native C function binding. In stock quickjs-ng a
`JS_CLASS_C_FUNCTION` object holds a raw `JSCFunction *` pointer plus a magic
int and cproto; a raw code pointer is meaningless across process boundaries and
cannot be serialized by value. Host objects with opaque `JS_SetOpaque` data are
the same class of problem: the C-side bytes are outside the JS heap and unknown
to the serializer. Stock quickjs simply refuses these.

## How v8x already solves the native-function blocker

The vendored `vendor/quickjs-ng/quickjs.c` carries v8x patches (notably
`patches/quickjs-67-snapshot-native-function-state.patch`, plus `-27`, `-68`,
`-100`) that add a reference-path scheme for native functions instead of
serializing them by value:

1. Before writing, the embedder installs an intrinsic registry on the global
   under `__v8x_snapshot_intrinsics` and calls
   `v82jsc_snapshot_capture_intrinsics` (quickjs.c around line 40795). This walks
   every reachable native function and records it in the registry array. In the
   Rust backend this is `refresh_snapshot_intrinsics` in `src/quickjs/core.rs`.
2. When the writer hits a `JS_CLASS_C_FUNCTION` node, `JS_WriteCFunctionObject`
   (quickjs.c line 41267) searches the registry for a property path that reaches
   that exact function pointer (matching pointer, and optionally length/cproto/
   magic) via `JS_FindIntrinsicFunctionPath`. It emits the path
   (`BC_TAG_C_FUNCTION_OBJECT` plus a sequence of value/getter/setter/prototype
   steps and atoms), not the pointer. An unregistered native function is a hard
   error ("unregistered native function").
3. On read, `JS_ReadCFunctionObject` (line 43075) walks the same path in the
   fresh runtime's registry and rebinds to whatever function pointer lives there
   now. The pointer is re-resolved in the new process; only the path is stored.
4. Patch 67 adds the piece that makes this a real heap snapshot rather than a
   re-binding: for an exactly-matched native function, the writer also emits
   `JS_WriteSnapshotObjectState` (line 41579), which serializes the function
   object's own properties, its extensible bit, and its internal prototype. So a
   property added to a native function on the heap after install (the prototype's
   `callCount = 42` in the test) survives the round trip. `JS_ReadSnapshotObjectState`
   reapplies it on top of the freshly rebound function.

This is the key architectural point. The snapshot is a hybrid: pure-JS heap
state travels by value through `JS_WriteObject`/`JS_ReadObject`, native bindings
travel as symbolic paths and are re-resolved against a freshly installed binding
table, and per-object mutations layered on top of native objects travel as
serialized object state. Opaque C-side data (`JS_SetOpaque`) still does not
travel; a host object must provide a `v82jsc_snapshot_write_host_object` /
`read_host_object` hook (quickjs.c line 40672) to encode its own bytes, or it is
skipped.

## The hybrid, stated precisely, and whether it works

The "restore pure-JS heap state via `JS_ReadObject` plus re-install native
bindings" hybrid is viable and is what the prototype does:

1. Fresh runtime.
2. Re-run only the native-binding install step (the same `JS_NewCFunction`
   registrations the original runtime used). This repopulates the pointers.
3. Rebuild the intrinsic registry (`refresh_snapshot_intrinsics`) so path
   resolution has the same reachable set.
4. `JS_ReadObject` the pure-JS heap blob on top. Native nodes resolve to the
   freshly installed pointers by path; everything else is restored by value.

In the prototype, forgetting step 2 fails deterministically with
`intrinsic function path property missing at step 1`: the writer recorded a path
to a native function that the fresh registry did not contain. Re-installing the
native bindings first makes all 16 checks pass.

Where it breaks:

- Opaque native state behind `JS_SetOpaque` (sockets, file handles, timers, a
  napi addon's C++ object) is not in the JS heap and does not serialize. It needs
  a per-class host-object hook, or the object must be reconstructed by the
  re-run bindings and only its JS-visible properties restored.
- The native binding set at restore time must match what was reachable at
  capture time. A path recorded against `intrinsics[5]` must still reach the same
  logical function after re-install. v8x mitigates this with metadata matching
  (length/cproto/magic) and a relaxed second pass, but a reordered or renamed
  builtin table would mis-resolve.
- Modules and their evaluated exports need the surrounding tape/registry
  scaffolding that `src/quickjs/snapshot.rs` already carries
  (`__v8x_snapshot_module_exports`); a bare `JS_WriteObject` of the global does
  not capture module identity on its own.

## Measured numbers

Standalone C program linked against a fresh build of the vendored quickjs core
(`e6-src/`, arm64, clang -O1). Times are single-run wall clock, indicative only.

Small bootstrap (the mixed-type graph with the mutated native function):

| path | blob size | time |
|---|---|---|
| snapshot write | 17876 B | 10.2 ms |
| snapshot restore | 17876 B | 0.08 ms |
| bytecode blob read + eval | 1318 B | 0.01 ms |
| from-source eval | n/a | 0.04 ms |

Scaling probe (20000 computed objects plus a 20000-entry `Map`,
`e6-src/e6_scale.c`):

| path | blob size | time |
|---|---|---|
| from-source bootstrap eval | n/a | 8.4 ms |
| snapshot write | 1.46 MB | 25.2 ms |
| snapshot restore | 1.46 MB | 10.0 ms |

## The honest finding on speed

The real heap snapshot is correct but it is not a speed win on QuickJS the way a
V8 startup snapshot is. At 20000 objects, restoring the blob (10 ms) is slightly
slower than re-running the bootstrap from source (8.4 ms), and the blob is 1.46
MB against a few hundred bytes of source. The reason is structural: V8's snapshot
is a serialized image of the heap arena that is largely mmap-and-fixup at
startup, so restore is near-linear in bytes touched and skips all the allocation
and shape-transition work. QuickJS's `JS_ReadObject` is a decoder that re-allocates
every object, re-hashes every property into shapes, and rebuilds `Map`/typed-array
internals. That is the same order of work as executing the constructing code, so
it does not beat re-execution for a pure-data bootstrap, and it produces a much
larger artifact than the source or bytecode.

Where the real heap snapshot is worth it on QuickJS is precisely the case the
prior research called out: state that is expensive to recompute or that has side
effects, so that re-execution is not an option. Restoring a graph that took
network I/O, heavy parsing, or non-deterministic construction to build is a real
win because the alternative is not "re-run cheaply," it is "cannot re-run." For
Deno bootstrap the payoff is avoiding the replay-tape machinery and its ordering
hazards, not raw milliseconds.

## By analogy to Hermes HBC plus native builtins

The same hybrid shape should carry to Hermes. Hermes Bytecode (HBC) is the code
half, exactly the AOT artifact the prior research described. A real Hermes heap
snapshot would need the two extra halves this experiment exercised: a by-value
serializer for the pure-JS object graph, and a symbolic rebinding scheme for
`HostFunction` / native builtins so restore re-resolves them against a freshly
installed native table. Hermes does not ship a `JS_WriteObject` equivalent for
arbitrary heap graphs, so this is more build-your-own than on quickjs-ng, but the
architecture is the same: heap by value, natives by path, native-object mutations
as serialized object state, opaque C state via per-type hooks or reconstruction.

## Recommended next experiment (E7)

Drive the existing `src/quickjs/snapshot.rs` `capture_context` /`replay_context`
path (not a standalone program) across a Deno bootstrap and measure whether the
by-value global snapshot can replace the replay-tape for the
`ext:core/ops` synthetic-module gap noted in the Deno-boot memory. Concretely:
capture the post-bootstrap global with `JS_WriteObject(REFERENCE)`, restore into
a fresh isolate with the native op bindings re-installed first, and check whether
`globalThis.bootstrap` and the op namespace resolve without replaying the tape.
That isolates the one place the tape is still load-bearing (synthetic
`ext:core/ops` module identity) from the parts the real heap snapshot already
covers.

## Reproduce

```
cd vendor/quickjs-ng
clang -O1 -c quickjs.c libregexp.c libunicode.c dtoa.c -DCONFIG_VERSION='"e6"'
clang -O1 -I. \
  ../../docs/hermes-spike/experiments/e6-src/e6_snap.c \
  ../../docs/hermes-spike/experiments/e6-src/e6_stubs.c \
  quickjs.o libregexp.o libunicode.o dtoa.o -o e6_snap
./e6_snap
```

`e6_stubs.c` supplies no-op definitions for the v8x extern hooks that the
vendored quickjs.c references but this experiment does not exercise (coverage,
debugger, locale, host-object). The snapshot path itself is unmodified vendored
code.
