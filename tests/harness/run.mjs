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
//   --skip-build       reuse already-built test binaries (cargo-self suites).
//
// With neither --check nor --update it just runs + reports (informational).
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawn } from "node:child_process";
import {
  ROOT,
  backend,
  die,
  loadBaseline,
  loadConfig,
  parseJUnit,
  parseLibtest,
  parseNextestJson,
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

// Known-flaky tests to quarantine for this suite/backend: bare libtest paths
// (no `<bin>::` prefix). They are skipped up-front so they never run (a hang
// would block the binary; a crash would truncate it and make later tests'
// outcomes order-dependent) AND excluded from pass/fail/baseline below, so the
// cell is reproducible. From config `ignore` (all backends) + per-backend
// `ignore_by_backend`.
const IGNORE = new Set([
  ...(s.ignore || []),
  ...((s.ignore_by_backend || {})[backendId] || []),
]);
const isMac = os.platform() === "darwin";
const needsCodesign = isMac && b.os === "macos"; // JSC JIT entitlements
const ENT = path.join(ROOT, "tools/jit-entitlements.plist");

// Per-test wall-clock timeout. libtest has NO per-test timeout, and we run with
// --test-threads 1, so a single hanging test (e.g. a deadlocked C-API bridge on
// the vendored-JSC deno_core module loader) blocks the entire binary forever.
// Under CI that means the job hits its job-level timeout-minutes and is
// cancelled — no artifact, the whole cell scores 0 (denoland/divybot#651).
// Override with LIBTEST_TEST_TIMEOUT_SECS; 0 disables the watchdog entirely.
// NOTE: must be declared before the top-level `await` switch below — the suite
// runners reference it during that synchronous-from-here evaluation, so a
// `const` placed lower in the file would hit the temporal dead zone.
const PER_TEST_TIMEOUT_MS =
  (process.env.LIBTEST_TEST_TIMEOUT_SECS != null
    ? Number(process.env.LIBTEST_TEST_TIMEOUT_SECS)
    : 120) * 1000;

let result;
switch (s.kind) {
  case "cargo-self":
    result = await runCargoSelf();
    break;
  case "cargo-deno":
    result = await runCargoDeno();
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
    // cargo prints either `Executable tests/rv8_x.rs (path)` (rusty_v8 integration
    // tests, one token) or `Executable unittests lib.rs (path)` (a crate's unit
    // tests, TWO tokens — e.g. deno_core). Match the trailing (path) regardless
    // of how many tokens precede it, else the unittests binary is never found and
    // the whole suite silently scores 0/0.
    const m = line.match(/^\s*Executable\s+.*?\(([^)]+)\)\s*$/);
    if (m) bins.push(path.resolve(cwd, m[1]));
  }
  return bins;
}

// Run a single libtest binary once, streaming its output behind a watchdog.
// With --test-threads 1 libtest's pretty formatter prints `test <name> ... `
// (flushed) BEFORE running each test and appends `ok`/`FAILED`/`ignored` after.
// So a test that misbehaves leaves that start line DANGLING (no result), in two
// cases we must recover from:
//   * HANG — the test deadlocks and emits nothing more. If no output arrives for
//     PER_TEST_TIMEOUT_MS we SIGKILL the process (killed=true).
//   * CRASH — the test aborts the whole process (SIGABRT/SIGSEGV), which on
//     `--test-threads 1` truncates every test after it (libtest prints no
//     summary, exits via signal / non-success). The dangling start line still
//     identifies the offender.
// Either way the dangling `test <name> ... ` line pins the offending test.
// Returns the captured output, whether the watchdog killed it, the exit
// code/signal, and the in-flight test name (null on a clean finish).
function streamWithWatchdog(bin, args, cwd) {
  return new Promise((resolve) => {
    console.error(`\$ ${bin} ${args.join(" ")}  (cwd: ${cwd})`);
    const child = spawn(bin, args, { cwd, env: { ...process.env } });
    let output = "";
    let killed = false;
    let timer = null;
    const arm = () => {
      if (!(PER_TEST_TIMEOUT_MS > 0)) return;
      clearTimeout(timer);
      timer = setTimeout(() => {
        killed = true;
        child.kill("SIGKILL");
      }, PER_TEST_TIMEOUT_MS);
    };
    const onData = (buf) => {
      output += buf.toString();
      arm(); // any output is forward progress — reset the watchdog
    };
    child.stdout.on("data", onData);
    child.stderr.on("data", onData);
    child.on("error", (e) => {
      output += `\n[spawn error] ${e.message}\n`;
    });
    arm();
    child.on("close", (code, signal) => {
      clearTimeout(timer);
      output = output.replace(/\x1b\[[0-9;]*m/g, "");
      // Surface libtest's enumeration count + summary so the CI log distinguishes
      // a truncated run (ran < enumerated) from a genuinely small suite.
      const hdr = output.match(/^running (\d+) tests?$/m);
      const res = output.match(/^test result:.*$/m);
      console.error(
        `  [run] ${hdr ? `enumerated ${hdr[1]}` : "no header"}` +
          ` | ${res ? res[0] : "no summary line (truncated/crashed)"}` +
          ` | exit ${signal ? `signal ${signal}` : `code ${code}`}`,
      );
      // The offending test is the last `test <name> ... ` start line with no
      // result suffix. A crash does NOT always leave that line LAST: a panic,
      // a `fatal runtime error: stack overflow` message, or the signal-handler
      // crash logger can print extra lines AFTER it, so checking only the final
      // line misses the culprit (inFlight=null → no recovery → the whole run
      // truncates). Scan the entire output instead. A cleanly-finished test
      // always carries its result on the same line (`test X ... ok`), so this
      // pattern only matches a started-but-unfinished test and stays null on a
      // clean finish.
      // `(?: - should panic)?` strips libtest's display annotation so the name
      // is the bare test PATH `--skip` expects (the panic suffix is not part of
      // the filter name).
      // The dangling line is not always CLEAN: the crashing test's stderr can
      // interleave onto it before any newline (`test X ... thread 'X' has
      // overflowed its stack`), so require only that the suffix is NOT a
      // libtest result — not that the line ends after `... `.
      let inFlight = null;
      const startRe =
        /^test (.+?)(?: - should panic)? \.\.\. (?!ok\b|FAILED\b|ignored\b)[^\n]*$/gm;
      for (let m; (m = startRe.exec(output)) !== null; ) inFlight = m[1];
      resolve({ output, killed, code, signal, inFlight });
    });
  });
}

// Run a libtest binary to completion, tolerating tests that HANG or CRASH the
// process. libtest with `--test-threads 1` runs tests sequentially, so a test
// that deadlocks (watchdog SIGKILL) or aborts the process (SIGABRT/SIGSEGV/
// SIGBUS, plus a stack-overflow `fatal runtime error`) truncates EVERY test
// after it. Each round we identify the dangling in-flight test and re-run,
// SKIPPING both it and every test already finished — so each round runs only
// the still-unseen suffix (total work ~= O(tests), not O(rounds x tests),
// which matters for a cell with many independent crashers). We stop when a
// round finishes with no dangling test. Without this a single bad test would
// zero the cell (hang -> CI timeout) or truncate it (crash -> every later test
// silently MISSING). Crashed/hung tests are surfaced as synthetic FAILED lines
// so they show in the run result and are never baselined.
async function runOneBin(bin, cwd, baseArgs) {
  const seen = new Set(); // test paths that produced a result (ok/FAILED/ignored)
  const bad = []; // [{ name, reason }] — tests that hung or crashed the process
  // Quarantined tests are skipped from the very first round (treated as
  // already-handled) so a flaky hang/crash can't block or truncate the run.
  const quarantined = [...IGNORE];
  // `(?: - should panic)?` strips the display annotation so the captured name
  // is the bare test PATH (what `--skip` matches).
  const resultRe =
    /^test (.+?)(?: - should panic)? \.\.\. (?:ok|FAILED|ignored)\b/gm;
  let combined = "";
  // A generous cap (a cell may have dozens of independent crashers); each round
  // strictly shrinks the unseen set, so this terminates well before the cap.
  const MAX_ROUNDS = 1024;
  for (let round = 0; round < MAX_ROUNDS; round++) {
    const skipNames = [...seen, ...bad.map((t) => t.name), ...quarantined];
    const skipArgs = skipNames.length
      ? ["--exact", ...skipNames.flatMap((n) => ["--skip", n])]
      : [];
    const r = await streamWithWatchdog(bin, [...baseArgs, ...skipArgs], cwd);
    // A kill/crash mid-test leaves a dangling `test <name> ... ` line with no
    // newline; terminate it so the next round's output (or the synthetic FAILED
    // lines below) can't merge into it and confuse the libtest parser.
    combined += r.output.endsWith("\n") ? r.output : r.output + "\n";
    // Record every test that produced a result this round (so we skip it next).
    for (let m; (m = resultRe.exec(r.output)) !== null; ) seen.add(m[1]);
    if (!r.inFlight) break; // finished with no test left dangling
    if (seen.has(r.inFlight) || bad.some((t) => t.name === r.inFlight)) {
      // The dangling test was already accounted for (a skip that didn't take or
      // a name we can't match) — re-running would loop, so stop here.
      console.error(
        `  [recover] '${r.inFlight}' still in flight after being skipped — ` +
          `stopping retries for ${bin}`,
      );
      break;
    }
    const reason = r.killed ? "hang (timeout)" : `crash (${r.signal || `exit ${r.code}`})`;
    bad.push({ name: r.inFlight, reason });
    console.error(
      `  [recover] '${r.inFlight}' ${reason} — re-running remaining tests ` +
        `with it skipped (${bad.length} bad, ${seen.size} done)`,
    );
  }
  // Mark each bad test as FAILED so it's visible (and excluded from baselines).
  for (const t of bad) combined += `test ${t.name} ... FAILED\n`;
  if (bad.length) {
    console.error(`  [recover] ${bad.length} test(s) skipped: ${bad.map((t) => `${t.name} [${t.reason}]`).join(", ")}`);
  }
  return combined;
}

async function runBins(bins, cwd) {
  let out = "";
  for (const bin of bins) {
    codesign(bin);
    // Print the path so parseLibtest can attribute tests to a binary.
    out += `Running ${bin}\n`;
    const batch = await runOneBin(bin, cwd, ["--test-threads", "1", "--color", "never"]);
    out += batch + "\n";
    if (flag("rescue")) out += await rescueHiddenPasses(bin, cwd, batch);
  }
  return out;
}

// --rescue: solo-rerun every batch-FAILED test, and count a solo pass.
//
// runOneBin's recovery handles tests that HANG or CRASH the process — but not
// the third failure mode: a test that panics while holding the vendored
// suite's shared PROCESS_LOCK *write* guard POISONS the lock. Every later test
// in the same process then fails inside setup() (`RwLock::read().unwrap()`)
// while the process runs on, so nothing dangles and no recovery triggers —
// dozens of genuinely-passing tests report FAILED. That's a batch artifact,
// not a backend failure: each FAILED test gets one solo `--exact` rerun (own
// process, fresh lock) and a solo "ok" supersedes the batch FAILED in
// parseLibtest. Opt-in per CI cell: a cell that turns --rescue on must refresh
// its baseline in the same PR (rescue surfaces passes --check would flag).
async function rescueHiddenPasses(bin, cwd, batchOutput) {
  const failed = [];
  const re = /^test (.+?)(?: - should panic)? \.\.\. FAILED\b/gm;
  for (let m; (m = re.exec(batchOutput)) !== null; ) {
    // Never solo-run a quarantined test: IGNORE holds tests that hang, crash,
    // or OOM the machine (e.g. heap_limits) — a solo rerun re-triggers exactly
    // that (killed our CI runner twice before this check).
    if (!IGNORE.has(m[1])) failed.push(m[1]);
  }
  let out = "";
  let rescued = 0;
  for (const name of failed) {
    // On Linux, cap each solo run's address space at 8 GiB: a test that
    // allocates unboundedly (heap_limits-class) must die with its own OOM
    // instead of taking down the CI runner VM. macOS has no working
    // `ulimit -v`, so the plain spawn is used there (dev machines have swap;
    // the output-silence watchdog still bounds wall clock).
    const args = [name, "--exact", "--test-threads", "1", "--color", "never"];
    const r =
      os.platform() === "linux"
        ? await streamWithWatchdog(
            "/bin/sh",
            ["-c", `ulimit -v 8388608; exec "$0" "$@"`, bin, ...args],
            cwd,
          )
        : await streamWithWatchdog(bin, args, cwd);
    if (/^test result: ok\. 1 passed/m.test(r.output)) {
      out += `test ${name} ... ok\n`;
      rescued++;
    }
  }
  if (failed.length) {
    console.error(`  [rescue] ${rescued}/${failed.length} batch-FAILED tests pass solo`);
  }
  return out;
}

async function runCargoSelf() {
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
    out += await runBins(bins, ROOT);
  }
  if (unbuildable.length) console.error(`unbuildable targets: ${unbuildable.join(", ")}`);
  return finalize(parseLibtest(out));
}

async function runCargoDeno() {
  const denoDir = reqDenoDir();
  ensureDenoV8Patch(denoDir);
  const env = {
    CARGO_TERM_COLOR: "never",
    NEXTEST_EXPERIMENTAL_LIBTEST_JSON: "1",
  };
  if (needsCodesign) {
    const list = run(
      "cargo",
      [
        "nextest",
        "list",
        "-p",
        s.package,
        "--message-format",
        "json",
        "--list-type",
        "binaries-only",
        "--color",
        "never",
        "--no-pager",
      ],
      { cwd: denoDir, env, echo: false },
    );
    if (list.code !== 0) {
      process.stderr.write(list.out);
      console.error(`deno_core nextest list failed — counts as 0 passing`);
      return finalize({ pass: new Set(), fail: new Set(), skip: new Set() });
    }
    for (const bin of nextestBinaryPaths(list.out)) codesign(bin);
  }

  const skipArgs = [...IGNORE].flatMap((name) => ["--skip", name]);
  const r = run(
    "cargo",
    [
      "nextest",
      "run",
      "-p",
      s.package,
      "--message-format",
      "libtest-json",
      "--message-format-version",
      "0.1",
      "--no-fail-fast",
      "--no-tests=pass",
      "--color",
      "never",
      "--no-pager",
      "--success-output",
      "never",
      "--failure-output",
      "final",
      "--status-level",
      "none",
      "--final-status-level",
      "none",
      ...(skipArgs.length ? ["--", ...skipArgs] : []),
    ],
    { cwd: denoDir, env, echo: false },
  );
  const parsed = parseNextestJson(r.out);
  if (r.code !== 0 && parsed.pass.size + parsed.fail.size + parsed.skip.size === 0) {
    process.stderr.write(r.out);
  }
  return finalize(parsed);
}

function nextestBinaryPaths(output) {
  const bins = [];
  for (const line of output.split("\n")) {
    if (!line.startsWith("{")) continue;
    let message;
    try {
      message = JSON.parse(line);
    } catch {
      continue;
    }
    for (const binary of Object.values(message["rust-binaries"] || {})) {
      if (binary && typeof binary["binary-path"] === "string") {
        bins.push(binary["binary-path"]);
      }
    }
  }
  return bins;
}

function ensureIcuData() {
  const p = path.join(ROOT, "vendor/rusty_v8/third_party/icu/common/icudtl.dat");
  if (fs.existsSync(p) && fs.statSync(p).size > 1024 * 1024) {
    return;
  }
  // The vendored rusty_v8 tests `include_bytes!` the real ICU common data that
  // stock rusty_v8 checks out via its nested Chromium ICU submodule. Older local
  // runs may have left a tiny synthetic header in this ignored path; remove it
  // and materialize the pinned submodule so the test binary embeds real data.
  fs.rmSync(path.join(ROOT, "vendor/rusty_v8/third_party/icu"), {
    recursive: true,
    force: true,
  });
  const r = run("git", ["-C", "vendor/rusty_v8", "submodule", "update", "--init", "third_party/icu"], {
    echo: false,
  });
  if (r.code !== 0 || !fs.existsSync(p) || fs.statSync(p).size <= 1024 * 1024) {
    die(`failed to materialize real ICU data at ${p}\n${r.out}`);
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

function ensureDenoV8Patch(denoDir) {
  const cargoToml = path.join(denoDir, "Cargo.toml");
  const text = fs.existsSync(cargoToml) ? fs.readFileSync(cargoToml, "utf8") : "";
  const patchedPaths = [];
  const re = /^\s*v8\s*=\s*\{[^}]*\bpath\s*=\s*"([^"]+)"/gm;
  for (let m; (m = re.exec(text)) !== null; ) {
    patchedPaths.push(path.resolve(denoDir, m[1]));
  }
  if (!patchedPaths.some((p) => p === ROOT)) {
    const found = patchedPaths.length ? patchedPaths.join(", ") : "none";
    die(
      `deno checkout is not patched to use this v8x checkout.\n` +
        `expected [patch.crates-io] v8 path: ${ROOT}\n` +
        `found v8 patch path(s): ${found}`,
    );
  }
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

// Drop quarantined tests from a result set. parseLibtest prefixes each name
// with its binary (`deno_core::<path>`); IGNORE holds bare `<path>`, so match
// against the name with the leading `<bin>::` segment stripped.
function dropIgnored(set) {
  if (!IGNORE.size) return;
  for (const n of [...set]) {
    const bare = n.includes("::") ? n.slice(n.indexOf("::") + 2) : n;
    if (IGNORE.has(bare) || IGNORE.has(n)) set.delete(n);
  }
}

function finalize(parsed) {
  dropIgnored(parsed.pass);
  dropIgnored(parsed.fail);
  dropIgnored(parsed.skip);
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
