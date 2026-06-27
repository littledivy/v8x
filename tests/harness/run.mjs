#!/usr/bin/env node
// Run ONE suite x backend, parse results, save a run-result JSON, and
// (optionally) ratchet against the committed baseline.
//
//   node tests/harness/run.mjs <suite> <backend> [flags]
//
// Flags:
//   --check            exit 1 if any baselined test regressed/vanished, or if
//                      there are new passes not yet in the baseline. (CI / PR)
//   --update           rewrite the baseline to exactly the current pass set.
//   --deno-dir=PATH    patched deno checkout (deno_core / deno-test suites).
//   --deno-bin=PATH    built deno binary (deno-test suites).
//   --skip-build       reuse already-built test binaries (cargo suites).
//
// With neither --check nor --update it just runs + reports (informational).
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import {
  ROOT,
  backend,
  die,
  loadBaseline,
  loadConfig,
  parseJUnit,
  parseLibtest,
  ratchet,
  run,
  saveBaseline,
  saveRunResult,
  suite,
} from "./lib.mjs";

const args = process.argv.slice(2);
const pos = args.filter((a) => !a.startsWith("--"));
const flag = (name) => args.includes(`--${name}`);
const opt = (name) => {
  const a = args.find((x) => x.startsWith(`--${name}=`));
  return a ? a.slice(name.length + 3) : undefined;
};

const [suiteId, backendId] = pos;
if (!suiteId || !backendId) die("usage: run.mjs <suite> <backend> [--check|--update]");

const cfg = loadConfig();
const s = suite(cfg, suiteId);
const b = backend(cfg, backendId);
const isMac = os.platform() === "darwin";
const needsCodesign = isMac && b.os === "macos"; // JSC JIT entitlements
const ENT = path.join(ROOT, "tools/jit-entitlements.plist");

let result;
switch (s.kind) {
  case "cargo-self":
    result = runCargoSelf();
    break;
  case "cargo-deno":
    result = runCargoDeno();
    break;
  case "deno-test":
    result = runDenoTest();
    break;
  default:
    die(`unknown suite kind '${s.kind}'`);
}

report(result);

// ---------------------------------------------------------------------------
function codesign(bin) {
  if (!needsCodesign) return;
  run("codesign", ["--force", "--sign", "-", "--entitlements", ENT, bin], { echo: false });
}

// Build cargo test bins (--no-run), return the executable paths. Returns null
// on build failure so the caller can degrade gracefully (a target that won't
// compile/link counts as 0 passing — exactly the thing agents climb).
// selectBackend=true picks our v8 backend via OUR crate's features (cargo-self).
// For the deno checkout (cargo-deno) the v8 backend is chosen by deno's
// [patch.crates-io] v8 features, so we must NOT pass them here.
function cargoBuild(extraArgs, cwd, selectBackend = true) {
  const base = selectBackend
    ? ["test", "--no-default-features", "--features", b.features, "--no-run"]
    : ["test", "--no-run"];
  // The vendored rusty_v8 binding references some C-ABI symbols our shim
  // doesn't define yet (crdtp__* inspector protocol, icu_set_default_locale).
  // macOS ld64 dead-strips the unreferenced ones; Linux lld errors out and the
  // whole test binary fails to link, zeroing the suite. Tell lld to leave
  // unreferenced-undefined symbols as 0 (same net effect as mac) so unrelated
  // tests still link + run; a test that actually calls one just fails. Only for
  // cargo-self (our crate); the deno checkout defines these via deno's crdtp.
  // Force plain output: CI sets CARGO_TERM_COLOR=always, whose ANSI codes
  // (e.g. around "Executable") break the path-parsing regex below.
  const env = { CARGO_TERM_COLOR: "never" };
  if (selectBackend && os.platform() === "linux") {
    const extra = "-C link-arg=-Wl,--unresolved-symbols=ignore-all";
    env.RUSTFLAGS = process.env.RUSTFLAGS ? `${process.env.RUSTFLAGS} ${extra}` : extra;
  }
  const r = run("cargo", [...base, ...extraArgs], { cwd, env });
  if (r.code !== 0) return null;
  const bins = [];
  // Strip any ANSI just in case CARGO_TERM_COLOR is forced upstream.
  const clean = r.out.replace(/\x1b\[[0-9;]*m/g, "");
  for (const line of clean.split("\n")) {
    const m = line.match(/Executable\s+\S+\s+\(([^)]+)\)/);
    if (m) bins.push(path.resolve(cwd, m[1]));
  }
  return bins;
}

function runBins(bins, cwd) {
  let out = "";
  for (const bin of bins) {
    codesign(bin);
    // Print the path so parseLibtest can attribute tests to a binary.
    out += `Running ${bin}\n`;
    const r = run(bin, ["--test-threads", "1", "--color", "never"], { cwd, echo: false });
    out += r.out.replace(/\x1b\[[0-9;]*m/g, "") + "\n";
  }
  return out;
}

function runCargoSelf() {
  ensureIcuData(); // vendored rusty_v8 tests include_bytes! the ICU blob
  // Build each [[test]] target on its own so a single unbuildable/unlinkable
  // target (missing v8__* symbol) doesn't zero the whole suite. The others
  // still run; the broken one simply contributes 0 passing.
  let out = "";
  const unbuildable = [];
  for (const t of s.cargo_tests) {
    const bins = flag("skip-build")
      ? discoverBins(path.join(ROOT, "target/debug/deps"), [t])
      : cargoBuild(["--test", t], ROOT);
    if (!bins || !bins.length) {
      unbuildable.push(t);
      console.error(`  (target ${t} did not build — counts as 0 passing)`);
      continue;
    }
    out += runBins(bins, ROOT);
  }
  if (unbuildable.length) console.error(`unbuildable targets: ${unbuildable.join(", ")}`);
  return finalize(parseLibtest(out));
}

function runCargoDeno() {
  const denoDir = reqDenoDir();
  const bins = cargoBuild(["-p", s.package], denoDir, false);
  if (!bins) {
    console.error(`deno_core test build failed — counts as 0 passing`);
    return finalize({ pass: new Set(), fail: new Set(), skip: new Set() });
  }
  // Run from the crate dir so cwd-relative fixtures resolve.
  const out = runBins(bins, path.join(denoDir, "libs/core"));
  return finalize(parseLibtest(out));
}

// The vendored rusty_v8 test files `include_bytes!` an ICU data blob that the
// stock rusty_v8 build downloads. We don't ship it; an empty placeholder lets
// the files COMPILE. The remaining blocker (v8::icu::set_common_data_77 ->
// udata_setCommonData_77) is a backend symbol for agents to implement; until
// then those targets fail to link and score 0 (handled above).
function ensureIcuData() {
  const p = path.join(ROOT, "vendor/rusty_v8/third_party/icu/common/icudtl.dat");
  if (!fs.existsSync(p)) {
    fs.mkdirSync(path.dirname(p), { recursive: true });
    fs.writeFileSync(p, "");
  }
}

function runDenoTest() {
  const denoDir = reqDenoDir();
  const denoBin = opt("deno-bin") || die("--deno-bin=PATH required for deno-test suites");
  const junit = path.join(os.tmpdir(), `v82jsc-${backendId}-${suiteId}.junit.xml`);
  const flags = (s.deno_flags || ["-A", "--no-check"]).slice();
  run(denoBin, ["test", ...flags, `--junit-path=${junit}`, s.test_path], { cwd: denoDir });
  if (!fs.existsSync(junit)) die(`deno produced no junit at ${junit}`);
  return finalize(parseJUnit(fs.readFileSync(junit, "utf8")));
}

function discoverBins(depsDir, names) {
  // Newest matching binary per test name (fallback for --skip-build).
  const out = [];
  for (const n of names) {
    const cands = fs
      .readdirSync(depsDir)
      .filter((f) => f.startsWith(`${n}-`) && !f.includes("."))
      .map((f) => path.join(depsDir, f))
      .sort((a, c) => fs.statSync(c).mtimeMs - fs.statSync(a).mtimeMs);
    if (cands[0]) out.push(cands[0]);
  }
  return out;
}

function reqDenoDir() {
  return opt("deno-dir") || die("--deno-dir=PATH required for this suite");
}

// Resolve the engine's version string for the dashboard label. Runs inside the
// build job (submodules present, correct OS), so aggregate.mjs — which checks
// out WITHOUT submodules — can read it back from the run-result instead.
//   vendor_jsc  -> WebKit FULL_VERSION + short submodule sha, e.g. "625.1.23 (0f307e9)"
//   system_jsc  -> framework CFBundleVersion, e.g. "20621.3.11.11.3"
//   quickjs     -> QJS_VERSION_* from quickjs.h, e.g. "0.15.1"
function engineVersion() {
  try {
    if (b.features.includes("vendor_jsc")) {
      const xc = fs.readFileSync(path.join(ROOT, "vendor/webkit/Configurations/Version.xcconfig"), "utf8");
      const g = (k) => (xc.match(new RegExp(`${k}\\s*=\\s*(\\d+)`)) || [])[1];
      const full = ["MAJOR_VERSION", "MINOR_VERSION", "TINY_VERSION"].map(g).filter(Boolean).join(".");
      const sha = run("git", ["rev-parse", "--short=7", "HEAD:vendor/webkit"], { echo: false });
      const s = sha.code === 0 ? sha.out.trim() : "";
      return (s ? `${full} (${s})` : full) || null;
    }
    if (b.features.includes("system_jsc")) {
      const r = run("defaults", ["read",
        "/System/Library/Frameworks/JavaScriptCore.framework/Resources/Info.plist",
        "CFBundleVersion"], { echo: false });
      return r.code === 0 ? r.out.trim() || null : null;
    }
    if (b.features.includes("quickjs")) {
      const h = fs.readFileSync(path.join(ROOT, "vendor/quickjs-ng/quickjs.h"), "utf8");
      const g = (k) => (h.match(new RegExp(`#define\\s+QJS_VERSION_${k}\\s+(\\S+)`)) || [])[1];
      const maj = g("MAJOR");
      if (maj == null) return null;
      const suf = (g("SUFFIX") || "").replace(/"/g, "");
      return `${maj}.${g("MINOR")}.${g("PATCH")}${suf}`;
    }
  } catch {}
  return null;
}

function finalize(parsed) {
  return {
    backend: backendId,
    suite: suiteId,
    version: engineVersion(),
    pass: parsed.pass.size,
    fail: parsed.fail.size,
    skip: parsed.skip.size,
    total: parsed.pass.size + parsed.fail.size,
    passing: [...parsed.pass].sort(),
    failing: [...parsed.fail].sort(),
    _sets: parsed, // not serialized
  };
}

function report(res) {
  const { _sets, ...clean } = res;
  saveRunResult(backendId, suiteId, clean);
  console.error(
    `\n[${backendId}/${suiteId}] pass=${res.pass} fail=${res.fail} skip=${res.skip} (total ${res.total})`,
  );

  const baseline = loadBaseline(backendId, suiteId);

  if (flag("update")) {
    const n = saveBaseline(backendId, suiteId, _sets.pass);
    console.error(`updated baseline: ${n} passing tests`);
    return;
  }

  const r = ratchet(baseline, _sets);
  if (r.regressions.length)
    console.error(`\nREGRESSIONS (${r.regressions.length}):\n  ${r.regressions.join("\n  ")}`);
  if (r.missing.length)
    console.error(`\nMISSING — baselined but not seen (${r.missing.length}):\n  ${r.missing.join("\n  ")}`);
  if (r.newPasses.length)
    console.error(`\nNEW PASSES — add via --update (${r.newPasses.length}):\n  ${r.newPasses.join("\n  ")}`);

  if (flag("check")) {
    if (!r.ok) {
      console.error(`\nFAIL: ratchet regression on ${backendId}/${suiteId}.`);
      process.exit(1);
    }
    // Unseeded cell (empty baseline): bootstrapping — don't block on new
    // passes; the main-branch run seeds it via aggregate --write-baselines.
    if (r.newPasses.length && baseline.size > 0) {
      console.error(`\nFAIL: ${r.newPasses.length} new passing test(s) not in baseline. Run: --update`);
      process.exit(1);
    }
    if (r.newPasses.length) console.error(`\n(unseeded cell — ${r.newPasses.length} passes will seed on main)`);
    console.error(`\nOK: ratchet holds (${baseline.size} baselined).`);
  }
}
