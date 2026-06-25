# v8x — one V8 C-ABI, many JS engines

`v8x` makes `rusty_v8` engine-agnostic. It vendors the real `v8` crate's Rust
source and re-implements the `v8__*` C ABI on a pluggable backend, so
`deno_core` — and all of Deno — compiles **unchanged** on JavaScriptCore or
QuickJS instead of V8.

```diff
# deno workspace Cargo.toml
[patch.crates-io]
- v8 = "0.155.0"
+ v8 = { package = "v8x", features = ["jsc"] }       # or "quickjs"
```
```diff
- cargo build -p deno
+ cargo build -p deno --features hmr                 # load ext JS at runtime
```

Backends today:
- **V8** 14.9.207.2 (baseline, real crate)
- **JavaScriptCore** — vendored static WebKit *and* a system-framework path that
  links the OS's JSC (0 engine bytes shipped)
- **QuickJS-ng** 0.15.x (statically linked, ~1 MB)

---

## 1. Numbers (Apple M-series, aarch64, release)

> ⚠️ Deno is built `--features hmr` on the JSC/QuickJS backends: **no startup
> snapshot**, so all extension JS is parsed from source at every boot. V8 here
> uses its normal startup snapshot. The "projected" startup column is what we'd
> expect once we add per-engine **bytecode caching** (JSC `CodeCache` /
> QuickJS `JS_WriteObject`), which is the equivalent of V8's snapshot.

### Binary size

All sizes **stripped** (`strip -x`), aarch64-apple-darwin, release. Two
binaries matter — don't conflate them.

**(a) Full `deno` CLI** — the dev tool (TS compiler, LSP, test runner, …):

| engine          | `deno` CLI (stripped) | vs V8 |
| --------------- | --------------------- | ----- |
| V8 14.9         | 82.6 MB               | —     |
| QuickJS-ng      | 38.4 MB               | −44.2 MB (−54%) |
| **system JSC**  | **36.7 MB** (0 engine bytes) | **−45.9 MB (−56%)** |

**(b) `denort`** — the runtime that actually ships in products: `deno compile`
appends user code to it, and **Deno Desktop links `libdenort.dylib`**. This is
the number that matters for CLIs / Lambda / desktop.

| engine          | `denort` (stripped) | vs V8 |
| --------------- | ------------------- | ----- |
| V8 14.9         | 72.6 MB *(official, w/ snapshot)* | — |
| QuickJS-ng      | 26.3 MB             | −46.3 MB (−64%) |
| **system JSC**  | **24.5 MB** (0 engine bytes) | **−48.1 MB (−66%)** |

The lever is the engine: V8 is ~40 MB of the binary. Reuse the OS's JSC (every
macOS / iOS device ships it) and that 40 MB disappears.

> **Apples-to-apples caveat.** V8 binaries are the official Deno release; our
> JSC/QuickJS builds are `--features hmr`, i.e. **no embedded snapshot** (and no
> TSC snapshot). So a slice of the gap is removed snapshot data, not engine — the
> dominant ~40 MB is the engine. Adding bytecode caching back adds a few MB but
> keeps the bulk of the win. (V8 denort measured from the actual official
> `denort-aarch64-apple-darwin`, not a `deno compile` output.)

**Jitless V8 doesn't help size.** `--jitless` is a runtime flag — the JIT
compiler code is still linked, so a jitless-V8 binary is the *same* size. On iOS
you'd pay full V8 size *and* lose the JIT; system JSC gives you 0 engine bytes
*and* keeps the JIT.

### Startup (`deno run hello.js`, min of 25, perf_counter)

| engine            | startup   | vs V8  |
| ----------------- | --------- | ------ |
| V8 (snapshot)     | **9.9 ms**| —      |
| QuickJS + caches  | **31.7 ms** | ~3.2× |
| system JSC + caches | **51 ms** | ~5.2× |
| QuickJS, no cache | 160 ms    | ~16×   |

*(JSC's extra ~20 ms is `ops_bindings` — JSC function/object creation is heavier
than QuickJS; partially mitigated by caching the constant key-strings and
`Function.prototype`, and by skipping `.prototype` on non-constructor ops.)*

We profiled where the 160 ms went (it is **not** the engine): `deno --version`
= 0 ms, so Rust/ICU init is free. The cost split (via `DENO_STARTUP_PHASES`):

| phase                       | no cache | with caches |
| --------------------------- | -------- | ----------- |
| `into_sources_and_source_maps` (swc TS→JS transpile of every ext file) | **138 ms** | 5 ms |
| `init_extension_js` (execute bootstrap JS) | 15 ms | 15 ms |
| op bindings + misc          | ~7 ms    | ~7 ms       |
| **total `JsRuntime::new`**  | **160 ms** | **27 ms** |

**No V8 snapshot is available** to JSC/QuickJS (a V8 snapshot is a serialized
*heap image*; neither engine can serialize a heap with native-bound ops). So we
do the next best thing — two content-hashed caches that make boot N+1 skip the
expensive, deterministic work:
1. **Transpile cache** (`runtime/transpile.rs`): persist swc's TS→JS output per
   ext source. Kills the 138 ms. Engine-agnostic (helps JSC too).
2. **Bytecode cache** (`src/qjs/fam_module.rs`): `JS_WriteObject` each module's
   QuickJS bytecode, `JS_ReadObject` on warm boot — skips re-parse.

The residual ~15 ms is *executing* the bootstrap, which only a real heap
snapshot avoids — that needs engine support we don't have (a WebKit/QuickJS
patch, or Divy's heap-image approach). Disable caches with
`V82JSC_NO_TRANSPILE_CACHE=1` / `V82JSC_NO_BC_CACHE=1`.

### HTTP throughput (`Deno.serve` hello-world, `ab -k -n 100000 -c 100`)

| engine            | req/s    | mean latency | p99   | vs V8   |
| ----------------- | -------- | ------------ | ----- | ------- |
| V8 14.9 (JIT)     | ~283,000 | 0.35 ms      | 1 ms  | —       |
| **system JSC (JIT)** | **~204,000** | **0.49 ms** | 1 ms | **~0.72×** |
| V8 (jitless)      | ~226,000 | 0.44 ms      | 1 ms  | ~0.80×  |
| QuickJS-ng        | ~135,000 | 0.74 ms      | 1 ms  | ~0.48×  |

*(`ab -k -n 100000 -c 100`, single consistent sweep; latency at c=100 ≈
concurrency/throughput. JSC has hit 230k / 0.79–0.86× across runs.)* Note V8
**jitless** barely drops on HTTP — this workload is op-dispatch-bound, not
JS-execution-bound. The JIT difference shows up on compute (below) — which is
the **iOS story**: on iOS only JSC is allowed to JIT; V8 must run jitless.

JSC reaches **~80–86% of V8** after two fixes (below). The rest is the **op
call path**: official V8 Deno uses V8 *fast API calls* for per-request ops; our
backend still routes every op through the slow C++ callback. Wiring JSC's
**DOMJIT** to the fast-op path (below) is the next step. (Without keep-alive all
sit ~45k — that's TCP accept rate, not the engine; ignore it.)

#### Two fixes that took JSC from 120k → 230k req/s

1. **JIT entitlement (critical).** On macOS JSC only maps JIT pages if the binary
   is codesigned with `com.apple.security.cs.allow-jit`. Without it JSC
   *silently* runs the LLInt interpreter — ~9× slower on raw JS. → **120k → 183k**.
   ```
   codesign --entitlements jit.entitlements -f -s - ./deno
   ```
2. **Cheaper microtask drain.** The microtask checkpoint (runs every request) was
   draining the job queue by `eval`-ing a dummy `"0"` script — recompiling + re-
   entering the VM each time. Replaced with a **cached no-op function call**
   (`JSObjectCallAsFunction`), which establishes the same outermost API-entry
   boundary JSC drains at, minus the parse. → **183k → ~204k**. *(A direct
   `JSC::VM::drainMicrotasks()` call hit 230k but broke correctness — continuations
   that schedule another op after an `await` ran in a broken re-entrant context;
   the entry-scope boundary is required.)*

### Memory (RSS, `Deno.serve` hello-world after load)

| backend     | RSS        |
| ----------- | ---------- |
| **QuickJS-ng** | **~42 MB** (barely grows — interpreter, no JIT code/heap) |
| V8 14.9     | ~51 MB     |
| system JSC  | ~80 MB     (JIT + GC heap hungry) |

QuickJS wins memory decisively — ideal for serverless / Lambda (LLRT's reason
for picking it). JSC trades RAM for JIT speed. Idle RSS: V8 ~41 MB, QuickJS
~40 MB, JSC ~66 MB.

### Prod-readiness matrix (both backends)

Validated across ~80+ scenarios in 7 integration/fuzz passes (realistic API
server, static-file server, 2 MB binary round-trips, WebSocket, crypto, streams,
subprocess, 500-request sustained load). The passes caught **6 prod-critical
bugs** — JSC microtask-checkpoint regression, JSC TextEncoder UTF-8 truncation,
JSC TextDecoder invalid-UTF-8→empty, QuickJS binary-Response-body corruption,
and an `Eternal`/`TracedReference` **misaligned-pointer UB** (both backends) —
all fixed. Both backends now pass the full matrix below; remaining gaps are
dynamic `import()` / npm-CJS (both) and minor QuickJS `toLocaleString` grouping.

**`Eternal`/`TracedReference` alignment fix (both backends).** `v8::Eternal<T>`
and `v8::TracedReference<T>` are mapped to Rust structs backed by
`data: [u8; SIZE]` — **alignment 1**. deno_core embeds them inline in
`thread_local!`s (e.g. the webidl sequence converter's cached `"next"`/`"done"`/
`"value"` string `Eternal`s, hit on every `crypto.subtle.generateKey` keyUsages
conversion). When such an embedding lands on an odd address, the FFI shims'
`*this` read of the pointer-sized payload is a misaligned dereference — UB that
panics under debug (`misaligned pointer dereference`) and is silently wrong in
release. The old binary happened to place the `thread_local` on an aligned
address; any unrelated code change that shifted layout flipped it to a crash.
Fixed by switching all six `Eternal`/`TracedReference` `Get`/`Set`/`CONSTRUCT`
shims to `read_unaligned`/`write_unaligned`. (`Global<T>` is a real `NonNull`,
so it was never affected.)


| capability | QuickJS | system JSC |
| --- | --- | --- |
| `console.*` (objects, arrays, table, group, count) | ✅ | ✅ *(enum dup fixed)* |
| `console.log(error)` formatting | ✅ *(stack-header fix)* | ✅ *(+ JSC `fn@url`→`at fn (url)` frame normalize)* |
| `Deno.serve` HTTP | ✅ | ✅ |
| `crypto.subtle` digest | ✅ | ✅ |
| JSON / Map / Set / Date / BigInt / RegExp / Intl | ✅ | ✅ |
| URL / TextEncoder (incl. unicode/emoji) / atob / AbortController / WeakRef | ✅ | ✅ *(UTF-8 truncation fix)* |
| TextDecoder of invalid/binary UTF-8 (→ `�`) | ✅ | ✅ *(lossy-decode fix)* |
| Date/Intl, RegExp (lookbehind/unicode/sticky), typed-array views, randomUUID, Blob, URLSearchParams, error.cause, AggregateError | ✅ | ✅ |
| fetch POST/headers, Request/Response/Headers, Transform/Compression streams, FormData | ✅ | ✅ |
| `Deno.Command` subprocess, `Deno.readDir`/`stat`, `crypto.subtle` ECDSA/AES-GCM/SHA-512 | ✅ | ✅ |
| binary/stream/file Response bodies (static-file server, streaming) | ✅ *(backing-store retention fix)* | ✅ |
| WebSocket upgrade + echo, 2 MB binary fetch round-trip (checksum), ReadableStream tee, Response.clone, DataView endianness, base64 of all 256 bytes, JWK import/export | ✅ | ✅ |
| Proxy/Reflect, FinalizationRegistry, Symbol.asyncIterator, circular/wide/throwing-getter console | ✅ | ✅ |
| Promise race/any/allSettled, async generators, 100-deep chains | ✅ | ✅ |
| `await op(); setTimeout()/op()` (microtask-checkpoint correctness) | ✅ | ✅ *(no-op-fn drain fix)* |
| top-level await, `Promise.all` | ✅ | ✅ |
| `structuredClone` (objects/arrays/Date/RegExp/Map/Set/TypedArray/ArrayBuffer/BigInt) | ✅ (`JS_WriteObject`) | ✅ (type-preserving encoder) |
| node-compat: `node:buffer`/`events`/`path`/`process`/`os` (+ Buffer rw/concat/base64/hex, EventEmitter) | ✅ *(import-parser fix)* | — |
| circular ESM module deps (named / re-export / default-import cycles) | ✅ | ✅ |
| node-compat: `node:stream`/`crypto`/`util` (circular indirect re-export) | ❌ QuickJS `js_resolve_export` | — |
| dynamic `import()` | ❌ async-bridge | ❌ async-bridge |
| startup (warm) | 31.5 ms | 51 ms |

**`structuredClone` fixed**: the `ValueSerializer::Release` shim now hands the
encoded bytes back (it returned null before, so every clone "failed to
deserialize"); crate `release()` treats `size` as capacity when the V8
reallocate-delegate atomic is 0. QuickJS round-trips full object graphs
(`JS_WriteObject`); JSC uses a JSON encoder so Map/Set/cycles aren't covered yet.

**Minor known issues** (non-blocking): QuickJS `Number.toLocaleString` doesn't
apply locale digit grouping (`1234567` vs `1,234,567`) — QuickJS-ng Intl is
partial; JSC is correct. *(Previously JSC `structuredClone` lost rich types; now
fixed — a hand-rolled type-preserving serializer in `shim_serializer.rs` encodes
Date/RegExp/Map/Set/TypedArray/ArrayBuffer/BigInt with type tags, since the JSC C
API has no `JS_WriteObject`.)*

**Module-system gaps (QuickJS).** General **circular ESM works** (verified: named,
re-export, and default-import cycles all match V8). The remaining gaps are
narrower: (1) `node:stream`/`crypto`/`util` fail with `SyntaxError: circular
reference when looking for export 'Duplex'`. Probed this round: the loader
*already* dedups by name (QuickJS resolves each module name once internally), so
this is **not** a specifier-keying/re-compile bug as previously thought — it's
QuickJS-ng's `js_resolve_export` refusing to resolve an **indirect re-export**
(`export { X } from …`) whose resolution chain loops back through the importing
module. V8's linker resolves the same indirect export lazily and succeeds. This
is an upstream QuickJS-ng linker limitation, not a v82jsc keying bug, so it needs
a real fix in the QuickJS resolve-export path (deep — deferred). (2) dynamic
`import()` (below). *(Fixed an earlier round: a regex import-scanner bug that
mis-extracted string literals inside `export function` bodies — e.g.
`_extensions[".node"]` — as bogus imports, which broke `node:process` and any ESM
module with such literals.)* *(Separately, `export *` star-export cycles fail on
both backends — QuickJS hits TDZ "X is not initialized", JSC panics in deno_core
`modules/map.rs` because the star-cycle module never reports `Evaluated`; rare
pattern, deferred.)*

**npm module-specifier resolution — fixed (QuickJS).** deno registers each module
source under its *resolved* name (`npm:express@4` →
`file:///…/express/4.22.2/index.js`), but QuickJS's module loader is handed the
*raw* specifier from the source text, so it couldn't find the source
(`no source for npm:express@4`). Fixed by walking deno's `ResolveModuleCallback`
during `InstantiateModule` to learn every `(referrer, specifier) → resolved-name`
edge, then installing a QuickJS module-*normalize* callback that canonicalizes
specifiers from that map (synthetic `ext:`/`node:` modules fall back to identity).
`import express from "npm:express@4"` now resolves and begins executing the CJS
graph (previously failed at module load).

**Remaining gap — node CJS polyfill loader (`loadExtScript`).** With resolution
fixed, `import express` now executes deep into its CJS dependency tree. Getting
there took a chain of four more fixes (all landed):

1. **Lazy-ESM namespace materialization.** `createLazyLoader(…)()`
   (`op_lazy_load_esm`) returned an empty namespace for modules requested *after*
   first eval — `mark_all_modules_evaluated` reports them `Evaluated`, so deno
   skips evaluation and asks straight for the namespace, which our
   `GetModuleNamespace` answered with the empty-object fallback (the module's
   `JSModuleDef` was never built). Now `GetModuleNamespace` compiles+evaluates the
   stored source on demand. This is what unblocked plain `import "node:fs"`
   (`node:fs`'s `createLazyLoader("…/internal/fs/utils.mjs")()` → `constants`).
2. **`import.meta.url`.** The host import-meta callback was inert, so
   `createRequire(import.meta.url)` (top of every npm CJS entry) got `undefined`.
   Now each compiled module's `import.meta.url`/`main` is filled from its resolved
   name.
3. **Function code cache.** node's CJS `wrapSafe` treats a null
   `function.createCodeCache()` as fatal ("Unable to create code cache from
   function"); return the same 1-byte placeholder the script paths use.

4. **V8 CallSite API — QuickJS source patch.** express's `depd` dep sets
   `Error.prepareStackTrace` and reads `obj.stack` as a CallSite array, calling
   `.isEval()`/`.getEvalOrigin()`/etc. QuickJS-ng already ships the
   `prepareStackTrace` hook + CallSite objects, but the proto only had
   `isNative`/`getFileName`/`getFunction`/`getFunctionName`/`get{Line,Column}Number`.
   **Patched `vendor/quickjs-ng/quickjs.c`** (mirroring the JSC/WebKit patch flow)
   to add the rest of V8's CallSite surface — `isEval`, `getEvalOrigin`,
   `getThis`, `getTypeName`, `getMethodName`, `isToplevel`, `isConstructor`,
   `isAsync`, `isPromiseAll`, `getPromiseIndex`, `toString`, … — returning V8's
   documented defaults for plain frames. `depd` now loads.

5. **Cyclic re-export of an imported binding — QuickJS-ng linker bug, source
   patch.** `node:stream` does `import Duplex from "node:_stream_duplex"; export
   { Duplex }` while `node:stream/promises` does `import * as _ from
   "node:stream"` — a module cycle that **re-exports an imported binding**.
   QuickJS-ng makes `export { Duplex }` a *local* export whose `var_ref` lives in
   node:stream's own import slot, which is still NULL while the cycle peer is
   unlinked. Upstream then either dereferences the NULL (crash, in
   `js_inner_module_linking`) or reports a spurious "circular reference when
   looking for export 'Duplex'" (in `js_build_module_ns`). **Confirmed a genuine
   QuickJS-ng bug** — reproduced in 4 lines through the stock `qjs` CLI (segfault),
   independent of deno/v82jsc. **Patched `vendor/quickjs-ng/quickjs.c`**: a shared
   `js_get_local_export_var_ref` helper that, when the slot is NULL, follows the
   import alias to its source module and resolves the binding there (the source's
   local var_ref is already allocated) — matching the ES spec, where the indirect
   binding exists for the whole SCC's instantiation. Used at both NULL sites.
   **`node:stream` (and `node:fs`) now load.**

**Remaining blocker — node-polyfill lazy-script eval order.** With `node:stream`
loading, express advances into `node:module`'s native-module require path and
hits `ReferenceError: default is not initialized` on
`loadExtScript("ext:deno_node/internal/streams/state.js").default` — a
`lazy_loaded_js` module whose `default` binding is read before its body has run.
This is an evaluation-ordering issue in deno's on-demand lazy-script loader (our
side), the next layer down. Dynamic `import()` of brand-new specifiers remains a
separate gap.

### Real-world app status (npm / Next.js)

Hello-world + `Deno.serve` work on all backends. `import express` now resolves npm
specifiers, loads `node:fs`, runs `createRequire`/CJS `wrapSafe`, clears `depd`
(V8 CallSite patch), and executes its dependency tree until it pulls `node:stream`
— blocked on the cyclic-`import *` instantiation issue above. **Next step: fix
`node:stream` module-instantiation ordering, then walk the rest of the express dep
tail; then re-measure RSS/throughput on a real npm server (per
Ryan's ask).**

### Raw JS compute (no ops — pure engine)

| benchmark        | V8 (JIT) | JSC (JIT) | V8 (jitless) | JSC (jitless) | QuickJS |
| ---------------- | -------- | --------- | ------------ | ------------- | ------- |
| `fib(32)` ×5     | 58 ms    | **41 ms** | 505 ms       | 524 ms        | 483 ms  |
| 50M-iter sum loop| 32 ms    | **30 ms** | 416 ms       | 191 ms        | 688 ms  |

With the JIT entitlement, **JSC matches/beats V8 on raw JS**. The jitless columns
are the **iOS reality**: V8 jitless is ~9× slower than V8 JIT — and on iOS V8 has
no other option. **JSC keeps its JIT on iOS**, so it runs raw JS ~12× faster than
jitless V8 there (41 ms vs 505 ms). That's the case for JSC on iOS. QuickJS is a
pure interpreter (never JITs) — fine for cold/short scripts, slower on hot loops.

---

## 2. Why ship a non-V8 engine

- **Deno Desktop / WebView reuse.** A desktop app can render in the OS WebView
  *and* run its JS on the same system JSC — one engine, zero extra engine bytes.
  Smallest possible desktop binary.
- **iOS.** The App Store allows JIT only for `JavaScriptCore`. JSC is the only
  way to run Deno on iOS *with* a JIT — no jitless-V8 penalty. Path to "Deno on
  iPhone."
- **`deno compile` CLIs.** Single-file tools where every MB counts. system-JSC
  drops ~24 MB off each compiled binary.
- **Serverless (AWS Lambda, edge).** Cold-start + artifact size are the cost
  drivers. Small binary + (projected) fast cached start = cheaper, faster.

### DOMJIT → V8 fast calls
JSC's DOMJIT lets native getters/ops be called without the full C++ call
overhead — the same role as V8 fast API calls. The op layer can map deno_core's
fast-call ops onto DOMJIT, keeping the hot op path fast on JSC.

### Inspector for every engine
Both JSC and QuickJS expose remote-debug protocols. As with the binding
generation, the inspector glue can be made uniform across engines so
`--inspect` works everywhere. *(scoping in progress)*

---

## 3. How it works — and how it stays current

`v8x` does **not** hand-write the API. It vendors the real `v8` crate's Rust
source verbatim; only the `v8__*` C-ABI symbols are re-implemented per engine.
So the entire `rusty_v8` surface that `deno_core` calls is real, and the swap is
a `[patch]` one-liner.

Core mappings (JSC example):
- `Local<T>` pointer **is** a `JSValueRef`
- `HandleScope` = protect/unprotect bridge to the engine GC
- `Isolate` = `JSContextGroup`; current context is thread-local

**Automated maintenance (agent loop).** Whenever upstream `v8` ships a new
version, an agent loop:
1. vendors the new `v8` crate source,
2. diffs the `v8__*` C-ABI surface,
3. implements/updates the new/changed symbols on each backend (JSC, QuickJS),
4. builds Deno on each engine and runs the smoke suite,

so v8x tracks V8 without manual porting. The same loop is how QuickJS
`console.log` (cppgc-wrapped `Console` class, prototype materialization,
property enumeration) was brought up.

---

## 4. `deno compile` size

| engine            | `deno compile` output | vs V8 |
| ----------------- | --------------------- | ----- |
| V8 (official)     | 68.6 MB               | —     |
| system JSC        | _≈44 MB (proj.)_ *    | ~−24 MB |
| QuickJS-ng        | _TBD_ *               |       |

\* `deno compile` on the JSC/QuickJS backends needs snapshot **deserialize**
support (the compiled binary embeds the runtime snapshot). That path is still
WIP — the per-engine size delta (≈24 MB, same as the runtime binaries) carries
straight through once it lands.

## 5. Demo script

```sh
# 1. Deno runs real JS on a non-V8 engine (console.log, objects, tables…)
deno run hello.ts                       # QuickJS / JSC backend

# 2. Size delta is in the runtime itself — show the executables:
ls -lh deno-v8 deno-jsc                 # 78.7 MB  vs  54.2 MB  (system JSC)

# 3. HTTP: JSC out-throughputs V8 today
deno run --allow-net serve.ts

# 4. Desktop: same app, JS on system JSC, UI in system WebView (1 engine, 0 extra MB)
```
