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
# GATE PRESERVATION (load-bearing, see C4/C6 docs): every symbol that also has
# a REAL implementation in src/hermes/core.rs (only compiled when
# `link_hermes` is on, see src/hermes/mod.rs) needs its stub here gated with
# `#[cfg(not(feature = "link_hermes"))]`, or the real-symbol build gets a
# duplicate-symbol error at codegen time (both the stub AND the real fn would
# be defined in the same crate). This generator has no static way to know
# which symbols core.rs will eventually implement, so it treats the CURRENTLY
# gated set in the checked-in shims.rs as the source of truth and reapplies
# those same gates to the freshly generated stub list (a symbol whose stub
# disappears entirely, because core.rs now implements AND excludes it via
# /tmp/hermes_implemented.txt, is simply dropped like any other newly
# implemented symbol - nothing to gate). Re-running this script is therefore
# idempotent w.r.t. gates: it neither drops an existing gate nor needs a new
# one hand-added for a symbol whose real implementation predates this run.
# A symbol getting a REAL implementation for the first time still needs its
# gate hand-added once, in the same commit as the core.rs change (see the
# CONSTRUCT/DESTRUCT/etc gate pattern already in shims.rs).
#
# Usage: tools/gen_hermes_shims.sh
set -euo pipefail
cd "$(dirname "$0")/.."

OUT=src/hermes/shims.rs

# Capture the CURRENTLY gated symbol set (the checked-in file, before we
# overwrite it) so it can be reapplied below.
PREV_GATED=/tmp/hermes_prev_gated.txt
: > "$PREV_GATED"
if [ -f "$OUT" ]; then
  awk '
    /#\[cfg\(not\(feature = "link_hermes"\)\)\]/ { gate = 1; next }
    /#\[unsafe\(no_mangle\)\]/ { next }
    /^pub extern "C" fn / {
      if (gate) {
        sym = $0
        sub(/^pub extern "C" fn /, "", sym)
        sub(/\(.*/, "", sym)
        print sym
      }
      gate = 0
      next
    }
    { gate = 0 }
  ' "$OUT" | sort -u > "$PREV_GATED"
fi

# v8__* and v8_inspector__* symbols declared anywhere in the vendored crate.
#
# Two root causes of the earlier "the vendored rusty_v8 scope.rs/platform.rs
# decls use a form gen_hermes_shims.sh did not capture" gap (which forced
# hand-appending 14 symbols at the bottom of shims.rs) are fixed here:
#
# 1. FULL-IDENTIFIER match, not substring. A symbol like
#    `std__shared_ptr__v8__Platform__CONVERT__std__unique_ptr` was being
#    matched starting mid-identifier (at the embedded `v8__`), yielding a
#    truncated, WRONG stub name that never satisfies the real symbol at link
#    time. `[A-Za-z0-9_]*(v8__|v8_inspector__)[A-Za-z0-9_]+` captures the
#    whole identifier.
# 2. RECURSIVE file scan, not just the top-level `src/*.rs`. Some symbols
#    (e.g. `v8__TryCatch__CONSTRUCT`, `v8__AllowJavascriptExecutionScope__
#    CONSTRUCT`) are declared one directory deeper, in
#    vendor/rusty_v8/src/scope/raw.rs; a flat `src/*.rs` glob silently misses
#    that whole file. `find ... -name '*.rs'` walks every module file.
find vendor/rusty_v8/src -name '*.rs' -print0 \
  | xargs -0 grep -hoE '[A-Za-z0-9_]*(v8__|v8_inspector__)[A-Za-z0-9_]+' \
  | sort -u > /tmp/hermes_all_syms.txt

# Already hand-implemented in src/hermes/*.rs (excluding shims.rs itself).
P='[A-Za-z0-9_]*(v8__|v8_inspector__)[A-Za-z0-9_]+'
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

# Symbols vendor/rusty_v8/src/**/*.rs itself DEFINES with #[unsafe(no_mangle)]
# (the engine-independent "BASE" callback bridges for CustomPlatform tasks
# and Value(De)Serializer::Delegate/Inspector Channel/Client trampolines, plus
# the crdtp_shim.rs-provided inspector protocol). These are real definitions,
# not just extern decls, and are compiled unconditionally for every backend,
# so stubbing them again would be a duplicate-symbol error. Recursive for the
# same reason as the /tmp/hermes_all_syms.txt scan above.
while IFS= read -r -d '' f; do
  awk '/#\[unsafe\(no_mangle\)\]/{getline; print}' "$f"
done < <(find vendor/rusty_v8/src -name '*.rs' -print0) \
  | grep -oE "${P}" | sort -u >> /tmp/hermes_implemented.txt
sort -u /tmp/hermes_implemented.txt -o /tmp/hermes_implemented.txt

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
      if grep -qxF "$sym" "$PREV_GATED"; then
        echo "#[cfg(not(feature = \"link_hermes\"))]"
      fi
      echo "#[unsafe(no_mangle)]"
      echo "pub extern \"C\" fn ${sym}() { unimplemented!(\"${sym}\") }"
    done
} > "$OUT"

GATED_NOW=$(grep -c 'cfg(not(feature = "link_hermes"))' "$OUT" || true)
echo "hermes stubs: $(grep -c no_mangle "$OUT"), implemented: $(wc -l < /tmp/hermes_implemented.txt), gated: ${GATED_NOW}"
