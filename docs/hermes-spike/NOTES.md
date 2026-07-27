# Hermes backend spike (overnight autonomous run)

Goal: prototype a static **Hermes** backend for v8x (`engine_hermes`) that can run
Deno, and test whether AOT-compiling JS (Hermes bytecode / static-hermes /
Porffor) can replace the V8 startup snapshot.

Branch: `hermes-backend-spike`. Loop state in `.omc/hermes-loop/`.

## Status board (updated each cycle)
- [x] C0 research: Hermes embedding API + AOT capabilities
- [x] C0 research: v8x integration surface (how to add engine_hermes)
- [x] C0 research: AOT-solves-snapshot feasibility (HBC / static-hermes / Porffor)
- [ ] C1 scaffold: engine_hermes feature + src/hermes/ skeleton + build.rs
- [ ] C2 build: vendor + build static Hermes, link stubs
- [ ] C3 implement: core v8__* (isolate/context/primitives/strings)
- [ ] C4 test: rusty_v8 harness on hermes, hill-climb
- [ ] C5 AOT experiment: precompile demo JS, boot without parser

## Cycle log
(newest first)

### Cycle 0 - Hermes embedding feasibility (agent: hermes-embed-aot) DONE => GO-WITH-CAVEATS
Verdict: technically feasible, hardest of the 3 backends. Decision: SPIKE it (not a commitment);
de-risk object identity before writing broad surface.
- Embedding: JSI is C++-only, NOT ABI-stable (vtables/STL/mangling). Experimental C ABI in
  API/hermes_abi/hermes_abi.h exists but under-documented/not production. => must write ~570 v8__*
  in C++ against JSI, export extern "C", translate C++ jsi::JSError at every boundary.
- Good news: JSI managed value = PointerValue* = a STABLE, refcounted, GC-updated slot (Hades moving
  GC rewrites the tagged ptr in place). That IS V8's handle indirection. Global/Persistent = natural
  fit; HandleScope = watermark pop; EscapableHandleScope::Escape = move slot to parent.
- BLOCKERS: (1) IDENTITY - JSI hands out no raw ptr; two handles to same JS object differ. Every
  V8 Value*/Object* identity/hash/Map/Set site must reroute to strictEquals OR canonicalize (intern
  one slot per object). Deepest, most invasive. (2) per-Local alloc + atomic refcount cost on hot
  paths. (3) C++-only boundary = most complex backend.
- Build: CMake+Ninja, vendors llvh (NO external LLVM), Intl OFF by default. Size ~8MB app contrib
  (> quickjs ~1MB, < jsc ~12MB). Linux/macOS first-class. PRIOR ART: rust-hermes/rusty_hermes +
  libhermes-sys build Hermes from source "following the rusty_v8 pattern" - use as reference/dep.
- AOT: HBC real+shipping (hermesc -emit-binary; prepareJavaScript sniffs source-vs-HBC magic;
  isHermesBytecode()). Static Hermes (shermes, native via lowering to C) = research branch, NOT
  shipping, needs types. RN 0.84 default = Hermes V1 = bytecode+small arm64 JIT, still not native.

## DECISION (end of C0)
Two overnight tracks:
- TRACK A (Hermes spike): C1 scaffold engine_hermes (link with stubs) -> C2 get static Hermes lib
  (reuse rusty_hermes/libhermes-sys machinery) -> C3 minimal C++ JSI shim for the 9-symbol hello
  world (isolate->context->run script->read string) = the feasibility proof -> later de-risk identity.
- TRACK B (AOT/snapshot): E5 QuickJS bytecode-boot experiment (IN-REPO, existing engine) - measure
  boot-from-bytecode vs boot-from-source for bootstrap-shaped JS; tests whether native-builtins+AOT
  makes snapshot unnecessary. Independent of Hermes.

### Cycle 0 - AOT vs snapshot (agent: aot-snapshot) DONE
CRUX: "AOT solves snapshot" is FALSE as stated. V8 snapshot = serialized INITIALIZED HEAP
STATE (deserialize -> skip running bootstrap, ~2ms restore). AOT/HBC = CODE only (still RUNS
bootstrap each boot, just no parse). They capture different things.
Defensible reframing that DOES hold: native builtins (Hermes/QuickJS both init builtins in C/C++,
not JS) + AOT-compiled runtime bootstrap can make snapshotting UNNECESSARY *iff* parse-free
bootstrap EXECUTION fits the startup budget. So the real question is empirical: how much of Deno's
snapshot win is parse (recoverable by AOT) vs heap-construction execution (not).
- Static Hermes (shermes, native via LLVM): WIP; needs static types for native speed; on UNTYPED
  Deno internals it falls back to dynamic path (no speedup) + typed-mode feature gaps. Not viable
  for compiling the bootstrap.
- Porffor: pre-alpha, ~50% Test262, no eval, cannot run Deno-sized JS. Research reference only,
  NOT a v8x backend.
High-value experiments (prioritized):
  E5 (IN-REPO, do first): QuickJS bytecode-boot - run Deno bootstrap from precompiled qjs bytecode
     vs source vs the abandoned snapshot-tape. If bytecode-boot ~ snapshot-restore, validates
     dropping the tape hacks. Uses existing engine, no Hermes needed. Directly actionable.
  E2: decompose Deno startup (snapshot on vs off) into parse vs execute fractions.
  E1: HBC boot-cost microbenchmark (needs hermesc).
Takeaway: Hermes is the credible AOT/mobile path but CHANGES architecture from "restore state"
to "re-run bootstrap cheaply". The snapshot pain reframes as "make bootstrap exec cheap", which
E5 can test in-repo tonight regardless of Hermes feasibility.

### Cycle 0 - integration surface (agent: v8x-integration) DONE
Plan to add `engine_hermes` is clear and mirrors quickjs:
- Features: `engine_hermes`, `link_hermes`, alias `hermes = [engine_hermes, link_hermes]`.
- `build.rs`: add `build_hermes` on the **build_wamr CMake pattern** (Hermes uses CMake+LLVM
  libs), honor `HERMES_LIB_DIR` override, link `static=hermesvm` + `c++`; add a `setup_vendor`
  `mode=="hermes"` branch + `CARGO_FEATURE_LINK_HERMES` dispatch with early return.
- **Backend = clone `src/quickjs/`, NOT `src/jsc/`**: Hermes JSI `Value` is a 16-byte NaN-boxed
  struct like QuickJS `JSValue`, so the arena-handle + one-refcount-per-slot model in
  `src/quickjs/core.rs` fits; JSC's "pointer IS the value" trick does not.
- Type map: Isolate=Box<IsoState> holding HermesRuntime*, Local=arena slot, HandleScope=root/unroot.
- Surface: 722 v8__ + 26 cppgc__ stubs to LINK; **crdtp__ (55, inspector) is FREE** via engine-
  independent `src/crdtp_shim.rs`. Hello-world path = 9 symbols (Isolate__New, Enter,
  HandleScope__CONSTRUCT, Context__New, String__NewFromUtf8, Script__Compile, Script__Run, read).
- Scaffold: `src/hermes/{mod,hermes_sys,core,shims,misc}.rs` + domain placeholders; auto-gen the
  ~722 no-op shims (clone tools/gen_qjs_shims.sh). Register 4th backend in tests/harness/config.json
  + CI matrix + empty baselines once it links.
Open question gating scaffold: does Hermes expose a C API or only C++ JSI? (waiting on embed agent)
