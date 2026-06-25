#!/usr/bin/env bash
# Init the pinned vendor submodules and apply our patches on top. Every vendored
# dependency lives in vendor/<name> at an exact upstream commit (see .gitmodules /
# the gitlinks); our edits ship as patches/<name>-NN-*.patch applied to the
# submodule working tree — never committed into the vendored source.
#
#   vendor/rusty_v8     denoland/rusty_v8 @ v149.4.0 — the Rust API surface this
#                       crate IS. src/lib.rs #[path]-includes its modules; our
#                       shims (src/jsc/, src/quickjs/) replace its C++ binding.cc
#                       by defining the same ~570 v8__* C-ABI over JSC / QuickJS.
#                       2 patches: EscapeSlot (Local==JSValueRef), serializer cap.
#   vendor/quickjs-ng   quickjs-ng @ v0.15.1            (QuickJS backend)
#   vendor/wamr         wasm-micro-runtime @ 26756a5c   (WebAssembly for QuickJS)
#
# Bumping a pin: move the submodule to the new commit, re-run this; if a patch
# rejects, upstream touched a patched file — reconcile by hand. New v8__* that
# upstream declares surface as undefined-symbol link errors on the next build.
#
# Usage: setup_vendor.sh [rusty_v8|quickjs]
#   rusty_v8  (default) only the Rust API surface — enough for the JSC backend
#   quickjs             also the QuickJS + WAMR engine submodules
set -euo pipefail
cd "$(dirname "$0")/.."

MODE="${1:-rusty_v8}"

# Apply every patches/<prefix>-NN-*.patch onto a submodule, in numeric order.
# Idempotent: a patch that already reverse-applies is skipped.
apply_series() {
  local sub="$1" prefix="$2"
  if [ ! -e "$sub/.git" ]; then
    git submodule update --init "$sub"
  fi
  for p in patches/"$prefix"-[0-9]*.patch; do
    [ -e "$p" ] || continue
    if ! git -C "$sub" apply --reverse --check "../../$p" 2>/dev/null; then
      git -C "$sub" apply "../../$p" || echo "warn: $p may already be applied"
    fi
  done
}

# rusty_v8 is always needed — it's the crate's own source, used by both backends.
apply_series vendor/rusty_v8 rusty_v8

if [ "$MODE" = quickjs ]; then
  apply_series vendor/quickjs-ng quickjs
  apply_series vendor/wamr       wamr
  # WAMR's CMake driver has no upstream counterpart; copy it in.
  mkdir -p vendor/wamr/v82jsc
  cp patches/wamr-v82jsc-CMakeLists.txt vendor/wamr/v82jsc/CMakeLists.txt
fi

echo "vendor setup done ($MODE)"
