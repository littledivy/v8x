#!/usr/bin/env bash
# Assemble the full GitHub Pages site into _site/ (or $1):
#   /                  typst-built docs (site/*.typ)
#   /status/           test dashboard (docs/index.html) + CI-generated data
# Used by both pages.yml and ci.yml's report job — keep them in sync by
# keeping the logic here.
set -euo pipefail
cd "$(dirname "$0")/.."

TYPST=${TYPST:-typst}
OUT=${1:-_site}

rm -rf "$OUT"
mkdir -p "$OUT/status"

for f in site/*.typ; do
  name=$(basename "$f" .typ)
  echo "typst: $f -> $OUT/$name.html"
  "$TYPST" compile --root site --features html "$f" "$OUT/$name.html"
done
cp site/main.css "$OUT/"

cp docs/index.html "$OUT/status/index.html"
cp tests/status/report.json "$OUT/status/report.json"
# history.jsonl may not exist on the very first deploy.
cp tests/status/history.jsonl "$OUT/status/history.jsonl" 2>/dev/null || : > "$OUT/status/history.jsonl"

echo "site assembled in $OUT/"
