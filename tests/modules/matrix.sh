#!/usr/bin/env bash
# Run the ES-module fixture suite across every backend and print a matrix.
# stock-v8 deno is the oracle the .expected files were captured from; each
# v82jsc backend must produce byte-identical output.
#
# Point these env vars at deno binaries (any unset backend is SKIPPED):
#   DENO_STOCK        stock-v8 deno            (default: `deno` on PATH)
#   DENO_JSC          v82jsc, vendored JSC     (features: jsc)
#   DENO_SYSTEM_JSC   v82jsc, system framework (features: system_jsc)
#   DENO_QUICKJS      v82jsc, QuickJS-ng       (features: quickjs)
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
rc=0

run() {
  local bin="$1" label="$2"
  if [ -n "$bin" ] && [ -x "$bin" ]; then
    bash "$HERE/run.sh" "$bin" "$label" || rc=1
  else
    echo "[$label] SKIP (no binary)"
  fi
}

run "${DENO_STOCK:-$(command -v deno)}" "stock-v8"
run "${DENO_JSC:-}" "jsc-vendored"
run "${DENO_SYSTEM_JSC:-}" "jsc-system"
run "${DENO_QUICKJS:-}" "quickjs"

exit $rc
