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

apply_patch() {
  local sub="$1" patch="$2"
  # Fetch the submodule at its pinned commit if the tree isn't checked out yet.
  if [ ! -e "$sub/.git" ]; then
    git submodule update --init "$sub"
  fi
  # Apply (idempotent — skip if it already reverse-applies cleanly).
  if ! git -C "$sub" apply --reverse --check "../../$patch" 2>/dev/null; then
    git -C "$sub" apply "../../$patch" || echo "warn: $patch may already be applied"
  fi
}

apply_patch vendor/quickjs-ng patches/quickjs-0001-v82jsc.patch
apply_patch vendor/wamr       patches/wamr-0001-v82jsc.patch

# WAMR: drop in our CMake driver (interpreter-only static vmlib). Not an upstream
# file, so it ships as a plain copy rather than a patch.
mkdir -p vendor/wamr/v82jsc
cp patches/wamr-v82jsc-CMakeLists.txt vendor/wamr/v82jsc/CMakeLists.txt

echo "vendor setup done: quickjs-ng + WAMR patched"
