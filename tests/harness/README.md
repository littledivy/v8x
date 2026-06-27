# Test hill-climb harness

Tracks how many tests pass per **backend × suite** and ratchets the pass set so
progress can't silently regress. See the root [`CLAUDE.md`](../../CLAUDE.md) for
the agent workflow. This file documents the harness internals.

## Files

| path | role |
|---|---|
| `config.json` | single source of truth: backends + suites |
| `run.mjs` | run ONE suite×backend, parse, ratchet (`--check`/`--update`) |
| `aggregate.mjs` | build `status/report.json` + `status/history.jsonl` (CI only) |
| `lib.mjs` | shared: parsers (libtest + JUnit), baselines, ratchet diff |

## Data layout (`status/`)

```
status/
  report.json                       # CI-generated aggregate (dashboard source)
  history.jsonl                     # CI-appended snapshots (time series)
  baselines/<backend>/<suite>.txt   # ratchet: known-passing test names
  .runs/<backend>__<suite>.json     # ephemeral per-run result (gitignored)
```

## run.mjs

```
node tests/harness/run.mjs <suite> <backend> [flags]
  --check       exit 1 on regression / vanished test / un-baselined new pass
  --update      rewrite the baseline to the current pass set
  --deno-dir=P  patched deno checkout (deno_core + deno-test suites)
  --deno-bin=P  built deno binary (deno-test suites)
  --skip-build  reuse already-built cargo test binaries
```

Suite kinds (`config.json`):
- `cargo-self` (rusty_v8): each `[[test]]` target built **separately** so one
  unlinkable target scores 0 without zeroing the rest. Parsed from libtest text.
- `cargo-deno` (deno_core): `cargo test -p deno_core` in the deno checkout; the
  v8 backend comes from deno's `[patch.crates-io] v8` features (not our flags).
- `deno-test` (node_compat, unit): the built deno binary's `deno test
  --junit-path`, parsed from JUnit XML.

## Ratchet semantics

`ratchet(baseline, result)` →
- **regression**: baselined test now FAILED → hard fail.
- **missing**: baselined test not seen (renamed/removed) → hard fail.
- **newPasses**: passing now, not baselined → `--check` fails (run `--update`).
  Exception: an **empty** baseline = unseeded cell (bootstrapping); `--check`
  allows new passes there. The first main run seeds it via `--write-baselines`.

## CI flow

- PR: each matrix job runs `--check` per cell. Red blocks merge.
- main push: jobs record run-result artifacts; the single `report` job folds
  them into baselines (`aggregate.mjs --write-baselines`), regenerates
  `report.json` + `history.jsonl`, commits, pushes. `pages.yml` redeploys the
  dashboard. Single writer ⇒ parallel PRs never conflict on these files.
