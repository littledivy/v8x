# E11 — the honest benchmark case: is Hermes worth adding as a Deno engine?

This cycle makes the case with NUMBERS, not adjectives, and reports where
Hermes LOSES as prominently as where it wins. Every number below is measured on
ONE machine (arm64 macOS, this dev box), back to back. The comparison baseline
is the same-machine QuickJS-vs-V8 suite in `~/deno-quickjs/BENCHMARKS.md`; I
re-ran QuickJS and V8 for the exact same tasks here to confirm and to keep the
comparison truly same-machine.

Backends:
- **Hermes** = vendored `hermesvm.framework` (`Hermes 260318099.0.1`, HBC 99),
  measured through the v8x Hermes backend's `aot::Rt` eval path
  (`src/hermes/mod.rs`, module `e11_bench`).
- **QuickJS** = `~/deno-quickjs/deno` (2.9.3), the shipping QuickJS-backed Deno.
- **V8** = stock `deno` 2.9.2 (canary), aarch64.

## Brutal-honesty summary (read this first)

1. **Is the JIT reachable through our JSI embedding? NO.** The vendored
   `hermesvm.framework` was built WITHOUT the JIT codegen backend. Flipping
   every JIT knob the JSI `RuntimeConfig` exposes (`withEnableJIT(true)` +
   `withForceJIT(true)` + `withJITThreshold(0)`) changes compute timings by at
   most **1.5%** (noise) across all four CPU tasks. Symbol evidence corroborates:
   the framework has the JIT *dispatch* plumbing (`_jitCallImpl`, the `JITMode`
   CLI flag, "JIT is on"/"JIT Enabled" strings) but ZERO JIT *emitter/codegen*
   symbols (no `JitEmitter`, no arm64 code generator). **Hermes is
   interpreter-only in our embedding.** The "small like QuickJS but faster
   compute via JIT" thesis is therefore DISPROVEN for this framework build; the
   compute story must stand on the interpreter alone.

2. **Compute: Hermes is NOT faster than QuickJS.** On the two heaviest CPU tasks
   it is slower than QuickJS; it ties on string build and beats QuickJS on
   JSON. Against V8 it is 3x-24x slower. There is no compute win here.

3. **Startup: the real win, and it is real.** Hermes engine construction is
   ~0.25 ms and the parse-free HBC advantage is 23x on bootstrap-shaped JS
   (215 ms of source parse+compile recovered down to 9.3 ms of run-only). This
   is the genuine value proposition and it is measured, but it is an
   ENGINE-LEVEL number, not a full-Deno `deno eval 0` cold start (we have no
   Deno binary on Hermes yet).

4. **Size: the other real win.** The Hermes runtime is a 6.3 MB arm64 slice
   (12 MB fat) vs a 71 MB QuickJS Deno and an 83 MB V8 Deno. Engine footprint
   is roughly an order of magnitude smaller than a full Deno binary.

5. **HTTP: a real JS server got 4 of 5 stages working on Hermes (boot, bind,
   accept, read) then hit ONE backend wall (write-buffer marshaling), so there
   is NO honest req/s number.** The reported HTTP figure is a per-request
   JS-handler-cost PROXY (Deno.serve does not exist on Hermes). On the
   compute-handler shape (fib(24)+JSON per request) Hermes is interpreter-bound
   like QuickJS and loses to V8's JIT by ~18x. See the HTTP section.

Verdict in one line: **Hermes earns a place as a small-footprint,
fast-cold-start engine (edge/serverless/mobile), NOT as a compute engine — in
THIS framework build it has no JIT and is slower than QuickJS on heavy CPU
work.**

## Reproduction (all commands)

```bash
# (A) Hermes numbers — the E11 bench module (JIT reachability, compute,
#     startup, HBC-boot, handler cost). ~3 min, cold runtime each iter.
cd /Users/divy/gh/v82jsc
export DYLD_FRAMEWORK_PATH=/Users/divy/gh/v82jsc/vendor/hermes
CARGO_INCREMENTAL=0 cargo test --no-default-features \
  --features hermes,link_hermes --lib hermes::e11_bench \
  -- --nocapture --test-threads=1 | grep E11:

# (B) same-machine QuickJS + V8 CPU tasks (identical task shapes)
#     writes cpu.js with fib32x5 / loopsum50M / stringbuild1M / json200k,
#     21 runs each, median. (see the "CPU task JS" block below for the file)
~/deno-quickjs/deno run cpu.js       # QuickJS column
~/.deno/bin/deno run cpu.js          # V8 column

# (C) same-machine QuickJS + V8 per-request handler cost (HTTP proxy)
~/deno-quickjs/deno run handler.js 20000
~/.deno/bin/deno run handler.js 200000

# (D) sizes
du -sh /Users/divy/gh/v82jsc/vendor/hermes/hermesvm.framework   # 12M fat
lipo -thin arm64 .../hermesvm -output /tmp/h && stat -f%z /tmp/h # 6.3M arm64
ls -la ~/deno-quickjs/deno ~/.deno/bin/deno                      # 71M / 83M

# (E) JIT symbol evidence
nm /Users/divy/gh/v82jsc/vendor/hermes/hermesvm.framework/Versions/Current/hermesvm \
  | grep -c JitEmitter        # 0 — no codegen backend compiled in
nm .../hermesvm | grep -i jit | c++filt   # only _jitCallImpl dispatch + JITMode flag
```

The `cpu.js` and `handler.js` files are reproduced verbatim at the end of this
doc so the numbers can be re-derived exactly.

## (1) JIT reachability — determined empirically AND by symbols

`vendor/hermes/include/hermes/Public/RuntimeConfig.h` exposes JIT builder
knobs: `EnableJIT` (default false), `ForceJIT`, `JITThreshold`, `JITMemoryLimit`.
So the JSI embedding CAN request the JIT. I added a second runtime constructor
`v8x_hermes_runtime_new_jit` (`src/hermes/hermes_shim.cpp`) identical to the
default one but with `.withEnableJIT(true).withForceJIT(true).withJITThreshold(0)`,
and a Rust `Rt::new_jit()`, then ran every CPU task through BOTH runtimes:

```
E11:COMPUTE task=fib32x5       jit_off_ms=1036.75 jit_on_ms=1036.27 on/off_ratio=1.000
E11:COMPUTE task=loopsum50M    jit_off_ms=550.97  jit_on_ms=552.26  on/off_ratio=0.998
E11:COMPUTE task=stringbuild1M jit_off_ms=42.48   jit_on_ms=42.59   on/off_ratio=0.997
E11:COMPUTE task=json200k      jit_off_ms=100.01  jit_on_ms=101.58  on/off_ratio=0.985
E11:JIT reachable=false max_on_off_delta=1.5%
```

`ForceJIT` compiles every function on first call, bypassing the warmup
threshold; if a real codegen backend existed, fib(32) (deep recursion, the most
JIT-favorable task) would move visibly. It does not (0.0% on fib). Max delta
across all tasks is 1.5%, i.e. noise.

Symbol evidence (the mechanism):

```
$ nm hermesvm | grep -c JitEmitter
0
$ nm hermesvm | grep -i jit | c++filt   (all 34 matches)
  hermes::vm::JSFunction::_jitCallImpl(...)       # dispatch stub (always present)
  hermes::vm::VTable::jitCallArray                # vtable slot
  ...VMOnlyRuntimeFlags::JITMode...               # CLI flag machinery
```

The `_jitCallImpl` methods and the `JITMode` flag are compile-time-unconditional
plumbing; the actual code emitter (`JitEmitter`, the arm64 backend) is absent.
This is a standard Hermes release build with `HERMESVM_JIT` codegen compiled
out. **Conclusion: the JIT is NOT reachable in our embedding; Hermes is
interpreter-only here. No JIT compute win may be claimed.**

(A future cycle that wants the JIT would need to build `hermesvm.framework` from
`vendor/hermes` source with the JIT codegen enabled and re-vendor it; the JSI
knobs are already wired and would then light up via `Rt::new_jit()`.)

## (2) COMPUTE — the exact BENCHMARKS.md CPU tasks, all three engines, same machine

Median ms over 21 runs, lower is better. Hermes fresh runtime per iteration;
QuickJS/V8 via their deno binaries (21 warm runs, median). All three produce the
IDENTICAL result value per task (checked: fib=10891545, loopsum=1249999975000000,
etc.), so the tasks are genuinely equivalent.

| task            | Hermes (interp) | QuickJS (interp) | V8 (JIT) | Hermes vs QuickJS | Hermes vs V8 |
|-----------------|-----------------|------------------|----------|-------------------|--------------|
| fib(32) x5      | 1036.8          | 559.1            | 57.7     | 1.85x SLOWER      | 18.0x slower |
| loop-sum 50M    | 551.0           | 455.6            | 32.8     | 1.21x SLOWER      | 16.8x slower |
| string build 1M | 42.5            | 41.0             | 2.6      | ~tie (1.04x)      | 16.3x slower |
| json 200k obj   | 100.0           | 208.9            | 31.9     | 2.09x FASTER      | 3.1x slower  |

Reading this honestly:
- Hermes **loses to QuickJS** on the two pure-compute tasks (function-call-heavy
  recursion and a tight arithmetic loop) by 1.2x-1.85x. Both are interpreters;
  QuickJS's interpreter is simply faster on this arithmetic/call-dispatch work.
- Hermes **ties** QuickJS on naive string concatenation.
- Hermes **beats** QuickJS 2x on the JSON build/stringify/parse task — its
  object model and native JSON path are stronger here.
- Against V8 (JIT), Hermes is 3x-18x slower across the board, as expected for an
  interpreter.

There is no "faster compute" story for Hermes in this build. The one bright spot
is JSON-shaped object work, where it beats QuickJS.

## (3) STARTUP — engine-level boot, source vs precompiled HBC

HONEST FRAMING: we do NOT have a Deno binary on Hermes, so the BENCHMARKS.md
full-Deno `deno eval 0` cold start (QuickJS 20.5 ms / V8 12.6 ms) is NOT
reproducible for Hermes. What IS measured is ENGINE-LEVEL: construct a fresh
HermesRuntime and evaluate a trivial program, fresh process-equivalent (fresh
runtime) each of 30 iterations.

```
E11:STARTUP boot_only_ms=0.252 boot+src_ms=0.300 boot+hbc_ms=0.254
            src_p50=0.298 src_p99=0.384  hbc_p50=0.253 hbc_p99=0.337
```

- **Hermes runtime construction is ~0.25 ms** and evaluating a trivial `0`
  program adds ~0.05 ms from source. This is fast at the engine level. It is
  NOT comparable to the 12-20 ms full-Deno cold starts, which include loading
  the entire Deno JS bootstrap; it is the engine primitive only.
- On a TRIVIAL program the HBC-vs-source delta is negligible (0.30 vs 0.25 ms):
  there is almost nothing to parse, so runtime construction dominates.

The parse-free HBC advantage only shows on a real, bootstrap-shaped amount of
JS, which is exactly the shape a Deno runtime bootstrap has:

```
E11:HBCBOOT defs=4000 iters=21 src_median_ms=215.24 hbc_median_ms=9.34
            speedup=23.1x src_bytes=1004517 hbc_bytes=2170977 hbc/src_size=2.16x
```

- Parsing+compiling ~1 MB of bootstrap-shaped source takes **215 ms**; running
  the AOT-compiled HBC of that same source takes **9.3 ms**. **23x** parse+compile
  cost recovered, at the cost of a 2.16x larger bytecode blob. This is the C5
  result reconfirmed at 21 iterations, and it is the concrete mechanism for a
  fast `deno compile`-shaped Hermes cold start: ship AOT HBC of the Deno
  bootstrap instead of source, skip the parse tax every process start, with NO
  snapshot/heap-serialization machinery (unlike V8/QuickJS snapshots).

Caveat, stated plainly: HBC still RUNS the bootstrap every start (it is code,
not a serialized heap), so it recovers only the parse+compile fraction, not the
heap-construction execution time. V8's startup snapshot recovers BOTH (it
restores a pre-built heap), which is why a full V8 Deno cold start (12.6 ms)
would likely still beat a hypothetical HBC-only Hermes Deno cold start on the
heap-construction portion. The honest claim is "parse-free, not
execution-free."

## (4) HTTP throughput

**Real JS HTTP server on Hermes: ATTEMPTED, got 4 of 5 stages working, then
hit ONE precise backend wall — no honest req/s number.** I built a genuine JS
HTTP server over real OS sockets (probe:
`/Users/divy/gh/deno-v8x-rebase/libs/hermes_web_probe/src/bin/http_bench.rs`,
not merged/pushed) and drove it with a loopback client. Stage by stage, with
evidence:

1. Boot (webidl+web+net+fetch) on Hermes: **works**.
2. Listener bind: **works** via raw `op_net_listen_tcp` — `listening on
   127.0.0.1:39117 rid=0`. (The ext/net `Deno.listen` JS wrapper itself throws
   an opaque `undefined` on Hermes — a separate op-error-value gap — so the
   server was driven off raw ops, the E7/E8 approach.)
3. Accept a real client connection: **works** — `op_net_accept_tcp` returns
   `[connRid=1, {hostname,port}, {remote}, fd=11]`.
4. Read the request bytes: **works** — `core.read(connRid, buf)` (async
   `op_read`, `#[buffer] &mut [u8]`) completes, step reaches `read-ok`.
5. Write the response: **BLOCKED**. Both `core.write` (async) and
   `core.writeSync` (`op_write_sync`, fast) throw
   `TypeError: expected i32 typeof=object` on the SAME `Uint8Array` that
   `op_read` accepted a moment earlier.

Root cause (isolated): the v8x-Hermes op bridge marshals a WRITE-direction
buffer arg (`#[buffer] &[u8]`, an immutable read-only view of a JS buffer)
incorrectly — reading INTO a JS buffer (`&mut [u8]`) works, but handing Rust an
immutable view of one falls through to an integer coercion (`expected i32`).
This is a real `src/hermes/` op2-buffer-marshaling gap, not a probe bug (proven
by read succeeding and write failing on the identical buffer object). Because no
request completes a full round trip, there is **no honest rps/p50/p99 to
report**, and I did not fake one. Fixing the write-buffer marshaling is its own
backend experiment (a good next cycle), out of scope for a benchmark. Two
tracked backend gaps fell out of this attempt: (1) `Deno.listen` JS wrapper
throws opaque `undefined` (op errors lose their value on Hermes), (2) the
write-buffer marshaling wall above.

So the HTTP throughput number here is the per-request JS-handler-cost PROXY
below, clearly labeled as such — not a Deno.serve rps.

The BENCHMARKS.md key insight holds and frames everything here: a TRIVIAL HTTP
handler is native-hyper/client-bound, so the engine is nearly invisible
(QuickJS and V8 both ~72k rps, essentially equal). The engine only matters on
per-request JS WORK. So the honest, engine-revealing measurement is the
per-request JS-handler cost.

**Per-request JS-handler cost (the honest proxy for req/s), same machine:**

| handler                    | Hermes (interp) | QuickJS (interp) | V8 (JIT) |
|----------------------------|-----------------|------------------|----------|
| trivial (build response)   | 5.78M rps       | 5.08M rps        | 95.6M rps |
| compute (fib24+JSON)/req   | 225 rps         | 422 rps          | 4060 rps  |

- On a **trivial** handler (build a small response object, no real compute),
  all three are so fast (millions of ops/s) that in a real server this cost is
  invisible next to native HTTP framing — matching BENCHMARKS.md's "~72k rps,
  engine invisible" for the trivial Deno.serve handler. Hermes and QuickJS are
  in the same ballpark; V8 is far ahead but it does not matter at this scale
  because the socket dominates.
- On a **compute** handler (fib(24)+JSON per request, the BENCHMARKS.md
  compute-handler shape), the interpreter gap is decisive: Hermes 225 "pure-JS
  rps", QuickJS 422, V8 4060. Hermes is ~half of QuickJS and ~18x slower than
  V8 — the same interpreter-vs-JIT gap the compute table shows. In a real
  compute-heavy server this is where Hermes (and QuickJS) would lose to V8.

These are pure-JS-handler numbers (no socket), labeled as a PROXY, not
Deno.serve rps. BENCHMARKS.md's actual compute-handler rps were QuickJS 408 / V8
3803 — our same-machine pure-JS proxy (422 / 4060) lands right on top of those,
confirming the compute handler is JS-bound and the proxy is faithful.

## (5) SIZE — the second real value prop

| artifact                          | size        |
|-----------------------------------|-------------|
| Hermes runtime, arm64 slice       | **6.34 MB** |
| Hermes runtime, fat (arm64+x86_64)| 12.70 MB    |
| `hermesc` AOT compiler (build-time only, not shipped in the runtime) | 9.12 MB |
| QuickJS Deno binary               | 70.9 MB     |
| V8 Deno binary                    | 82.7 MB     |

The Hermes VM is ~6 MB (arm64), roughly an order of magnitude smaller than a
full Deno binary. This is not an apples-to-apples comparison (the deno binaries
include the whole runtime, ext modules, TS compiler, npm/node compat, etc., not
just the engine) — but the ENGINE FOOTPRINT difference is real and large:
Hermes's engine is ~6 MB where V8's engine alone inside a deno binary is tens of
MB. For mobile / edge / serverless where binary size and memory footprint are
first-class costs, a Hermes-backed Deno would be materially smaller. `hermesc`
is a build-time tool (produces HBC), not shipped in the runtime, so it does not
count against runtime size.

## (6) The written case: is Hermes worth adding as a Deno engine?

Where Hermes WINS (measured):
- **Cold start via parse-free HBC.** 23x recovery of the parse+compile tax on
  bootstrap-shaped JS (215 ms -> 9.3 ms), with no snapshot machinery. Engine
  construction itself is ~0.25 ms. For `deno compile`-shipped, cold-start-bound
  workloads (edge functions, serverless, CLIs), shipping AOT HBC of the
  bootstrap is a concrete, simple, real speedup.
- **Small footprint.** ~6 MB engine vs tens of MB for V8; a Hermes-backed Deno
  would be materially smaller — the mobile/edge/serverless value prop.
- **JSON-shaped object work.** 2x faster than QuickJS on the json200k task (the
  one compute task it wins).

Where Hermes LOSES (measured):
- **No JIT in this framework build.** Interpreter-only; the JIT codegen backend
  is not compiled in and cannot be enabled through JSI. So no compute win, full
  stop, until the framework is rebuilt with the JIT.
- **Slower than QuickJS on heavy CPU.** 1.2x-1.85x slower on fib and loop-sum,
  the two pure-compute tasks. Hermes does NOT dominate QuickJS on compute; it
  trades wins (JSON) for losses (arithmetic/recursion).
- **3x-18x slower than V8** across every CPU task and on the compute HTTP
  handler. Any compute-heavy or per-request-JS-heavy service wants V8.
- **Slower cold start than a V8 snapshot for full Deno**, in principle: HBC is
  parse-free but not execution-free; V8's snapshot restores a pre-built heap and
  skips both. HBC's win is the parse fraction only.

Target use case where Hermes is the right pick: **edge / serverless / mobile
cold-start-bound and size-bound workloads that are IO-bound, not compute-bound**
— many short-lived processes, small binaries, fast first-byte, where the JS work
per request is light (native HTTP dominates) and the engine's job is mostly to
boot fast and stay small. That is precisely the QuickJS niche, and Hermes's HBC
parse-free boot plus small footprint make it a credible sibling there. It is NOT
the right pick for compute-heavy JS: in this build it is beaten by QuickJS on
raw arithmetic and by V8 everywhere.

Honest bottom line: **Hermes is worth adding as a Deno engine for the
small-and-fast-to-start niche, but the pitch must be startup + size, not
compute. The "small like QuickJS but faster via JIT" framing is false for this
framework build — the JIT is not there, and interpreter-to-interpreter it is a
wash-to-loss against QuickJS on compute.**

## What was added this cycle (backend + harness only; NOT pushed)

- `src/hermes/hermes_shim.cpp`: `v8x_hermes_runtime_new_jit()` — a JIT-config
  runtime constructor (`withEnableJIT/withForceJIT/withJITThreshold(0)`), used
  only to empirically test JIT reachability. Inert here (no codegen backend).
- `src/hermes/mod.rs`: `aot::Rt::new_jit()` + module `e11_bench` (tests
  `e11_compute_and_jit`, `e11_startup`, `e11_hbc_boot_win`,
  `e11_http_handler_cost`), all `#[cfg(all(test, feature = "link_hermes"))]`,
  standalone bench harness (no `v8__*` C-ABI change), same shape as the C5
  `hermes_hbc` module.
- Backend suite unaffected (these are additive test-only items). Nothing under
  `vendor/`, no report/history, no main.

## Appendix — exact benchmark JS

`cpu.js` (QuickJS/V8 CPU column, identical task shapes to the Hermes harness):

```js
function fib(n){ return n < 2 ? n : fib(n-1) + fib(n-2); }
function timeit(name, fn){
  const runs = [];
  for (let r = 0; r < 21; r++){
    const t0 = performance.now();
    const res = fn();
    runs.push(performance.now() - t0);
    if (r === 0) globalThis.__res = res;
  }
  runs.sort((a,b)=>a-b);
  console.log(`TASK ${name} median_ms=${runs[Math.floor(runs.length/2)].toFixed(2)} result=${globalThis.__res}`);
}
timeit("fib32x5", ()=>{ let s=0; for(let i=0;i<5;i++) s+=fib(32); return s; });
timeit("loopsum50M", ()=>{ let s=0; for(let i=0;i<50000000;i++) s+=i; return s; });
timeit("stringbuild1M", ()=>{ let s=''; for(let i=0;i<1000000;i++) s+='x'; return s.length; });
timeit("json200k", ()=>{ let a=[]; for(let i=0;i<200000;i++) a.push({id:i,name:'item_'+i,v:i*2,ok:(i&1)===0}); return JSON.parse(JSON.stringify(a)).length; });
```

`handler.js` (QuickJS/V8 per-request handler cost; arg = compute iters):

```js
function fib(n){ return n < 2 ? n : fib(n-1) + fib(n-2); }
function trivialHandler(i){
  const body = "hello world";
  const headers = {"content-type":"text/plain","content-length":String(body.length)};
  return body.length + Object.keys(headers).length;
}
function computeHandler(i){
  const v = fib(24);
  const s = JSON.stringify({n:i, fib:v, ts:i*1000, items:[1,2,3,{a:i}]});
  return JSON.parse(s).fib;
}
function bench(name, fn, iters){
  for (let i=0;i<Math.min(1000,iters);i++) fn(i);
  const t0 = performance.now(); let acc = 0;
  for (let i=0;i<iters;i++) acc += fn(i);
  const dt = performance.now() - t0;
  console.log(`HANDLER ${name} iters=${iters} per_req_us=${(dt*1000/iters).toFixed(3)} pure_js_rps=${Math.round(iters/(dt/1000))}`);
}
bench("trivial", trivialHandler, 1000000);
bench("compute_fib24_json", computeHandler, parseInt(Deno.args[0]||"50000"));
```
