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
  saveBaseline,
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

// On main (single writer) we fold each fresh run's pass set back into the
// committed baseline. PRs never do this — their ratchet (run.mjs --check)
// compares against the committed baseline, so regressions are blocked there.
if (args.includes("--write-baselines")) {
  for (const [key, r] of Object.entries(runResults)) {
    const n = saveBaseline(r.backend, r.suite, new Set(r.passing));
    console.error(`baseline ${key}: ${n} passing`);
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
    const total = run ? run.total : null;
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
  backends: cfg.backends.map((b) => ({ id: b.id, label: b.label })),
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
