#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
SYMS="${1:-/tmp/qjs_union.txt}"
P='(v8__|v8_inspector__|std__|simdutf__|cppgc__)[A-Za-z0-9_]+'
cat src/quickjs/shim_core.rs src/quickjs/shim_*.rs src/quickjs/fam_*.rs 2>/dev/null \
  | { grep -oE "extern \"C\" fn ${P}" || true; } | sed -E 's/.* //' | sort -u > /tmp/qjs_impl.txt
cat src/quickjs/shim_core.rs src/quickjs/shim_*.rs src/quickjs/fam_*.rs 2>/dev/null \
  | { grep -oE "!\(${P}" || true; } | sed -E 's/^!\(//' | sort -u >> /tmp/qjs_impl.txt
sort -u /tmp/qjs_impl.txt -o /tmp/qjs_impl.txt
{
  echo "//! AUTO-GENERATED QuickJS link stubs (tools/gen_qjs_shims.sh). Not yet implemented."
  echo "#![allow(non_snake_case)]"; echo
  comm -23 <(sort -u "$SYMS") /tmp/qjs_impl.txt | while read -r s; do
    [ -z "$s" ] && continue
    echo "#[unsafe(no_mangle)] pub extern \"C\" fn ${s}() { unimplemented!(\"${s}\") }"
  done
} > src/quickjs/shims.rs
echo "qjs stubs: $(grep -c no_mangle src/quickjs/shims.rs), implemented: $(wc -l < /tmp/qjs_impl.txt)"
