# E14 — the honest fast-startup truth for Hermes-as-a-Deno-engine

E13 reported a minimal Deno-on-Hermes release binary cold-starting at ~30-35 ms,
SLOWER than V8 deno (16.5 ms) and QuickJS deno (24.1 ms). That number is real
but MISLEADING as a verdict on Hermes startup, because the E13 `boot_trivial`
binary parses and executes the ENTIRE deno_core core bootstrap + ext/web/net/
fetch JS FROM SOURCE on every launch, with no snapshot and no HBC. This cycle
decomposes that number, applies the parse-free HBC lever to the real boot
workload, and gives an apples-to-apples cold-start table. Every number below is
same-machine (arm64 macOS, this dev box), release binaries, medians over fresh
processes.

## Brutal-honesty summary (read this first)

1. **The ~30-35 ms is NOT the engine and NOT mostly dyld.** Decomposed: ~6.5 ms
   is dyld+framework+crt (pre-main), ~26 ms is deno_core core bootstrap +
   ext/web+net+fetch JS being PARSED+EXECUTED FROM SOURCE inside
   `JsRuntime::try_new`, ~0.2 ms is the trivial eval. The engine's own
   construction is ~0.25 ms (E11). So E13's number is a SOURCE-PARSE story, not
   an engine story.

2. **HBC recovers the parse tax on the REAL boot workload: 12.4x.** Compiling the
   exact core+ext bootstrap JS (638 KB, 23 files) to HBC and running it
   parse-free on the real Hermes backend drops the boot from a 20.7 ms source
   median to a 1.68 ms HBC median — ~19 ms of parse+compile recovered. On real
   code the HBC blob is SMALLER than source (0.56x), not larger.

3. **As a real cold-start PROCESS on the same bootstrap, HBC-Hermes beats both.**
   A fresh-process HBC boot of the real bootstrap medians **7.4 ms**, vs V8 deno
   eval 0 at **14.0 ms** and QuickJS deno eval 0 at **21.4 ms**. The
   from-source equivalent process medians 27.4 ms (matching E13).

4. **BUT this HBC-boot number understates a REAL Hermes-Deno cold start, and the
   gap is the snapshot gap.** HBC removes PARSE cost, not EXECUTION cost. In the
   HBC-boot probe the bootstrap bodies run against a permissive `__bootstrap`
   stub, so their control flow executes but their heap-construction WORK
   (building every class prototype, primordial table, WeakMap, stream
   machinery) is stubbed to no-ops. A production Hermes-Deno would run that work
   for real, adding time this probe does not charge. V8's startup snapshot skips
   BOTH parse and that heap-construction execution (it restores a pre-built
   heap); Hermes has no heap-snapshot equivalent, so HBC alone cannot match a
   V8 snapshot on the execution fraction.

5. **Static-linking Hermes would save only ~1.5 ms.** The hermesvm framework's
   full dyld load (RTLD_NOW) is ~1.4-1.9 ms; of the ~6.5 ms pre-main, the rest
   is the base binary's own dyld/crt. dyld is not the bottleneck; parse is.

**Verdict:** Hermes startup IS fast when you use the parse-free HBC path, and on
the parse fraction it is genuinely competitive with (indeed faster than) V8 and
QuickJS. The honest, non-flattering framing: **Hermes's cold-start win is
parse-free HBC + a small engine, NOT a heap snapshot. On short scripts and the
parse-dominated fraction of boot it beats V8; on a FULL runtime cold start the
un-measured heap-construction execution is where V8's snapshot still wins, and
Hermes has no equivalent for it.** The pitch is size + parse-free-HBC + embedding
footprint, landing Hermes in the QuickJS niche (and ahead of it on parse), not
"beats V8's snapshot cold start."

## Part A — decomposition of the ~30-35 ms

Instrumented `boot_trivial` (probe, not merged) with `Instant` timers around
each phase and an OS process-start reader (`sysctl KERN_PROC` p_starttime at
offset 0) for the pre-main dyld+crt slice. `E14_PHASES=1` prints the breakdown.
Warm medians over 5+ runs:

| phase | ms | what it is |
|---|---|---|
| process_start -> main entry | ~6.5 | dyld map+bind (incl hermesvm framework) + crt |
| main -> try_new start | ~0.05 | arg parse, ext `init()` (Rust op assembly) |
| ext_init (webidl/web/net/fetch, Rust side) | <0.7 total | op/middleware assembly, NOT JS |
| **try_new (core bootstrap + ALL ext JS parse+exec)** | **~26** | **the dominant cost** |
| trivial module eval (`1+1`) | ~0.2 | the actual user program |
| **total (wall ~33)** | **~33** | |

Splitting `try_new` by extension set (measured with `--minimal` = deno_core core
only, no web/net/fetch):

| boot | try_new ms | delta |
|---|---|---|
| MINIMAL (deno_core core JS only) | ~14 | deno_core core bootstrap parse+exec |
| FULL (+ webidl/web/net/fetch JS) | ~26 | +~12 ms for ext/web+net+fetch source |

So the ~26 ms full-boot try_new is ~14 ms deno_core core bootstrap + ~12 ms
ext/web+net+fetch, all SOURCE parse+compile+execute. The biggest single file is
`ext/web/06_streams.js` (252 KB). This confirms the E13 hypothesis exactly: the
number is source-parse of the runtime JS, not the engine.

dyld/framework share: a direct `dlopen(hermesvm.framework, RTLD_NOW)` (full
bind) is **1.4-1.9 ms**. So of the ~6.5 ms pre-main, the hermesvm framework is
~1.5 ms and the rest (~5 ms) is the base binary's own dyld+crt. A bare Rust
`exit(0)` binary starts in ~0 ms; the ~5 ms base is the 11 MB executable's own
dylibs (libc++, system) and fixups.

## Part B — the HBC lever on the REAL boot workload

The vendored `hermesc` (`vendor/hermes/bin/hermesc`) emits **HBC version 99**,
matching the vendored framework (the C5 note about HBC 84 is stale post the D6
bump; re-confirmed by reading the compiled header). So AOT HBC of the real
bootstrap is valid input to the framework.

New test `hermes::e14_bootstrap_hbc` (v8x, `#[cfg(all(test, link_hermes))]`)
reads the exact core+ext JS files `boot_trivial` runs, applies the SAME
async-generator lowering the real backend runs on every module (ext/web
`06_streams.js` + `09_file.js` contain `async function*`, which Hermes's
compiler and hermesc reject natively; the E1 pass downlevels them), compiles the
concatenation to HBC with `hermesc -O`, and times cold-runtime SOURCE
parse+compile+run vs precompiled-HBC run on the real backend (25 iters, fresh
runtime each):

```
E14:BOOTHBC real ext boot source assembled: 23 files, 637871 bytes
E14:BOOTHBC hbc compiled: 355123 bytes (0.56x source)
E14:BOOTHBC iters=25 source_median_ms=20.71 hbc_median_ms=1.68 speedup=12.35x parse_recovered_ms=19.04
```

- **Source (parse+compile+run) median: ~20.7 ms** — this IS the ~14-26 ms
  try_new parse cost from Part A, isolated on the real bytes.
- **HBC (parse-free, run only) median: ~1.68 ms.**
- **~19 ms of parse+compile recovered, 12.4x.**
- On real code HBC is 0.56x the source SIZE (compact), unlike E11's synthetic
  gen_bootstrap blob which was 2.16x — real code compresses to bytecode well.

Honesty caveat on the HBC RUN number: under the `__bootstrap` stub the bodies
execute their control flow but do stubbed (Proxy no-op) heap work, so the ~1.68
ms run is parse-free + STUBBED-execution, not parse-free + full-runtime
execution. The load-bearing, faithful figure here is `parse_recovered_ms` (~19
ms) — parse+compile is incurred identically whether or not the body does real
work, so the source-vs-HBC delta is a true measure of the parse tax HBC
eliminates. It is NOT a claim that a full runtime boots in 1.68 ms.

## Part C — fair cold-start comparison

Identical harness for every engine: spawn as a fresh process, `time.perf_counter`
around `subprocess.run`, 5 warmup discarded, median over 31 fresh processes.
`deno eval 0` is the minimal V8/QuickJS boot (their runtimes ship a startup
SNAPSHOT: parse-free AND heap pre-built). Hermes rows: the from-source boots (as
E13), plus the real-process HBC-boot / SRC-boot of the exact bootstrap blob from
Part B (parse-free HBC run vs same lowered source, through the raw Hermes
backend as a fresh process).

| boot (fresh process, median wall) | median ms | p90 ms | vs V8 |
|---|---|---|---|
| **Hermes HBC-boot** (real bootstrap, parse-free) | **7.4** | 8.3 | **0.53x** |
| V8 deno eval 0 (snapshot) | 14.0 | 14.6 | 1.00x |
| QuickJS deno eval 0 (snapshot) | 21.4 | 22.1 | 1.53x |
| Hermes MINIMAL (deno_core core only, source) | 21.3 | 22.0 | 1.52x |
| Hermes SRC-boot (real bootstrap, from source) | 27.4 | 28.8 | 1.96x |
| Hermes FULL boot_trivial (web/net/fetch, source) | 34.5 | 35.1 | 2.47x |

Reading this honestly:

- **From SOURCE (E13's path), Hermes is the slowest**: 34.5 ms full, 27.4 ms for
  the real bootstrap, 21.3 ms even core-only. That is the source-parse tax, and
  it is why E13's number looked bad. V8 and QuickJS never pay it — they ship
  snapshots.
- **With HBC, the Hermes bootstrap process medians 7.4 ms — below both V8 (14.0)
  and QuickJS (21.4).** This is the real, fair, parse-free number on the same
  bootstrap workload, measured as a fresh process with the same spawn overhead.
- **The 7.4 ms is a floor, not the full story.** It charges parse-free load +
  control-flow execution with stubbed heap work. A production Hermes-Deno runs
  the heap-construction work for real; that added execution is exactly what V8's
  snapshot skips and Hermes cannot. So the honest read is: **on the PARSE
  fraction Hermes+HBC beats V8; on a FULL runtime cold start the un-charged
  heap-construction execution narrows or erases that lead, and without a heap
  snapshot Hermes cannot beat V8 on that fraction.**

## The honest verdict

- **Is Hermes startup fast?** Yes, via parse-free HBC, and the engine itself is
  ~0.25 ms to construct. The 30-35 ms E13 number is a source-parse artifact, not
  an engine limit.
- **Against which baseline?** On the parse-dominated boot fraction, HBC-Hermes
  (7.4 ms process) beats V8 deno eval 0 (14.0 ms) and QuickJS deno eval 0 (21.4
  ms) on this same-machine harness. It is comfortably in and ahead of the
  QuickJS niche on parse.
- **Does it beat V8's snapshot cold start for a FULL runtime?** Not
  demonstrated, and probably not. HBC removes parse; V8's snapshot removes parse
  AND heap-construction execution. The 7.4 ms figure stubs that execution, so it
  flatters Hermes relative to a real full-runtime boot. Hermes has no
  heap-snapshot equivalent, so on the execution fraction V8 retains an advantage
  a Hermes-Deno cannot currently close.
- **The real story, stated plainly:** *fast engine + parse-free HBC on the boot
  JS + small footprint, giving a QuickJS-class-or-better cold start on the parse
  fraction, with the primary wins being size and embedding, not a heap-snapshot
  cold start that beats V8 on a full runtime.* If a future cycle wants to beat
  V8 cold start, it needs a Hermes heap snapshot (serialize the initialized
  runtime), not just HBC.
- **dyld:** ~1.5 ms of the ~6.5 ms pre-main is the hermesvm framework;
  static-linking it would save ~1.5 ms — real but minor next to the ~19 ms parse
  tax that HBC already addresses.

## Reproduction

```bash
# (0) prereqs: deno checkout at /Users/divy/gh/deno-v8x-rebase (branch
#     v8x-rebase-rc), v8x at /Users/divy/gh/v82jsc (branch hermes-backend-spike).
export DYLD_FRAMEWORK_PATH=/Users/divy/gh/v82jsc/vendor/hermes
export CARGO_INCREMENTAL=0

# (A) DECOMPOSITION — instrumented boot_trivial phases (release):
cd /Users/divy/gh/deno-v8x-rebase
cargo build --release -p hermes_web_probe --bin boot_trivial
E14_PHASES=1 ./target/release/boot_trivial            # full boot decomposition
E14_PHASES=1 ./target/release/boot_trivial --minimal  # core-only try_new
# dyld/framework share:
#   clang dlopen probe on vendor/hermes/.../hermesvm -> ~1.5 ms RTLD_NOW.

# (B) HBC LEVER on the real boot workload (v8x test) — also emits boot.hbc/js:
cd /Users/divy/gh/v82jsc
export E14_DENO_DIR=/Users/divy/gh/deno-v8x-rebase
export E14_EMIT_DIR=/tmp/v8x-e14-boot
cargo test --no-default-features --features hermes,link_hermes \
  --lib hermes::e14_bootstrap_hbc -- --nocapture --test-threads=1 | grep E14:

# (C) FAIR cold-start table — rebuild probe (consumes E14_EMIT_DIR blobs), then
#     the python harness (31 fresh processes, median wall) over all engines:
cd /Users/divy/gh/deno-v8x-rebase
E14_EMIT_DIR=/tmp/v8x-e14-boot cargo build --release -p hermes_web_probe --bin boot_trivial
E14_EMIT_DIR=/tmp/v8x-e14-boot python3 coldstart.py   # harness in E14 scratch
#   Hermes HBC-boot / SRC-boot: ./boot_trivial --hbc-boot | --src-boot
#   V8:      ~/.deno/bin/deno eval 0
#   QuickJS: ~/deno-quickjs/deno eval 0
```

## Backend / probe changes (NOT pushed)

v8x (`/Users/divy/gh/v82jsc`, branch hermes-backend-spike):
- `src/hermes/mod.rs`: new test module `e14_bootstrap_hbc`
  (`#[cfg(all(test, feature = "link_hermes"))]`) — reads the real deno ext JS,
  applies the E1 async-gen lowering, compiles to HBC via vendored hermesc, times
  source-vs-HBC cold boot on the real backend, and emits boot.hbc/boot.js for
  the process-level probe. Additive test-only; no `v8__*` C-ABI change.
- `src/lib.rs`: `#[doc(hidden)] pub use hermes::aot as hermes_aot`, gated on
  `engine_hermes + link_hermes`, so the deno-side probe can run a real
  parse-free HBC boot process. Adds no C-ABI symbol; spike-only surface.
- Backend suite: `cargo test ... --lib hermes::` = **54 passed, 0 failed**
  (53 prior + this cycle's E14 test), from a clean build.

deno (`/Users/divy/gh/deno-v8x-rebase`, branch v8x-rebase-rc):
- `libs/hermes_web_probe/src/bin/boot_trivial.rs`: E14 instrumentation —
  `E14_PHASES` phase decomposition (incl OS process-start pre-main reader),
  `--minimal` (core-only boot), `--hbc-boot`/`--src-boot` (real parse-free HBC
  vs source boot process via `deno_core::v8::hermes_aot::Rt`). No Deno test files
  touched.

## Disk

Session ran with `/` at ~3-5 GB free; `CARGO_INCREMENTAL=0` throughout. Release
builds of `boot_trivial` (~11 MB) and the v8x test binary fit; no reclaim needed
below the ~2 GB floor (low-water ~3.1 GB during a build, recovered after).
