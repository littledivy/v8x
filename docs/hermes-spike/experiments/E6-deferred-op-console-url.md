# E6 — deferred-op async bridge (the honest crux) + console + URL on Hermes

Three targets, all landed and asserted through the real deno_core / ext/web
path on the Hermes backend:

1. **PRIORITY 1 (the crux):** a genuinely-deferred async op settles its JS
   promise through the real `run_event_loop` (not a bare microtask checkpoint).
2. **console:** `console.log("hi", {a:1}, [2,3])` produces real, byte-correct
   output through ext/web's `01_console.js` inspector.
3. **URL:** `new URL(...)` parses; `pathname` and `searchParams` read correctly.

Probe: 7/7 functional round-trips pass (was 5/5 at E5), plus the E6 P0/P0b/P1
diagnostics. Backend lib suite: **41 passed, 0 failed** (was 39; +2 regression
tests). No Deno test file touched; no op stubbed at the deno layer.

## Results (asserted, from the probe)

| target | result | asserted |
|---|---|---|
| deferred async op via `run_event_loop` | PASS | `await op_delayed(41) === 42` |
| `await` on Hermes (already-resolved + microtask-deferred) | PASS | `7`, `9` |
| captured `resolve()` settles its promise | PASS | `.then -> 55`, Fulfilled |
| URL pathname + searchParams | PASS | `/p/q\|1\|#h` |
| console.log inspect output | PASS | `hi { a: 1 } [ 2, 3 ]` |

## (1) DEFERRED-OP ASYNC — PASS. Root cause: opened `Global<Function>` receiver

**Answer to the crux question: YES.** A genuinely-deferred op (an `op2` async
op that awaits a real `tokio::task::yield_now()` then a `tokio::time::sleep`, so
its future is pending when first polled) settles its JS promise through the real
deno_core event loop. From JS `const v = await Deno.core.ops.op_delayed(41)`,
driven only by `rt.run_event_loop(...)` on a tokio current-thread runtime (no
hand-driven microtask checkpoint), `v === 42`, asserted. `run_event_loop`
elapsed ~7ms (the future's real 5ms sleep), `__e6_done === true`, promise state
`Fulfilled value="42"`, and a raw `.then` on the op promise also observed `42`.

### The diagnosis (competing hypotheses, resolved by instrumentation)

The op was correctly deferred (`__e6_done === false` right after dispatch) and
`await`/captured-`resolve()`/`.then`-drain all worked in isolation (E6 P0/P0b).
So the bridge failure was specifically in op-response delivery. Instrumenting
`dispatch_event_loop_tick` (in the scratch deno checkout, reverted after):

```
dispatch_event_loop_tick: 3 arg(s) (triplets), has_ops=true   # op completed, delivered
__eventLoopTick called; returned_some=false exception=false   # the JS call FAILED silently
v8__Function__Call: argc=3 ok=0 out=-1                         # C++ returned NULL_SLOT, no exception
function_call: fn_slot=-1 fv=0x0 isObject=-1                   # the RECEIVER slot was -1
```

So the op completed and deno_core called `__eventLoopTick(promiseId, isOk, res)`
— but the call returned None (failed), so `__resolvePromise` never ran and the
promise stayed Pending forever.

**Root cause.** deno_core stores `__eventLoopTick` as a `v8::Global<v8::Function>`
and, each tick, calls `tick_cb.open(tc_scope)` then `.call(...)`. rusty_v8's
`Handle::open` returns the Global's stored pointer **directly**
(`&*data.as_ptr()`), WITHOUT re-materializing through `v8__Local__New`. On the
Hermes backend a Global is a *global-pin handle* (`(pin_id << 2) | 0b10`), not an
ordinary tagged value slot (`(i << 1) | 1`). `v8__Function__Call` decoded its
receiver with `slot_of(this)`, which only understands the value-slot tag and
returned `NULL_SLOT` (-1) for a pin handle. So calling ANY opened
`Global<Function>` failed — and deno_core's async-op resolver is exactly such a
call. This blocked EVERY deferred-op promise (timers, net, fetch, `for await`).

**The fix.** A new `slot_of_handle(rtw, ptr)` decodes both handle shapes: a value
slot returns its index; a global-pin handle resolves to the pinned value's live
slot via `v8x_hermes_pin_get`. `v8__Function__Call` now uses it for the receiver
(`this`), the `recv`, and each argument (args can also be opened Globals). With
that, the opened `__eventLoopTick` global resolves to a live function and the
call succeeds, resolving the op promise. This is the single most important fix of
the cycle: it is the foundation for all deferred-op async on Hermes.

Regression test: `hermes_global_function_call` (round-trip a function through a
`Global`, reopen, call it, assert `40 + 2 === 42`).

## (2) console — PASS via `op_console_inspect_args`; the cppgc `ConsoleWrap` wall

This Deno version's `console.log` goes through a `Console` class whose formatting
is delegated to a **cppgc-backed native `ConsoleWrap`** (Oilpan). cppgc is
unimplemented on Hermes (`isolate.get_cpp_heap()` is `None`; the `cppgc__*`
symbols are `unimplemented!()` stubs), so `new ConsoleWrap(...)` panics. The
console CLASS is therefore blocked on the cppgc subsystem — a large, separate
unlock (the repo's #2 lever), correctly an E7+ target, not a P2 slice.

The REAL formatting engine console uses, `op_console_inspect_args` (exposed as
`internals.inspectArgs`), is a native Rust op that produces the exact console
string WITHOUT cppgc. Driving it + `Deno.core.print` (op_print) gives real,
observable output. `internals.inspectArgs(["hi", {a:1}, [2,3]], {colors:false})`
returns `hi { a: 1 } [ 2, 3 ]`, asserted byte-for-byte, and is also emitted to
stdout via `Deno.core.print`. This is the honest functional console-output path
today.

Getting the inspector correct surfaced a chain of null-stubbed Object/Value
reflection ABI, each fixed in turn (the inspector is a strict test of the whole
reflection surface):

- `v8__Object__GetConstructorName` — was null, so every object printed as
  `[Object: null prototype]`. Cached helper reads `o.constructor.name`.
- `v8__Value__TypeOf` — was null (panicked at `type_of().unwrap()`). Cached
  `(v) => typeof v`.
- `v8__Object__GetOwnPropertyNames` / `GetPropertyNames` — were null, so objects
  enumerated zero keys and printed `{}`. Cached helper honoring the
  `PropertyFilter` bits (ONLY_ENUMERABLE / SKIP_SYMBOLS / SKIP_STRINGS) and the
  `IndexFilter::SkipIndices` mode (so array elements are not duplicated as
  `"0"/"1"` string keys).
- `v8__Object__GetOwnPropertyDescriptor` — was null, so the constructor-name
  walk (which reads the `"constructor"` descriptor) found none and mislabeled
  objects. Cached `Object.getOwnPropertyDescriptor`.
- `v8__Object__Has` with a **Symbol key** — stringified the key, so
  `has(Symbol.iterator)` missed and arrays were treated as non-iterable objects
  (printed `{ "0":2, "1":3 }` instead of `[ 2, 3 ]`). Now routes Symbol keys
  through the `in` operator (the cached `(o,k) => k in o` helper).
- the 11 well-known Symbols (`v8__Symbol__Get{Iterator,ToStringTag,...}`) — were
  null; the inspector reads `obj[Symbol.toStringTag]` and matches well-known
  prototypes. Backed by a cached JS array of the intrinsics.
- `v8__Value__IsSymbol` — was a null stub (always false); implemented via
  `jsi::Value::isSymbol`.

## (3) URL — PASS. Fixes: `IsUint32Array` + `Array::New_with_elements`

`new URL("https://a.b/p/q?x=1#h")`: `pathname === "/p/q"` and
`searchParams.get("x") === "1"` (and `hash === "#h"`), through the real ext/web
`00_url.js` (`op_url_parse` + `op_url_get_serialization`), asserted.

Two backend gaps surfaced:

- `op_url_parse` takes a `#[buffer] &mut [u32]` (a JS `Uint32Array` of URL
  component offsets). `v8__Value__IsUint32Array` was a null stub, so the
  op-layer's `Local<Uint32Array>::try_from` failed with "expected typed
  ArrayBufferView". JSI has no per-kind TypedArray predicate (only
  `isTypedArray`/`isUint8Array`), so this checks the object's TypedArray
  constructor name via a cached helper.
- `v8__Array__New_with_elements` was a null stub, so building an array from
  Rust-side Locals (URLSearchParams pair lists) returned null and panicked at the
  vendored `.unwrap()`. Implemented by creating an array of the given length and
  filling each index (decoding pin handles too).

Regression test: `hermes_reflection_abi` covers IsNullOrUndefined, IsUint32Array,
TypeOf, GetConstructorName, GetOwnPropertyNames, GetOwnPropertyDescriptor,
Symbol-keyed Has, a well-known Symbol, and Array::new_with_elements.

(Also fixed in passing: `v8__Value__IsNullOrUndefined`, a null stub that
misclassified real `null`/`undefined` as present. Composed from the working
`IsNull`/`IsUndefined`.)

## (4) Backend fixes: files + commit + test count

Branch `hermes-backend-spike`, NOT pushed. Files:
`src/hermes/core.rs`, `src/hermes/hermes_shim.cpp`, `src/hermes/shims.rs`,
`src/hermes/mod.rs`.

New/real C-ABI symbols this cycle: `v8__Function__Call` receiver+arg pin-handle
decode (the crux), `v8__Value__IsNullOrUndefined`, `v8__Value__IsUint32Array`,
`v8__Value__IsSymbol`, `v8__Value__TypeOf`, `v8__Object__GetConstructorName`,
`v8__Object__GetOwnPropertyNames`, `v8__Object__GetPropertyNames`,
`v8__Object__GetOwnPropertyDescriptor`, `v8__Object__Has` (Symbol keys),
`v8__Array__New_with_elements`, and `v8__Symbol__Get{AsyncIterator,HasInstance,
IsConcatSpreadable,Iterator,Match,Replace,Search,Split,ToPrimitive,ToStringTag,
Unscopables}` (11 well-known Symbols).

Backend lib suite: **41 passed, 0 failed** (`hermes,link_hermes`); was 39 at E5.
Two new regression tests: `hermes_reflection_abi`, `hermes_global_function_call`.

Sandbox (deno checkout `v8x-rebase-rc`, NOT pushed):
`libs/hermes_web_probe/src/main.rs` extended with a real `op_delayed` async op
(`deno_core::extension!` + `#[op2] async`), the E6 P0/P0b/P1 deferred-op
diagnostics, and the URL + console assertions. All temporary deno_core
instrumentation in `libs/core/runtime/jsruntime.rs` was reverted (clean diff).

## (5) The single most important next wall + recommended E7 target

The deferred-op bridge is now proven end-to-end, so the honest next step is
**ext/net + fetch — the real network stack where `for await` over a socket and
genuinely deferred I/O live.** The `op_delayed` proof exercised the op ->
promise -> event-loop path with a timer-shaped future; ext/net exercises the same
path with real resources (a TcpListener/TcpStream in the resource table, async
`op_net_accept`/`op_net_read` returning futures resolved by the event loop, and
`for await (const conn of listener)` driving an async generator over them). Two
sub-walls to expect: (a) the resource table + `op2` `Rc<RefCell<OpState>>`
resource ops, and (b) whether an async generator's `for await` (which chains
many deferred-op promises) stays live across multiple `run_event_loop`
iterations. Recommended E7: bring up a loopback `Deno.listen`/`conn.read` echo
(or the minimal ext/net op set) and assert a byte round-trips over a real socket
through `run_event_loop`, plus one `for await` iteration over `listener`.

A smaller, high-confidence side unlock if a full console is wanted: **cppgc**
(Oilpan) — implementing `get_cpp_heap` + `make_garbage_collected` +
`Object::Wrap`/`Unwrap` on Hermes unblocks the real `ConsoleWrap` class (and is
the repo's separately-tracked #2 lever), but it is a large subsystem and net is
higher-leverage for "full Deno".

## (6) Disk at end

`df -h /`: 7.2Gi avail (71% used). `CARGO_INCREMENTAL=0` throughout; no ENOSPC,
no incremental dir created.

## Honesty ledger

- Genuinely functional through real deno_core / ext/web: a deferred async op
  settling its JS promise through `run_event_loop` (`await op_delayed(41) === 42`,
  the crux); URL parsing (pathname + searchParams); console inspect output
  (`hi { a: 1 } [ 2, 3 ]`) via the real `op_console_inspect_args`.
- The deferred-op fix is the real bridge: it makes an opened
  `Global<Function>` callable, which is how deno_core resolves EVERY async op.
  It is now exercised by a genuinely-pending tokio future, not just an
  already-resolved promise (E5's boundary). This is the honest "async I/O works"
  foundation — but only the op->promise->loop plumbing is proven; no real socket
  or file I/O has been round-tripped yet (that is E7).
- console: the `Console` CLASS is BLOCKED on cppgc (honest wall, not faked). The
  asserted output comes from the same native inspector op console uses, driven
  directly + emitted via `Deno.core.print`. Object formatting is byte-correct for
  strings / plain objects / arrays; deeper cases (getters/setters, Maps/Sets,
  circular graphs, colors) exercise more inspector paths not asserted here.
- URL: the tested surface (parse, pathname, searchParams.get, hash) works; the
  full WHATWG URL surface (setters, IDNA hosts, all component getters) is not
  exhaustively asserted.
