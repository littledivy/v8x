#!/usr/bin/env bash
# Init the pinned quickjs-ng + WAMR submodules and apply our patches on top.
#
# Both vendors track an exact upstream commit (see .gitmodules / the gitlinks):
#   - vendor/quickjs-ng  -> tag v0.15.1
#   - vendor/wamr        -> master snapshot 26756a5c (pre the 2026-06-25 fork merges)
# Our changes live as patch files under patches/ applied to the pristine submodule
# working tree — never committed into the vendored source — so bumping the pin is:
# move the submodule, refresh the patch, done.
#
# WAMR additionally needs our own CMake driver (vendor/wamr/v82jsc/CMakeLists.txt),
# which has no upstream counterpart; it's stored under patches/ and copied in.
#
# Idempotent: re-running skips already-applied patches. build.rs calls this before
# compiling either engine.
set -euo pipefail
cd "$(dirname "$0")/.."

# Apply every patches/<prefix>-NN-*.patch onto a submodule, in numeric order.
# Each patch is one logical change; the series is the full delta from the pinned
# upstream commit. Idempotent: a patch that already reverse-applies is skipped.
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

apply_series vendor/quickjs-ng quickjs
apply_series vendor/wamr       wamr

# WAMR: drop in our CMake driver (interpreter-only static vmlib). Not an upstream
# file, so it ships as a plain copy rather than a patch.
mkdir -p vendor/wamr/v82jsc
cp patches/wamr-v82jsc-CMakeLists.txt vendor/wamr/v82jsc/CMakeLists.txt

echo "vendor setup done: quickjs-ng + WAMR patched"
