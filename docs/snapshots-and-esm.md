# Snapshots, System JSC, and `deno compile`

## The constraint

System JSC = Apple's `JavaScriptCore.framework`. OS-shipped dylib, can't patch.
Two APIs we'd want are **SPI, not in the public headers** (verified against the
MacOSX26.2 SDK — they're missing even on the latest, not just old WebKit):

- ESM module-loader hooks — public API has only `evaluateScript:withSourceURL:`
  (plain script). `JSScript` (the module class) is forward-declared in
  `JSContext.h` with **no public `JSScript.h`** anywhere in the SDK.
- bytecode-cache / snapshot serialization — lives on that same SPI `JSScript`
  (`cachePath:`); no public header. Newer WebKit widens the SPI, but it stays
  unexported, so the system dylib can't be linked against it.

(Vendored `jsc` and `quickjs` we build ourselves, so we add these freely — same
as Bun forking WebKit. System JSC is the only hard case.)

## Snapshots

V8 `SnapshotCreator` captures a heap blob, restores on boot to skip
parse/compile. The shim **polyfills it via the engine bytecode cache** on owned
engines (`jsc`, `quickjs`) → measured startup + throughput win. On System JSC
the cache API isn't exposed, so this path is gone there.

For `deno compile` specifically, snapshots don't matter for correctness (below).

## Why System JSC can back `deno compile` but not main `deno`

- main `deno` = **open world**: resolves/fetches ESM at runtime (dynamic
  `import()`, HTTPS/JSR/npm) → needs live loader hooks → System JSC can't.
- `deno compile` = **closed world**: every module known at build time → we do
  module work ahead of time, hand the engine a finished script.

### How `deno compile` works (Deno source)

Output = copy of `denort` binary + appended data section. Not an eszip (that's
`--eszip`).

- **Build** — `cli/standalone/binary.rs` `write_bin`: build `deno_graph`,
  transpile every module to plain JS (`RemoteModuleEntry`), build VFS, assemble
  `Metadata` (`entrypoint_key`, perms, v8 flags…).
- **Embed** — `libsui` appends section **`d3n0l4nd`** (Mach-O / ELF / PE).
  Engine-agnostic, nothing V8 here.
- **Startup** — `cli/rt/binary.rs extract_standalone`: `find_section`, parse
  `Metadata`, rebuild module store + VFS → `run::run` → `execute_main_module`.
- **Load** — `EmbeddedModuleLoader` (`cli/rt/run.rs`) feeds modules **lazily**;
  V8 drives imports, pulls from embedded store. **User code never snapshotted.**
  V8 startup snapshot (`deno_snapshots::CLI_SNAPSHOT`) = Deno runtime globals
  only, zero user code. Code-cache (`code_cache_key`) = optimization only.

### The substitution

V8-specific bits unavailable on System JSC = startup snapshot + code-cache.
Both **optimizations, not correctness** — strip them, compile still works. Sole
**load-bearing** need = `EmbeddedModuleLoader` feeding an ESM graph at startup.
That's the one hook System JSC lacks.

Kill that need at build time: **bundle the closed-world graph to one non-ESM
script** before embedding.

1. Bundle internal Deno runtime JS → one non-ESM program (strip imports).
2. Bundle user's whole graph → one non-ESM unit.
3. Embed via the same `d3n0l4nd` trailer/VFS. At startup feed System JSC **one
   plain script** → no loader, no snapshot, no code-cache. Nothing the dylib
   fails to expose.

Main `deno` can't do this — module set unknown till runtime. `compile`'s is
known. That asymmetry is the whole reason.
