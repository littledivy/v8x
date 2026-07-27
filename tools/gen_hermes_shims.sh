#!/usr/bin/env bash
# Regenerate src/hermes/shims.rs: a no-arg link stub for every v8__* and
# v8_inspector__* C-ABI symbol the vendored rusty_v8 surface declares, except
# the cppgc__* symbols (hand-written in src/hermes/misc.rs, since those need
# real bodies to satisfy the Member/Persistent wrapper logic in
# vendor/rusty_v8/src/cppgc.rs) and crdtp__* (provided engine-independently by
# src/crdtp_shim.rs).
#
# Rust `extern "C"` FFI linking is name-only: the linker does not check
# argument/return types against the vendored `unsafe extern "C" { fn ... }`
# declarations, only that a symbol of that name exists somewhere in the final
# binary. So a no-arg `unimplemented!()` stub links cleanly against any
# declared signature (same technique tools/gen_qjs_shims.sh and
# tools/gen_shims.sh already use for the QuickJS/JSC backends).
#
# Usage: tools/gen_hermes_shims.sh
set -euo pipefail
cd "$(dirname "$0")/.."

OUT=src/hermes/shims.rs

# v8__* and v8_inspector__* symbols declared anywhere in the vendored crate.
grep -rhoE '(v8__|v8_inspector__)[A-Za-z0-9_]+' vendor/rusty_v8/src/*.rs \
  | sort -u > /tmp/hermes_all_syms.txt

# Already hand-implemented in src/hermes/*.rs (excluding shims.rs itself).
P='(v8__|v8_inspector__)[A-Za-z0-9_]+'
for f in src/hermes/*.rs; do
  [ "$(basename "$f")" = shims.rs ] && continue
  cat "$f" 2>/dev/null
done > /tmp/hermes_all_src.txt
{
  { grep -oE "extern \"C\" fn ${P}" /tmp/hermes_all_src.txt || true; } \
    | sed -E 's/.* //'
  { grep -oE "!\(${P}" /tmp/hermes_all_src.txt || true; } \
    | sed -E 's/^!\(//'
} | sort -u > /tmp/hermes_implemented.txt

{
  echo "//! AUTO-GENERATED Hermes link stubs (tools/gen_hermes_shims.sh)."
  echo "//!"
  echo "//! Cycle 0/1 scaffold: every v8__*/v8_inspector__* symbol the vendored"
  echo "//! rusty_v8 surface declares, stubbed as an unimplemented!() no-arg"
  echo "//! function so the crate links with zero Hermes dependency. Real"
  echo "//! implementations land incrementally in later cycles, same pattern as"
  echo "//! the QuickJS backend (see tools/gen_qjs_shims.sh)."
  echo "#![allow(non_snake_case)]"
  echo
  comm -23 <(sort -u /tmp/hermes_all_syms.txt) /tmp/hermes_implemented.txt \
    | while read -r sym; do
      [ -z "$sym" ] && continue
      echo "#[unsafe(no_mangle)]"
      echo "pub extern \"C\" fn ${sym}() { unimplemented!(\"${sym}\") }"
    done
} > "$OUT"

echo "hermes stubs: $(grep -c no_mangle "$OUT"), implemented: $(wc -l < /tmp/hermes_implemented.txt)"
