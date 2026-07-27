# C5: parse-free AOT execution on the Hermes backend, measured

**Result: YES.** Hermes Bytecode (HBC), compiled ahead of time from JS source
with `hermesc`, runs through the v8x Hermes backend via a new buffer-eval
entry point, produces the identical result to running the source it was
compiled from, and is measured to run **21.4x faster** than parsing+compiling
the same source at eval time, on a ~1.4MB bootstrap-shaped chunk of JS. This is
the concrete, measured proof that AOT-compiling to HBC recovers the
parse+compile cost of a JS runtime bootstrap on this backend.

## Test command and result

```bash
cargo test --no-default-features --features hermes,link_hermes \
  --lib hermes:: -- --nocapture
```

```
test hermes::hermes_hbc::hermes_hbc_runs_through_backend ... ok
test hermes::hermes_hbc::hermes_hbc_parse_free_win ... ok
```

(plus the 4 pre-existing C2/C3/C4 tests, all still green: 6 passed; 0 failed)

## Step 1: getting hermesc (the AOT compiler)

`hermesc` is not in the vendored `hermes.framework` (that ships only the
runtime, not the compiler). The lightest path worked on the first try: the
deprecated `hermes-engine` npm package. Its **v0.11.0** release exactly
matches the vendored `hermes.framework`'s version (`vendor/hermes/HERMES_VERSION`),
so the bytecode it emits is guaranteed compatible with the linked runtime.

- Downloaded `https://registry.npmjs.org/hermes-engine/-/hermes-engine-0.11.0.tgz`
  (15.4MB tarball), extracted only `package/osx-bin/hermesc` (a 2.9MB
  universal arm64+x86_64 Mach-O binary), discarded the rest (Android AARs,
  Windows/Linux binaries, headers already vendored).
- Vendored to `vendor/hermes/bin/hermesc`. Verified with `hermesc --version`:
  `Hermes release version: 0.11.0`, `HBC bytecode version: 84`, matching the
  runtime exactly.
- Total added: 2.9MB. Disk stayed far inside budget (6GB+ free throughout).
- Earlier C2 notes said the npm package "ships no macOS-host linkable
  library", which is still true for the *runtime* (the `.so`s are
  Android-only); it does ship a working macOS-host `hermesc` binary, which is
  the piece C5 actually needed.

## Step 2: compiling JS to HBC

```bash
vendor/hermes/bin/hermesc -emit-binary -O -out foo.hbc foo.js
```

Confirmed the HBC magic on the output (`xxd foo.hbc | head -1`):

```
00000000: c61f bc03 c103 191f 5400 0000 ...
```

`c6 1f bc 03 c1 03 19 1f` is the 8-byte Hermes Bytecode magic
`HermesRuntime::isHermesBytecode` checks for.

## Step 3: running HBC through the backend (the parse-free execution proof)

`HermesRuntime::evaluateJavaScript`/`evaluateJavaScriptWithSourceMap` already
accept either JS source or HBC in the same `jsi::Buffer` and sniff which one
they got; no separate "run bytecode" API is needed. So the shim adds one new
buffer-oriented entry point rather than two:

- `src/hermes/hermes_shim.cpp`: `v8x_hermes_eval_buffer(rtw, data, len,
  source_url, ok)` wraps the raw bytes in a new `OwnedBuffer` (a `jsi::Buffer`
  over an owned copy, since the Rust slice does not outlive the call) and
  calls `runtime().evaluateJavaScript(buffer, url)`. Wrapped in the usual C2
  catch-all (`jsi::JSError` and any other C++ exception mapped to the null
  slot, never unwinds into Rust).
- `v8x_hermes_is_hbc(data, len)` exposes the static
  `HermesRuntime::isHermesBytecode` check with no runtime instance needed.
- `src/hermes/mod.rs`: a new `aot` module (`#[cfg(feature = "link_hermes")]`,
  parallel to the existing `smoke` module from C2) with a small `Rt` handle
  (`Rt::new()`/`Rt::eval_buffer(&[u8], &str) -> Option<String>`) and
  `is_hermes_bytecode(&[u8]) -> bool`. Deliberately NOT wired into `core.rs`'s
  `v8__*` C-ABI surface: this is a standalone proof/bench harness, one
  HermesRuntime per call, the same shape C2's `smoke_eval` used for its
  feasibility proof.

The `hermes_hbc_runs_through_backend` test proves, against a real libhermes:

```
hermes_hbc: isHermesBytecode(source)=false isHermesBytecode(hbc)=true (source 32 bytes, hbc 210 bytes)
hermes_hbc: eval(source) = "42", eval(hbc) = "42"
```

- `is_hermes_bytecode` correctly classifies both a plain JS source buffer
  (false) and hermesc output (true): this is exactly the sniff
  `evaluateJavaScript` relies on internally, checked independently here.
- The SAME `eval_buffer` entry point, fed `"(function(){ return 40 + 2; })()"`
  as source vs. fed the `hermesc`-compiled HBC of that exact source, produces
  the identical result (`"42"`) from two independent fresh runtimes.

## Step 4: the measured win

A bootstrap-shaped JS generator (`generate_bootstrap_shaped_js` in
`src/hermes/mod.rs`) emits 4000 tiny function + constructor + object-literal
definitions (shaped like the many small builtin bindings a runtime bootstrap
module defines), then sums a value computed from every one of them so the
result depends on all 4000 having actually run. This produced **1.4MB of
source**, comfortably in the "bootstrap-shaped" size range the mission asked
for.

For each of 7 iterations, a **fresh** HermesRuntime evaluates the full source
(parse + compile + run) and a **fresh** HermesRuntime evaluates the
`hermesc -O` compiled HBC of that same source (run only, parsing skipped).
Medians, from the passing test run:

| | source (parse+compile+run) | HBC (parse-free, run only) |
|---|---|---|
| median time | 202.36 ms | 9.38 ms |
| size | 1,404,535 bytes | 1,915,205 bytes (1.36x) |

**Delta (parse+compile cost recovered by AOT): ~193 ms. Speedup: 21.4-21.6x**
(stable across repeated full test-suite runs; see raw per-iteration timings
below).

```
hermes_hbc bench [7 iters, cold runtime each]: source-run median = 201.261916ms,
  hbc-run (parse-free) median = 9.404041ms, delta (parse+compile recovered) = 191.857875ms,
  speedup = 21.40x
hermes_hbc bench sizes: source = 1404535 bytes, hbc = 1915205 bytes (1.36x)
hermes_hbc bench raw source times: [203.322541ms, 198.766833ms, 193.698042ms, 207.442125ms,
  200.54ms, 201.261916ms, 202.838834ms]
hermes_hbc bench raw hbc times: [9.514375ms, 9.290291ms, 9.594ms, 9.559667ms, 9.213209ms,
  9.404041ms, 9.285709ms]
```

HBC is ~36% bigger than source here (no minification, and bytecode encodes a
fixed per-function overhead that mostly matters for many tiny functions like
this synthetic benchmark; a real bootstrap with less repetition and denser
functions would likely compress this ratio). The size cost buys a 21x
reduction in the fixed per-boot latency, paid once per JS engine build (or
per app-code build for `deno compile`-shaped shipping), not per process start.

## Load-bearing bug found and fixed: OwnedBuffer needs a NUL terminator

The initial implementation crashed (SIGSEGV, `EXC_BAD_ACCESS`, deep inside
Hermes's lexer/parser call stack) on this exact 1.4MB script, but not on
smaller ones (~340KB reliably crashed too; 66KB did not) - the first
`OwnedBuffer` was a plain `std::vector<uint8_t>` built from `(data, data +
len)`, with no trailing byte.

Root cause: Hermes' JS lexer, like most hand-rolled C++ lexers, reads a
one-byte lookahead past the last real source character and expects a NUL
sentinel there, rather than bounds-checking every single access against
`size()`. `jsi::StringBuffer` (used everywhere else in this backend, e.g.
`v8x_hermes_run`) gets this for free because `std::string::data()` has been
guaranteed NUL-terminated since C++11. A raw `std::vector<uint8_t>` carries no
such guarantee: the byte immediately after the used range is genuinely
out-of-bounds if `size() == capacity()` (the common case for a vector sized
exactly to its contents), so the lexer's lookahead read is an out-of-bounds
read that only crashes when it happens to land on an unmapped page - which
explained the size-dependent crash (small buffers' overrun more often landed
inside other valid heap allocations by luck; larger ones were more likely to
sit at an allocation boundary).

Fix: `OwnedBuffer` now allocates `len + 1` bytes, copies the payload, and
zeroes the extra byte, while `size()` still reports only `len` (the trailing
NUL is invisible to every caller, matching `std::string`'s own contract).
Reproduced the crash directly against the C++ shim (bypassing Rust entirely,
via a standalone harness linking `hermes_shim.cpp`) to confirm the bug lived
in the shim, then confirmed the fix with the same harness before touching the
Rust test. This is the one correctness fix this experiment required; it
applies to ANY future shim entry point that hands Hermes a raw byte buffer of
JS source (not HBC - bytecode has a fixed-size header/footer and no lexer
lookahead, so it does not need this, though the fix pads it too, harmlessly).

## Debugging note: this looked like an infinite hang before it looked like a crash

Under `cargo test`'s default parallel runner, the failure surfaced as an
unkillable-looking, high-CPU process that had to be killed after 5+ minutes;
under `--test-threads=1` it looked the same. It was NOT a hang: `sample`(1)
and repeated `lldb` backtraces showed the stuck thread's program counter
genuinely advancing through different Hermes-internal functions each time,
never resting on one instruction - consistent with a crash-handling or
recovery path doing real (slow) work, not a deadlock. Running the exact same
scenario directly (no `cargo test` harness, no Rust) with a plain
C++-only reproduction immediately surfaced the real signal: `SIGSEGV`, exit
139. Debugging lesson for this backend generally: a Hermes-side memory
corruption can present very differently (hang-shaped vs. crash-shaped)
depending on how much surrounding process/harness machinery is present;
isolating the repro down to the smallest possible driver (bypass Rust,
bypass `cargo test`, call the shim's `extern "C"` functions directly from a
tiny hand-linked C++ `main`) was what actually revealed the true signal.

## Tying this back to the Deno-bootstrap / `deno compile` story

This is the same shape of win the E5/E6 QuickJS bytecode-boot experiments and
the C0 AOT-vs-snapshot framing predicted for Hermes specifically: HBC is CODE
only (not a serialized heap), so it still *runs* the bootstrap every process
start, but it skips *parsing and compiling* that bootstrap, which C0 measured
as the fraction AOT can actually recover (as opposed to heap-construction
execution time, which AOT does not touch). On this backend, 21x is the
measured size of that recoverable fraction for a bootstrap-shaped chunk of
JS - directly comparable to, and consistent with, the framing in
`E6-quickjs-real-snapshot.md`.

For `deno compile`: this is the concrete mechanism to ship a Hermes-backed
Deno binary that embeds AOT-compiled HBC of the Deno JS runtime bootstrap (and
optionally the user's TypeScript-compiled-to-JS application code) instead of
shipping source text to be parsed at every cold start - trading a ~36% larger
bytecode blob for a >20x reduction in the parse+compile portion of startup
latency, with zero snapshot/heap-serialization machinery required (unlike the
V8/QuickJS snapshot path, HBC needs no special "replay tape" or heap-graph
serializer; it is produced by an ordinary ahead-of-time compiler run over the
same JS source that would otherwise be parsed at boot).

## Recommended next step

1. **Measure on a REAL Deno/runtime-shaped bootstrap**, not just the
   synthetic generator here, to get a production-representative number (this
   experiment's generator is intentionally simple - many tiny, similarly
   shaped definitions - a real bootstrap has more varied code shapes,
   closures, and control flow that could shift the parse/compile-vs-run ratio
   either direction).
2. **Wire `prepareJavaScript`/`evaluatePreparedJavaScript`** as a second code
   path alongside the current `evaluateJavaScript`-only implementation: JSI
   docs note `PreparedJavaScript` objects can be shared across multiple
   Runtime instances of the same concrete type, which could let one hermesc
   compile step's *in-process* prepared form be reused across several
   isolates in a single process (relevant for any future multi-isolate
   Hermes host), distinct from the file-level HBC reuse this experiment
   already demonstrates across process runs.
3. **Try `hermesc` without `-O`** and compare: the optimizing pipeline costs
   compile-time (paid once, ahead of time, off the hot path) in exchange for
   possibly faster bytecode; this experiment always used `-O` and did not
   isolate that variable.
4. Feed this into a real `v8x_hermes_run`-style integration once the broader
   `Script::Compile`/`Script::Run` v8 C-ABI surface (from C3) grows a
   bytecode-aware entry point, so a v8x consumer (not just this standalone
   harness) can request parse-free execution through the ordinary
   `v8::Script` API, not only through the standalone `aot` module added here.
