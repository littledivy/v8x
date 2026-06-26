#!/usr/bin/env bash
# Run the ES-module fixture suite against one deno binary and diff each result
# against its captured .expected (captured from stock-v8 deno — the oracle).
#
#   run.sh <deno-binary> [label]
#
# Exit 0 iff every fixture matches. Used by CI and tests/modules/matrix.sh.
set -u
DENO="${1:?usage: run.sh <deno-binary> [label]}"
LABEL="${2:-$(basename "$DENO")}"
DIR="$(cd "$(dirname "$0")/fixtures" && pwd)"

pass=0
fail=0
failed=""
for f in "$DIR"/*.mjs; do
  exp="${f%.mjs}.expected"
  [ -f "$exp" ] || continue
  name="$(basename "$f")"
  # --node-modules-dir=auto so the npm: fixtures resolve without a deno.json.
  got="$("$DENO" run -A --node-modules-dir=auto "$f" 2>/dev/null)"
  if [ "$got" = "$(cat "$exp")" ]; then
    pass=$((pass + 1))
  else
    fail=$((fail + 1))
    failed="$failed $name"
    echo "  FAIL $name"
    echo "    expected: $(cat "$exp" | head -1)"
    echo "    got:      $(echo "$got" | head -1)"
  fi
done

# Clean the throwaway npm install the fixtures trigger.
rm -rf "$DIR/node_modules" "$DIR/deno.lock" 2>/dev/null

echo "[$LABEL] PASS=$pass FAIL=$fail"
[ "$fail" -eq 0 ]
