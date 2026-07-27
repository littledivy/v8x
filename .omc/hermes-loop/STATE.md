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
