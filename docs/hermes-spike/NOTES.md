# Hermes backend spike (overnight autonomous run)

Goal: prototype a static **Hermes** backend for v8x (`engine_hermes`) that can run
Deno, and test whether AOT-compiling JS (Hermes bytecode / static-hermes /
Porffor) can replace the V8 startup snapshot.

Branch: `hermes-backend-spike`. Loop state in `.omc/hermes-loop/`.

## Status board (updated each cycle)
- [ ] C0 research: Hermes embedding API + AOT capabilities
- [ ] C0 research: v8x integration surface (how to add engine_hermes)
- [ ] C0 research: AOT-solves-snapshot feasibility (HBC / static-hermes / Porffor)
- [ ] C1 scaffold: engine_hermes feature + src/hermes/ skeleton + build.rs
- [ ] C2 build: vendor + build static Hermes, link stubs
- [ ] C3 implement: core v8__* (isolate/context/primitives/strings)
- [ ] C4 test: rusty_v8 harness on hermes, hill-climb
- [ ] C5 AOT experiment: precompile demo JS, boot without parser

## Cycle log
(newest first)
