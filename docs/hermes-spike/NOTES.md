# Hermes backend spike (overnight autonomous run)

Goal: prototype a static **Hermes** backend for v8x (`engine_hermes`) that can run
Deno, and test whether AOT-compiling JS (Hermes bytecode / static-hermes /
Porffor) can replace the V8 startup snapshot.

Branch: `hermes-backend-spike`. Loop state in `.omc/hermes-loop/`.

## Status board (updated each cycle)
- [ ] C0 research: Hermes embedding API + AOT capabilities
- [x] C0 research: v8x integration surface (how to add engine_hermes)
- [ ] C0 research: AOT-solves-snapshot feasibility (HBC / static-hermes / Porffor)
- [ ] C1 scaffold: engine_hermes feature + src/hermes/ skeleton + build.rs
- [ ] C2 build: vendor + build static Hermes, link stubs
- [ ] C3 implement: core v8__* (isolate/context/primitives/strings)
- [ ] C4 test: rusty_v8 harness on hermes, hill-climb
- [ ] C5 AOT experiment: precompile demo JS, boot without parser

## Cycle log
(newest first)

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
