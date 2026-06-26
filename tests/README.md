# tests

## `tests/modules` — ES-module conformance suite

Differential tests for the module system: every fixture is run on stock-v8 deno
(the oracle) and its stdout captured to `<fixture>.expected`. Each v82jsc backend
must produce **byte-identical** output.

Coverage (`tests/modules/fixtures/`):

| fixture | feature |
| --- | --- |
| `01_basic_imports` | named / default / namespace imports |
| `02_cycle` | circular ESM (live bindings) |
| `03_barrel` | `export *`, `export { x as y } from` re-exports |
| `04_tla` | top-level await |
| `05_dynamic_import` | dynamic `import()` |
| `06_json_static` | `import … with { type: "json" }` |
| `07_json_dynamic` | dynamic JSON import with attributes |
| `08_node_builtins` | `node:os` / `node:process` / `node:path` |
| `09_import_meta` | `import.meta.url` / `import.meta.main` |
| `20`–`27` | npm packages (zod, hono, commander, lodash, date-fns, nanoid, preact SSR, react SSR) — pinned versions |

### Run against one binary

```sh
tests/modules/run.sh /path/to/deno [label]
```

### Run the full backend matrix

Build a v82jsc deno per backend, point the env vars at the binaries, and run:

```sh
DENO_STOCK=$(command -v deno) \
DENO_JSC=/path/to/deno-jsc-vendored \
DENO_SYSTEM_JSC=/path/to/deno-jsc-system \
DENO_QUICKJS=/path/to/deno-quickjs \
  tests/modules/matrix.sh
```

Any backend whose env var is unset is reported `SKIP`.

### Notes

- The vendored-JSC backend implements the module system with **native JSC module
  records** (zero string rewriting) — it matches stock-v8 on every fixture,
  including the cases the string rewriter cannot do (cycles, zod, commander).
- The system-JavaScriptCore backend has no C++ module API, so it targets
  **bundled** apps (`deno compile` / desktop); unbundled-ESM gaps don't apply
  once the graph is flattened.
- The npm fixtures need network (npm registry) the first run; pinned versions
  keep output deterministic.
