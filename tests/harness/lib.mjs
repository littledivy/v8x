// Shared helpers for the v82jsc test-suite hill-climb harness.
// Plain Node ESM, no deps. Used by run.mjs and aggregate.mjs.
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

export const ROOT = path.resolve(fileURLToPath(import.meta.url), "../../..");
export const STATUS_DIR = path.join(ROOT, "tests/status");
export const BASELINE_DIR = path.join(STATUS_DIR, "baselines");
export const MANIFEST_DIR = path.join(STATUS_DIR, "manifest");
export const RUNS_DIR = path.join(STATUS_DIR, ".runs");

export function loadConfig() {
  const p = path.join(ROOT, "tests/harness/config.json");
  return JSON.parse(fs.readFileSync(p, "utf8"));
}

export function backend(cfg, id) {
  const b = cfg.backends.find((x) => x.id === id);
  if (!b) die(`unknown backend '${id}' (have: ${cfg.backends.map((x) => x.id).join(", ")})`);
  return b;
}

export function suite(cfg, id) {
  const s = cfg.suites.find((x) => x.id === id);
  if (!s) die(`unknown suite '${id}' (have: ${cfg.suites.map((x) => x.id).join(", ")})`);
  return s;
}

export function die(msg) {
  console.error(`error: ${msg}`);
  process.exit(2);
}

// --- baseline files: one passing-test name per line, sorted, unique ---------
export function baselinePath(backendId, suiteId) {
  return path.join(BASELINE_DIR, backendId, `${suiteId}.txt`);
}

export function loadBaseline(backendId, suiteId) {
  const p = baselinePath(backendId, suiteId);
  if (!fs.existsSync(p)) return new Set();
  return new Set(
    fs
      .readFileSync(p, "utf8")
      .split("\n")
      .map((l) => l.trim())
      .filter((l) => l && !l.startsWith("#")),
  );
}

export function saveBaseline(backendId, suiteId, names) {
  const p = baselinePath(backendId, suiteId);
  fs.mkdirSync(path.dirname(p), { recursive: true });
  const sorted = [...names].sort();
  const header =
    `# Baseline: tests that PASS for backend=${backendId} suite=${suiteId}.\n` +
    `# Ratchet — CI goes red if any of these regress. Add new passes here\n` +
    `# (run.mjs --update). ${sorted.length} passing. DO NOT hand-sort; the\n` +
    `# harness keeps this sorted + unique.\n`;
  fs.writeFileSync(p, header + sorted.join("\n") + (sorted.length ? "\n" : ""));
  return sorted.length;
}

// --- manifest: the FULL known test set for a suite, shared across backends ---
// One file per SUITE (not per backend): the same deno_core / rusty_v8 tests run
// against every v8 backend, so the denominator is backend-independent. It is the
// monotonic union of every test name ever ENUMERATED (passing ∪ failing) on ANY
// backend — so it never shrinks, and a backend that under-discovers (e.g. a test
// binary that won't link) is measured against the full set (95/303), not its own
// truncated total (95/95). This is the fixed "all tests" denominator agents
// hill-climb toward.
export function manifestPath(suiteId) {
  return path.join(MANIFEST_DIR, `${suiteId}.txt`);
}

export function loadManifest(suiteId) {
  const p = manifestPath(suiteId);
  if (!fs.existsSync(p)) return new Set();
  return new Set(
    fs
      .readFileSync(p, "utf8")
      .split("\n")
      .map((l) => l.trim())
      .filter((l) => l && !l.startsWith("#")),
  );
}

export function saveManifest(suiteId, names) {
  const p = manifestPath(suiteId);
  fs.mkdirSync(path.dirname(p), { recursive: true });
  const sorted = [...names].sort();
  const header =
    `# Manifest: the FULL known test set for suite=${suiteId} (shared across\n` +
    `# backends). The fixed denominator — pass/total is measured against THIS,\n` +
    `# so under-discovery shows as e.g. 95/303 red, not 95/95 green. Monotonic:\n` +
    `# only grows (union of every enumerated test), never shrinks. ${sorted.length} tests.\n`;
  fs.writeFileSync(p, header + sorted.join("\n") + (sorted.length ? "\n" : ""));
  return sorted.length;
}

// --- run results: ephemeral per (backend,suite) JSON under status/.runs -----
export function runResultPath(backendId, suiteId) {
  return path.join(RUNS_DIR, `${backendId}__${suiteId}.json`);
}

export function saveRunResult(backendId, suiteId, result) {
  fs.mkdirSync(RUNS_DIR, { recursive: true });
  fs.writeFileSync(runResultPath(backendId, suiteId), JSON.stringify(result, null, 2));
}

// --- command runner ---------------------------------------------------------
export function run(cmd, args, opts = {}) {
  console.error(`\$ ${cmd} ${args.join(" ")}${opts.cwd ? `  (cwd: ${opts.cwd})` : ""}`);
  const r = spawnSync(cmd, args, {
    cwd: opts.cwd || ROOT,
    encoding: "utf8",
    maxBuffer: 256 * 1024 * 1024,
    env: { ...process.env, ...(opts.env || {}) },
  });
  const out = (r.stdout || "") + (r.stderr || "");
  if (opts.echo !== false) process.stderr.write(out);
  return { code: r.status ?? 1, out };
}

// --- libtest (cargo) output parser ------------------------------------------
// Handles many test binaries in one stream; prefixes each test with its binary
// name so identical test fn names across binaries don't collide.
function normalizeLibtestName(name) {
  return name.replace(/ - should panic$/, "");
}

export function parseLibtest(output) {
  const pass = new Set();
  const fail = new Set();
  const skip = new Set();
  let bin = "";
  for (const line of output.split("\n")) {
    const running = line.match(/^Running (?:unittests |tests )?(\S+)/);
    if (running) {
      bin = path
        .basename(running[1])
        .replace(/-[0-9a-f]+(?:\.exe)?$/, "");
      continue;
    }
    const m = line.match(/^test (.+?) \.\.\. (ok|FAILED|ignored)\b/);
    if (!m) continue;
    const testName = normalizeLibtestName(m[1]);
    const name = bin ? `${bin}::${testName}` : testName;
    if (m[2] === "ok") {
      pass.add(name);
      // A later solo-rerun "ok" (run.mjs --rescue) supersedes an earlier
      // FAILED from the same binary's poisoned batch run.
      fail.delete(name);
    } else if (m[2] === "FAILED") {
      if (!pass.has(name)) fail.add(name);
    } else skip.add(name);
  }
  return { pass, fail, skip };
}

// --- nextest JSON parser ----------------------------------------------------
// `cargo nextest run --message-format libtest-json` emits newline-delimited JSON
// records for test events. Unit-test names are shaped as
// `<package>::<binary>$<test path>`; normalize them to the libtest baseline form
// `<binary>::<test path>` so switching deno_core to nextest doesn't rename the
// whole baseline.
function normalizeNextestName(name) {
  const clean = normalizeLibtestName(name);
  const dollar = clean.indexOf("$");
  if (dollar === -1) return clean;
  const left = clean.slice(0, dollar);
  const testPath = clean.slice(dollar + 1);
  const binary = left.split("::").pop();
  return binary ? `${binary}::${testPath}` : testPath;
}

export function parseNextestJson(output) {
  const pass = new Set();
  const fail = new Set();
  const skip = new Set();
  const failOutput = new Map();
  for (const line of output.split("\n")) {
    if (!line.startsWith("{")) continue;
    let event;
    try {
      event = JSON.parse(line);
    } catch {
      continue;
    }
    if (event.type !== "test" || typeof event.name !== "string") continue;
    const name = normalizeNextestName(event.name);
    if (event.event === "ok") {
      pass.add(name);
      fail.delete(name);
    } else if (event.event === "failed" || event.event === "timeout") {
      if (!pass.has(name)) fail.add(name);
      if (typeof event.stdout === "string" && event.stdout.length)
        failOutput.set(name, event.stdout);
    } else if (event.event === "ignored") {
      skip.add(name);
    }
  }
  return { pass, fail, skip, failOutput };
}

// --- JUnit XML parser (deno test --junit-path) ------------------------------
// Minimal: enough for deno's output. classname+name identify a case; a nested
// <failure>/<error> (or skipped) marks the result.
export function parseJUnit(xml) {
  const pass = new Set();
  const fail = new Set();
  const skip = new Set();
  const caseRe = /<testcase\b([^>]*?)(\/>|>([\s\S]*?)<\/testcase>)/g;
  let m;
  while ((m = caseRe.exec(xml))) {
    const attrs = m[1];
    const body = m[3] || "";
    const cls = (attrs.match(/classname="([^"]*)"/) || [, ""])[1];
    const nm = (attrs.match(/\bname="([^"]*)"/) || [, ""])[1];
    const name = unescapeXml(cls ? `${cls} > ${nm}` : nm);
    if (/<failure\b|<error\b/.test(body)) fail.add(name);
    else if (/<skipped\b/.test(body)) skip.add(name);
    else pass.add(name);
  }
  return { pass, fail, skip };
}

function unescapeXml(s) {
  return s
    .replaceAll("&lt;", "<")
    .replaceAll("&gt;", ">")
    .replaceAll("&quot;", '"')
    .replaceAll("&apos;", "'")
    .replaceAll("&amp;", "&");
}

// --- ratchet diff -----------------------------------------------------------
// baseline: Set of expected-pass names. result: {pass,fail,skip} Sets.
export function ratchet(baseline, result) {
  const regressions = []; // expected pass, now failing
  const missing = []; // expected pass, not seen at all (renamed/removed)
  const newPasses = []; // passing now, not in baseline
  for (const n of baseline) {
    if (result.fail.has(n)) regressions.push(n);
    else if (!result.pass.has(n)) missing.push(n);
  }
  for (const n of result.pass) if (!baseline.has(n)) newPasses.push(n);
  return {
    regressions: regressions.sort(),
    missing: missing.sort(),
    newPasses: newPasses.sort(),
    ok: regressions.length === 0 && missing.length === 0,
  };
}
