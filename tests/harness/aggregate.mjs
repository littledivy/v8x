#!/usr/bin/env node
// Build the single status/report.json (dashboard source of truth) and append a
// snapshot to status/history.jsonl, from the committed baselines + whatever
// fresh run-results exist under status/.runs/.
//
//   node tests/harness/aggregate.mjs [--commit=SHA] [--no-history]
//
// CI is the only writer of report.json / history.jsonl. Agents only ever touch
// baseline files, so their PRs never conflict on the aggregate.
import fs from "node:fs";
import path from "node:path";
import {
  RUNS_DIR,
  STATUS_DIR,
  loadBaseline,
  loadConfig,
  loadManifest,
  saveBaseline,
  saveManifest,
} from "./lib.mjs";

const args = process.argv.slice(2);
const opt = (n) => {
  const a = args.find((x) => x.startsWith(`--${n}=`));
  return a ? a.slice(n.length + 3) : undefined;
};
const commit = opt("commit") || process.env.GITHUB_SHA || "local";
const ts = new Date().toISOString();

const cfg = loadConfig();
const runResults = loadRuns();

// Carry-forward the engine version from the previous report when this run has no
// fresh run-result for a backend (e.g. its build job was slow/absent this time).
// Keeps the dashboard label stable instead of dropping the version + sha.
const prevVersions = {};
try {
  const prev = JSON.parse(fs.readFileSync(path.join(STATUS_DIR, "report.json"), "utf8"));
  for (const b of prev.backends || []) if (b.version) prevVersions[b.id] = b.version;
} catch {}

// On main (single writer) we fold each fresh run's pass set back into the
// committed baseline. PRs never do this — their ratchet (run.mjs --check)
// compares against the committed baseline, so regressions are blocked there.
if (args.includes("--write-baselines")) {
  for (const [key, r] of Object.entries(runResults)) {
    // MONOTONIC: union with the committed baseline — the floor only ever rises,
    // never falls. A flaky / truncated / crashed run (e.g. a non-deterministic
    // JSC crash that cuts the suite short) can only ADD newly-passing tests; it
    // can never lower the baseline. This stops such a run from silently
    // overwriting e.g. 256 -> 95 and masking a coverage regression from CI +
    // auto-merge. Self-healing: once the crash is fixed, the next clean run
    // unions the full set back in and the baseline is restored.
    const prev = loadBaseline(r.backend, r.suite);
    const merged = new Set([...prev, ...r.passing]);
    const n = saveBaseline(r.backend, r.suite, merged);
    console.error(`baseline ${key}: ${n} passing (monotonic, +${n - prev.size})`);
  }
  // MANIFEST (per suite): the fixed "all tests" denominator. Union every test
  // name ENUMERATED this round (passing ∪ failing) across ALL backends into the
  // suite manifest — monotonic, never shrinks. The backend that discovers the
  // most (e.g. jsc enumerating all 303 deno_core tests) seeds the full set, so a
  // backend that under-discovers (quickjs linking only 95) is scored 95/303, not
  // 95/95. Done on main only (single writer) alongside baselines.
  const seenBySuite = {};
  for (const r of Object.values(runResults)) {
    const set = seenBySuite[r.suite] || (seenBySuite[r.suite] = new Set());
    for (const t of r.passing || []) set.add(t);
    for (const t of r.failing || []) set.add(t);
  }
  for (const [suiteId, seen] of Object.entries(seenBySuite)) {
    const prev = loadManifest(suiteId);
    const m = saveManifest(suiteId, new Set([...prev, ...seen]));
    console.error(`manifest ${suiteId}: ${m} tests (+${m - prev.size})`);
  }
}

const cells = {};
let totalPass = 0;
let totalKnown = 0;
for (const b of cfg.backends) {
  cells[b.id] = {};
  for (const s of cfg.suites) {
    const baseline = loadBaseline(b.id, s.id).size;
    const run = runResults[`${b.id}__${s.id}`];
    // pass: live run if present, else the committed baseline floor.
    const pass = run ? run.pass : baseline;
    // total: the FIXED per-suite manifest (all known tests, shared across
    // backends, monotonic) — never the run's own enumeration. So a cell reads
    // pass/ALL (e.g. quickjs deno_core 95/303), exposing under-discovery instead
    // of showing a truncated 95/95 "green". Fall back to run.total only before
    // the manifest has been seeded.
    const manifestTotal = loadManifest(s.id).size;
    const total = manifestTotal || (run ? run.total : null);
    cells[b.id][s.id] = {
      pass,
      total,
      baseline,
      fail: run ? run.fail : null,
      tier: s.tier,
      fresh: !!run,
    };
    totalPass += pass;
    if (total != null) totalKnown += total;
  }
}

const report = {
  generated: ts,
  commit,
  backends: cfg.backends.map((b) => {
    // run.mjs records the engine version into each run-result (it has the
    // submodules + right OS; this aggregator's checkout does not). Append it to
    // the base label, e.g. "JavaScriptCore" -> "JavaScriptCore 625.1.23 (0f307e9)".
    // Fall back to the previous report's version if no fresh run carried one.
    const v = cfg.suites.map((s) => runResults[`${b.id}__${s.id}`]?.version).find(Boolean)
      || prevVersions[b.id] || null;
    return { id: b.id, label: v ? `${b.label} ${v}` : b.label, version: v };
  }),
  suites: cfg.suites.map((s) => ({ id: s.id, label: s.label, tier: s.tier })),
  cells,
  totals: { pass: totalPass, known: totalKnown },
};

fs.mkdirSync(STATUS_DIR, { recursive: true });
fs.writeFileSync(path.join(STATUS_DIR, "report.json"), JSON.stringify(report, null, 2) + "\n");
console.error(`wrote status/report.json — ${totalPass} passing across the matrix`);

if (!args.includes("--no-history")) {
  // Compact per-snapshot line: { ts, commit, pass: { "backend/suite": n } }
  const flat = {};
  for (const b of cfg.backends)
    for (const s of cfg.suites) flat[`${b.id}/${s.id}`] = cells[b.id][s.id].pass;
  const line = JSON.stringify({ ts, commit, totalPass, pass: flat });
  fs.appendFileSync(path.join(STATUS_DIR, "history.jsonl"), line + "\n");
  console.error("appended snapshot to status/history.jsonl");
}

function loadRuns() {
  const out = {};
  if (!fs.existsSync(RUNS_DIR)) return out;
  for (const f of fs.readdirSync(RUNS_DIR)) {
    if (!f.endsWith(".json")) continue;
    try {
      const r = JSON.parse(fs.readFileSync(path.join(RUNS_DIR, f), "utf8"));
      out[`${r.backend}__${r.suite}`] = r;
    } catch {}
  }
  return out;
}
