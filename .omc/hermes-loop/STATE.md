# Hermes spike — loop state

Branch: hermes-backend-spike. Progress board + cycle log: docs/hermes-spike/NOTES.md

## Loop protocol (read this every wake-up)
1. Read docs/hermes-spike/NOTES.md (status board + cycle log) and this file.
2. If research/impl agents are still running, wait for them (they notify on completion).
3. When agent results arrive: synthesize into NOTES.md cycle log, update the status
   board checkboxes, `git add -A && git commit` the progress.
4. Advance to the next cycle (scaffold -> build -> implement -> test -> AOT experiment).
   Spawn subagents for heavy work (research=general-purpose, code=executor with model=opus
   for hard parts). Keep MY context lean: agents return conclusions, not file dumps.
5. Push the branch to origin periodically so the user can review.
6. Re-arm a fallback ScheduleWakeup (~1800s) each cycle. Keep looping until ~07:00 local.

## Guardrails
- Local spike branch ONLY. Never touch main, never open/modify PRs, never publish.
- Commit often with clear messages. No fake "done" — record blockers honestly.
- Style: no em dashes in committed docs (repo rule).

## Current cycle: C0 (research) — 3 agents launched
- hermes-embed-aot (Hermes API + AOT feasibility)
- v8x-integration (how to add engine_hermes)
- aot-snapshot (does AOT solve snapshot; Porffor)
Next after C0: decide GO/NO-GO, then C1 scaffold engine_hermes.

## DIRECTIVE (user, overnight - override cautious defaults)
EXPLORE FRONTIERS. TRULY INNOVATE. Do NOT just accept limitations or stop at "hard/cautious".
AOT can make this WONDERFUL - lean hard into it. The vision: a tiny, instant-start, AOT-baked
JS runtime that runs Deno (mobile + native). Bias to BUILD + MEASURE real prototypes and run
experiments end-to-end with real numbers, NOT more analysis docs. When research says "limitation",
find the creative path around it and TEST it. Autonomous until morning; user is asleep, no questions.

## FRONTIER EXPERIMENT BACKLOG (ambitious; pursue as cycles allow)
- E5  QuickJS bytecode-boot vs source-boot timing (baseline; parse-cost recovered by AOT).
- E6  REAL QuickJS heap snapshot: can JS_WriteObject/JS_ReadObject (quickjs-ng) serialize the
      POST-bootstrap global object graph, not just bytecode? If yes, that is a true snapshot
      cross-engine and dissolves the "AOT != heap state" limitation. INVESTIGATE + PROTOTYPE.
- E7  Static Hermes (shermes) native AOT on a real bootstrap-shaped JS chunk. Push past "untyped =
      no win": actually compile it, measure, see where it breaks, try minimal typing. Real data.
- E8  "AOT-native Deno" north star: smallest end-to-end path to a tiny native binary that runs a
      trivial Deno program with the runtime AOT-baked in. Prototype the smallest slice that works.
- E9  Hybrid: AOT-compile bootstrap to bytecode (parse-free) + embed build-time-precomputed CONSTANT
      data structures as raw data (partial heap capture of the static parts), AOT-run only the
      dynamic init. Measure vs full snapshot. Novel middle path.
Rule: prefer a working prototype + measured number over a paragraph. Record failures honestly too.

## [DONE 01:21] TOP DELIVERABLE: working QuickJS Deno binary
Built deno release --no-default-features --features quickjs (v8x 149.4.0-rc.1 from crates.io) in
~/gh/deno-v8x-rebase (branch v8x-rebase-rc). 68M binary. VERIFIED fully working, fails=0:
JS builtins, async+setTimeout, Deno.readTextFile, TypeScript, Deno.serve+fetch (200 OK), npm import
(change-case downloaded+ran). Delivered to ~/deno-quickjs/deno + README.md. NO further work needed.
(This is the mature QuickJS backend; Hermes backend remains the spike.)

## NEW NORTH STAR (user, morning): BOOT DENO ON THE HERMES BACKEND
User called it: overnight built the Hermes BACKEND (77/267 rusty_v8, runs JS) but NOT deno-on-hermes,
which was the real goal. Directive: GRIND toward Deno boot. Realistic multi-session effort.
Path: D0 recon (map deno_core boot's v8__* needs + set a measurable minimal-boot target) ->
D1 Promises + microtask queue -> D2 ES modules -> D3 ops/External infra + snapshot-or-bootstrap ->
D4 deno_core hill-climb -> D5 actually boot a deno_core JsRuntime running a script on Hermes.
Measure: get deno_core JsRuntime::new + eval "1+1" working on the hermes backend; then the deno_core suite.
Honest: Hermes v0.11.0 is interpreter-only (no JIT) - not a speed win vs QuickJS; value is AOT/HBC + mobile.
Same rules: commit+push each step, subagents for heavy work, local branch only, no fake wins.
