# Hermes backend spike (overnight autonomous run)

**Directive (user): explore frontiers, truly innovate, do not accept limitations. AOT can make
this wonderful. Bias to BUILD + MEASURE real prototypes/experiments, chase the AOT-native-Deno
vision (tiny instant-start binary). Fully autonomous overnight.**

Goal: prototype a static **Hermes** backend for v8x (`engine_hermes`) that can run
Deno, and test whether AOT-compiling JS (Hermes bytecode / static-hermes /
Porffor) can replace the V8 startup snapshot.

Branch: `hermes-backend-spike`. Loop state in `.omc/hermes-loop/`.

## Status board (updated each cycle)
- [x] *** SHIP: working QuickJS deno binary at ~/deno-quickjs/deno (TS+HTTP+npm verified) ***
- [x] C0 research: Hermes embedding API + AOT capabilities
- [x] C0 research: v8x integration surface (how to add engine_hermes)
- [x] C0 research: AOT-solves-snapshot feasibility (HBC / static-hermes / Porffor)
- [x] C1 scaffold: engine_hermes feature + src/hermes/ skeleton (build.rs untouched, not needed yet)
- [x] C2 FFI feasibility PROVEN: Rust -> extern "C" C++ -> real libhermes JSI
      evals "40 + 2" == 42, prints it (docs/hermes-spike/experiments/C2-hermes-ffi.md).
      Prebuilt macOS hermes.framework (facebook/hermes v0.11.0 release, 4.5MB
      vendored, no source build). `--features hermes,link_hermes`.
- [x] C3 implement: core v8__* hello-world path runs a REAL script THROUGH the
      hermes backend: v8x Rust surface (Isolate/HandleScope/Context/String/
      Script) -> our v8__* -> libhermes evaluateJavaScript -> "hello world" back
      in Rust (docs/hermes-spike/experiments/C3-hermes-helloworld.md).
- [x] C4 SOLVED object identity (strictEquals + hidden-symbol-id hash); 4 hermes tests pass
- [x] C5 AOT: Hermes HBC parse-free boot = 21x faster than source (202ms->9.3ms) THROUGH the backend
- [x] E6 AOT: REAL QuickJS heap snapshot WORKS (but no startup speedup - see log)
- [x] C6 widen surface: Object/Array/Number/Integer/Boolean/Function real + 6 smoke tests pass (12/12
      hermes tests total); registered hermes as 4th harness backend, rusty_v8 baseline honestly 0/16
      (14 targets link, 2 don't - ICU + TypedArray gaps, named not chased); gen_hermes_shims.sh gate
      + symbol-detection bugs fixed (docs/hermes-spike/experiments/C6-hermes-surface.md).
- [ ] C7 AOT E7: Static Hermes native AOT on real bootstrap chunk (push past 'untyped=no win')
- [ ] C8 AOT E8: AOT-native Deno north star - tiny native binary running a Deno program
- [ ] C9 AOT E9: hybrid AOT-bytecode + build-time-precomputed constant data (partial heap)

## Cycle log
(newest first)

### Cycle C6 - widen the surface (Object/Array/Number/Function) + register the rusty_v8 baseline (agent: c6) DONE
PART 1: Object (New/Get/Set/Has, keys coerced to string via JSI toString - no Symbol-key overload in
JSI's C++ surface), Array (New/Length/indexed get-set via the existing Object__GetIndex/SetIndex),
Number/Integer/Boolean (New/Value, Integer routed through Number - JSI has no separate int repr),
Function::Call (jsi::Function::call/callWithThis, argv marshaled through a std::vector<jsi::Value>),
plus v8__Undefined/Null (static jsi::Value factories, needed as Function::Call's receiver) and the
Is{Array,Function,Number,Boolean,String} predicates. 6 new hermes_surface smoke tests (v8x Rust API,
not raw shim calls) build {a:1,b:"x"}, a nested object, a [10,20,30] array summed via indexed gets, a
number/integer/boolean roundtrip, add(19,23) via Function::Call, and cross-check every one against a
real JS JSON.stringify/Array.isArray/.reduce run through Script::compile/run - not just our own
read-back. 12/12 hermes tests pass (6 pre-existing + 6 new), stable parallel and --test-threads=1.
PART 2: registered hermes as tests/harness/config.json's 4th backend (features "hermes,link_hermes",
os "macos" - build.rs's build_hermes panics off-macOS). Empty baselines created. `run.mjs rusty_v8
hermes` runs clean end to end; --check holds against the empty baseline. Honest baseline: 0 passing /
16 total, 14 targets LINK (0 pass each - they exercise slots/flags/entropy/External/snapshot/platform
machinery this cycle didn't touch), 2 don't link (rv8_test_api, rv8_test_cppgc) on genuinely-missing
ICU symbols (icu_get_default_locale/icu_set_default_locale/udata_setCommonData_77) +, for test_api
only, the 11 TypedArray-constructor symbols. Named, not chased tonight per the mission.
BONUS FIX (gen_hermes_shims.sh): found and fixed the actual root cause behind the old "14 hand-appended
symbols" note - (1) gate-preservation was entirely missing (blind regen dropped every hand-added
`#[cfg(not(feature = "link_hermes"))]`, breaking the stub build's link step) - fixed by reading the
CURRENTLY-gated set from the checked-in file first and reapplying it; (2) the symbol-scan regex matched
mid-identifier (truncating e.g. std__shared_ptr__v8__Platform__CONVERT__... to a wrong shorter name) and
only scanned vendor/rusty_v8/src/*.rs one level deep, silently missing src/scope/raw.rs entirely (the
file that declares TryCatch/Allow/DisallowJavascriptExecutionScope CONSTRUCT). Both fixed at the regex/
glob level; verified idempotent (two back-to-back runs on an unchanged tree produce a byte-identical
shims.rs) and no regression on the stub (`--features hermes`) or quickjs builds.
Full report: docs/hermes-spike/experiments/C6-hermes-surface.md.
Next: ICU trio (smallest highest-leverage unlock - both nonlinking targets need only this), then
TypedArrays to fully unlock test_api's link, then TryCatch/exception surfacing.

### Cycle C5 - Hermes HBC parse-free AOT through the backend (agent: c5) DONE - the 'AOT wonderful' number
Same shim entry (v8x_hermes_eval_buffer) runs JS source OR hermesc-compiled HBC transparently (Hermes
sniffs the 8-byte magic). isHermesBytecode(source)=false, (hbc)=true verified (magic c6 1f bc 03..).
MEASURED (1.4MB bootstrap-shaped JS, 4000 tiny fn/obj/proto defs, 7 cold-runtime iters, medians):
  source parse+compile+run ~202 ms  vs  HBC run-only ~9.3 ms  =>  ~21x faster, ~193ms parse+compile
  recovered by AOT. HBC size 1.36x source.
hermesc from deprecated hermes-engine npm v0.11.0 (matches framework, HBC v84), 2.9MB, vendored to
vendor/hermes/bin/hermesc.
Bug found+fixed: OwnedBuffer needed a trailing zeroed byte (not counted in size()) - Hermes lexer does
a 1-byte OOB lookahead read that SIGSEGVs on raw vector buffers >~300KB (jsi::StringBuffer dodges it
via std::string NUL). First looked like an infinite hang under cargo test; a standalone C++ repro
revealed the real SIGSEGV.
SYNTHESIS (E6 + C5): the AOT-vs-snapshot question is now answered with data. On QuickJS a heap-snapshot
restore is NOT faster than re-running (no mmap fastpath). On Hermes, parse-free HBC boot is 21x faster
than source, and Hermes builtins are native C++ so ONLY the runtime/app bootstrap runs at boot. =>
parse-free AOT bytecode, not heap-snapshot, is the real startup lever. This IS 'AOT makes startup
wonderful', demonstrated. Maps directly to deno compile shipping HBC not source.
Next: measure on a REAL Deno bootstrap chunk; wire prepareJavaScript for cross-isolate sharing; -O vs plain.

### Cycle C4 - Hermes object identity SOLVED (agent: c4) DONE - deepest blocker cleared
Both identity-sensitive parts of the V8 C-ABI reroute through JSI primitives:
- v8__Value__StrictEquals/SameValue -> jsi::Value::strictEquals (shim v8x_hermes_strict_equals).
- v8__Object__GetIdentityHash -> lazily attach a HIDDEN non-enumerable Symbol-keyed prop (real JS
  Symbol + Object.defineProperty; no JSI-native symbol-prop API) holding a monotonic id. VERIFIED
  stable: same object via 2 independent slots -> hash 1 & 1; different object -> 2. Invisible to
  Object.keys/JSON/for-in (visible to getOwnPropertySymbols = correct JS, enumerable:false).
- 4 hermes tests pass; quickjs + stub-hermes still clean. Bonus fixes: v8__Value__IsObject/ToObject,
  a process-wide init_v8_once shared across hermes test modules (V8::initialize gates one global state
  machine; a 2nd module's private Once panicked).
Residual risks (documented, not hidden): (1) SameValue==StrictEquals, not exact for NaN/+0/-0 (JSI has
no bit-level float inspection). (2) no canonicalization (interned slot per obj) built - likely
unneeded since strictEquals+GetIdentityHash match V8's embedder identity contract, BUT unaudited
whether any Rust-side rusty_v8 code hashes raw Local pointers directly (bypassing GetIdentityHash).
(3) GetIdentityHash costs 1-2 real JS calls, unmeasured.
SHARP EDGE: tools/gen_hermes_shims.sh drops hand-added cfg(not(link_hermes)) gates on re-run - do NOT
blindly re-run it; hand-patch new stubs. NEEDS FIX before more shim regen.
NEXT: AOT flourish (run Hermes HBC bytecode through the backend, parse-free) + widen Object/Array +
register 4th backend in tests/harness/config.json (after auditing residual risk 2).

### Cycle C3 - Hermes backend runs hello world through v8 C-ABI (agent: c3, opus) DONE (headline)
A v8x smoke test drives the VENDORED rusty_v8 Rust surface: Isolate -> scope! -> Context -> String ->
Script::compile -> Script::run -> to_rust_string_lossy => "hello world", executed on real libhermes.
Source is compiled+run by OUR v8__Script__Compile/Run and read via the same String/Value path real V8
strings use (not the C2 standalone eval). 3 tests pass (C3 + both C2 smokes). No regressions on stub-
hermes or quickjs.
- Design: arena lives C++-SIDE (jsi::Value is move-only + Runtime-bound, cannot sit in a Rust arena
  like qjs). RuntimeWrapper owns unique_ptr<jsi::Runtime> rt (first-declared, last-destroyed per C2
  rule) + vector<jsi::Value> handle table. Local = table index handed to Rust as tagged ptr ((i<<1)|1).
  HandleScope = watermark; DESTRUCT truncates. Thread-local current iso/ctx; one ctx per runtime.
- ~30 v8__* made REAL in src/hermes/core.rs (Isolate lifecycle, HandleScope CONSTRUCT/DESTRUCT +
  EscapeSlot, Context, String NewFromUtf8/OneByte/Length/Utf8Length/WriteUtf8 + the ValueView quintet
  fast-path read, Script Compile/Run, Value ToString). Their stubs gated cfg(not(link_hermes)) so no
  dup symbols; stub build unchanged. C++ bridge: src/hermes/hermes_shim.cpp.
- Test cmd: cargo test --no-default-features --features hermes,link_hermes --lib hermes:: -- --nocapture
  (scope to hermes:: - bare run also builds vendored rv8_test_api = hundreds of stubbed syms, won't link).
- Known compromise: EscapeSlot__escape re-materializes the escaping value as a STRING (exact for the
  hello-world string, lossy for non-string Values). Clean fix = a handles_dup(rtw, slot) shim entry.
NEXT C4: de-risk OBJECT IDENTITY (the deepest C0 risk) - JSI hands out no raw ptr so two Locals to one
object differ. Intern same object twice, show tagged ptrs differ, wire jsi::Runtime::strictEquals,
demo a Set with one logical member. Gate before broad surface / rusty_v8 hill-climb.

### Cycle C3 - Hermes backend runs hello world (agent: executor, opus) DONE => script runs THROUGH the backend
A real script runs end to end through engine_hermes via the v8 C-ABI: a v8x smoke test drives the
vendored rusty_v8 Rust surface (Isolate -> scope! -> Context -> String -> Script::compile -> run ->
to_rust_string_lossy) and gets "hello world" back on real libhermes. Not the standalone C2 eval shim:
the source is compiled+run by our v8__Script__Compile/Run and read back by the same String/Value path
every real V8 string uses. Test: cargo test --no-default-features --features hermes,link_hermes --lib
hermes:: -- --nocapture => "hermes backend ran: 'hello' + ' ' + 'world' = \"hello world\"", 3 passed.
- Design: the arena lives on the C++ SIDE (src/hermes/hermes_shim.cpp). jsi::Value is move-only and
  Runtime-bound, so unlike quickjs's Rust arena, the handle table is a std::vector<jsi::Value> inside a
  RuntimeWrapper (rt declared first => destroyed last, C2 lifetime rule; one runtime/thread). A v8 Local
  is a table index handed to Rust as the tagged pointer ((i<<1)|1) (non-null, slot 0 safe). HandleScope
  = watermark; DESTRUCT truncates the vector. Thread-local current iso/ctx; one context per runtime so a
  Context* handle is the isolate ptr reused.
- Made REAL in src/hermes/core.rs (stubs gated cfg(not(link_hermes)) so stub build + no dup symbols):
  Isolate New/Dispose/Enter/Exit/GetCurrent/Get+SetData/GetNumberOfDataSlots/GetCurrentContext;
  HandleScope CONSTRUCT/DESTRUCT + EscapeSlot reserve/escape; Context New/Enter/Exit/Global; String
  NewFromUtf8/NewFromOneByte/Length/Utf8Length/WriteUtf8_v2 + the ValueView quintet (the actual read
  fast-path to_rust_string_lossy uses - traced empirically, NOT WriteUtf8); Script Compile/Run; Value
  ToString; plus inert Platform/V8-init/ArrayBuffer-allocator/CreateParams lifecycle to bring an isolate
  up (Hermes owns its own heap). Only hello-world-touched symbols were made real; rest stay stubs.
- One real compromise: EscapeSlot__escape re-materializes the escaping value AS A STRING (reads utf8,
  interns fresh string handle above the watermark) because the shim has no dup-slot primitive yet. Exact
  for the hello-world string result; lossy for non-string Values. Clean fix = a handles_dup shim entry
  (copy jsi::Value via its Runtime), one-function follow-up.
- Harness note: use --lib hermes:: not the bare cargo test command - the bare one also builds the
  vendored rv8_test_api target (hundreds of stubbed symbols, fails to link) and the crate's inline
  array-buffer/sandbox #[test]s hit stubs and SIGABRT the shared lib-test process. Scoping is a harness
  detail, not a backend limit.
- No regressions: --features hermes (stub) and --features quickjs (default) both compile clean.
Next: (1) de-risk object IDENTITY (JSI gives no raw ptr; two Locals to one object differ - reroute
identity/hash/Map/Set/Global to strictEquals or intern one slot per object). This is the deepest
unchanged risk and gates broad surface work. (2) OR widen surface: add handles_dup for exact escape,
then Value Is/To, Object/Array, register the 4th backend in tests/harness/config.json + hill-climb.

### Cycle C2 - Hermes FFI PROOF (agent: c2, opus) DONE => GO CONFIRMED (breakthrough)
Rust -> extern "C" C++ shim -> real libhermes JSI evaluateJavaScript("40 + 2") -> asNumber -> 42
back in Rust. The C++-only-JSI blocker from C0 is BEATEN: author v8__* in C++ against JSI, export
extern "C", catch jsi::JSError at the boundary. Test asserts 42; a thrown JS error maps to a sentinel.
- libhermes: PREBUILT facebook/hermes v0.11.0 release asset hermes-runtime-darwin (universal
  hermes.framework + JSI/hermes headers), 4.5MB into vendor/hermes/. NO source/CMake/LLVM build, no
  disk risk. (npm hermes-engine is the WRONG artifact: hermesc + android .so only, no macOS host lib.)
- build.rs build_hermes: cc::Build cpp(true) std=c++17 compiles src/hermes/hermes_eval_shim.cpp,
  includes vendor/hermes/include, links framework=hermes + c++ + rpath; gated on link_hermes; honors
  HERMES_LIB_DIR/HERMES_INCLUDE_DIR. Run: cargo test --no-default-features --features hermes,link_hermes --lib hermes_smoke.
- Real JSI rules learned (carry into every impl): (1) the Runtime must OUTLIVE any caught jsi::JSError
  (its embedded Value dtor calls back into the Runtime) - declare rt in outer scope. (2) one
  HermesRuntime per thread. (3) link_hermes surfaced 14 more scope/platform stub symbols (added to
  shims.rs); quickjs + stub-hermes builds still clean, no regressions.
Deepest remaining risk unchanged: object IDENTITY (JSI hands out no raw ptr) - de-risk right after the
hello-world path.
Next C3: real backend = clone src/quickjs/ arena-handle shape (JSI Value is NaN-boxed struct like qjs
JSValue). Implement the 9-symbol hello-world path (Isolate/Context/HandleScope/String/Script) in C++
against JSI + expand the extern-C shim, so a v8x smoke test runs a real script THROUGH the hermes backend.

### DELIVERABLE - working QuickJS deno binary (01:21) SHIPPED
Built deno release --no-default-features --features quickjs on v8x 149.4.0-rc.1 (crates.io) in
~/gh/deno-v8x-rebase. 68M. VERIFIED fully working: JS builtins, async+setTimeout, Deno.readTextFile,
TypeScript, Deno.serve+fetch (200), npm import (change-case). Delivered ~/deno-quickjs/{deno,README.md}.

### Cycle - E6 REAL QuickJS heap snapshot (agent: e6) DONE - the 'AOT vs snapshot' answer
FRONTIER RESULT: a real post-bootstrap heap snapshot IS achievable on QuickJS and v8x already has
the machinery. JS_WriteObject + JS_WRITE_OBJ_REFERENCE serializes the whole reachable graph by value
(nested objs, prototypes, frozen bits, Map/Set, typed arrays, BigInt, cycles, Symbol identity all
round-trip). Native C fn pointers (unserializable) are solved by a SYMBOLIC reference-path registry
(__v8x_snapshot_intrinsics + patch quickjs-67-snapshot-native-function-state): writer emits a
property PATH to each native fn, re-resolved on read against the fresh runtime. Patch 67 also
serializes native object state -> a REAL heap snapshot, not just rebinding. Residual blocker: opaque
JS_SetOpaque C-state (sockets/napi) needs per-class hooks.
Hybrid (re-install natives -> refresh registry -> JS_ReadObject pure-JS on top) WORKS: prototype
docs/hermes-spike/experiments/e6-src/e6_snap.c, 16/16 checks incl a native add() callable after
restore carrying heap-added callCount=42.
MEASURED (arm64 standalone C): small graph 17.9KB blob / 0.08ms restore; 20k-obj graph: from-source
reboot 8.4ms vs snapshot restore 10.0ms, blob 1.46MB.
KEY INSIGHT that refines the whole idea: on QuickJS, restore is NOT faster than re-execution (no
mmap-and-fixup fast path like V8; JS_ReadObject re-allocs+re-hashes every node = same order of work
as running the code). So the heap snapshot's value is STATE CAPTURE (side effects, non-determinism,
expensive-to-recompute state), NOT startup latency. Corollary for the user's vision: for STARTUP,
parse-free AOT-bytecode boot is already ~as good as snapshot-restore on QuickJS -> AOT genuinely
'solves' the startup half without needing snapshots. Snapshots only earn their keep for stateful boot.
Next (E7): drive src/quickjs/snapshot.rs capture/replay across a REAL Deno bootstrap; test replacing
the replay-tape, isolating the load-bearing synthetic ext:core/ops module identity.

### Cycle 1 - scaffold engine_hermes, fix link failure (agent: executor) DONE
The prior commit (9d3d86a) already added Cargo.toml/src/lib.rs wiring and
src/hermes/{mod,misc,shims}.rs, but `cargo build --no-default-features
--features hermes` did not actually compile: rustc rejected it with "symbol
`v8__Platform__CustomPlatform__BASE__DROP` is already defined" (a
duplicate-symbol error, not a linker error - it happens during codegen of the
v8x crate itself, since both the generated stub and the real definition live
in the same crate). Root-caused and fixed tools/gen_hermes_shims.sh; after
the fix, `cargo build --no-default-features --features hermes` compiles and
links clean (0 errors, 0 warnings) with zero Hermes dependency. Pure-Rust
stub backend only, nothing runs real JavaScript yet.
- Cargo.toml / src/lib.rs / src/hermes/{mod,misc}.rs: already correct as
  committed. `engine_hermes`, `link_hermes` (unused so far), `hermes =
  ["engine_hermes"]` alias (deliberately does not pull in `link_hermes`);
  `#[cfg(feature="engine_hermes")] mod hermes;` in lib.rs next to the other
  backends, `V8X_ENGINE` returns `"hermes"`; misc.rs has 25 `cppgc__*` stubs
  with small real bodies backed by a raw pointer slot, since the
  Member/Persistent wrapper code in cppgc.rs treats them as plain data, not
  just link placeholders.
- src/hermes/shims.rs: regenerated, now 737 `v8__*`/`v8_inspector__*` stubs
  (down from the prior 764 - the 27 symbols below are excluded).
- tools/gen_hermes_shims.sh: adapted from tools/gen_qjs_shims.sh. Diverges in
  one important way: it sources symbol names directly from
  `vendor/rusty_v8/src/*.rs` extern decls (no test-build union.txt exists yet
  for hermes) and explicitly excludes symbols the vendored crate itself
  DEFINES with `#[unsafe(no_mangle)]` (the engine-independent
  CustomPlatform-task and Value(De)Serializer::Delegate/Inspector
  Channel/Client callback trampolines in platform.rs/value_serializer.rs/
  value_deserializer.rs/inspector.rs). Stubbing those again is a
  duplicate-symbol error at compile time, not a linker warning, since they
  live in the same crate.
- Confirmed empirically: Rust `extern "C"` FFI linking is name-only (no
  signature check across module/file boundaries), so no-arg
  `unimplemented!()` stub bodies link fine against any real declared
  signature. Same technique the QuickJS/JSC generators already rely on.
- No regression: `cargo check --no-default-features --features quickjs` still
  builds clean.
Next: C2, vendor a real Hermes static library and start replacing shims with
real JSI-backed implementations, starting with the 9-symbol hello-world path
noted in the C0 integration-surface log entry.

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
