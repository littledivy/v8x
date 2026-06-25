#!/usr/bin/env bash
# Re-vendor the rusty_v8 Rust API surface from a pinned crate release and apply
# our patches on top. This is a MAINTENANCE tool (run when bumping the pin), not
# part of the normal build: the vendored files are checked into src/ + gen/ so
# plain `cargo build` needs nothing.
#
# Model: this project IS rusty_v8 with a different binding layer. We vendor its
# Rust API verbatim (Local<T>, Scope, Object, …) and replace its C++ binding.cc
# (calls into V8) with our Rust shims (shim_*.rs / qjs/), which define the same
# ~570 v8__* C-ABI symbols over JavaScriptCore / QuickJS. Only THREE upstream
# files need edits, kept as patches/rusty_v8-*.patch:
#   01 lib-shim-mods        declare the shim modules
#   02 escape-slot          EscapeSlot over Local==JSValueRef (adds v8__EscapeSlot__*)
#   03 serializer-capacity  ValueSerializer::Release capacity for non-V8 buffers
#
# Bumping: edit RUSTY_V8_VERSION, run this, then implement any newly-declared
# v8__* the shims don't define yet (the linker names them on the next deno build).
#
# Usage: tools/sync_rusty_v8.sh [version]   (default: contents of RUSTY_V8_VERSION)
set -euo pipefail
cd "$(dirname "$0")/.."

VER="${1:-$(cat RUSTY_V8_VERSION)}"
echo "syncing rusty_v8 $VER"

# Files/dirs that are OURS — never overwritten by the vendor copy.
is_ours() {
  case "$1" in
    src/jsc_sys.rs|src/shims.rs|src/shim_*.rs|src/qjs/*) return 0 ;;
    *) return 1 ;;
  esac
}

# The two gen binding files we actually reference (build.rs). Vendored verbatim.
GEN_FILES=(
  "src_binding_debug_aarch64-apple-darwin.rs"
  "src_binding_simdutf_debug_aarch64-apple-darwin.rs"
)

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
echo "  downloading v8-$VER.crate"
curl -sL -o "$tmp/v8.crate" "https://static.crates.io/crates/v8/v8-$VER.crate"
tar xzf "$tmp/v8.crate" -C "$tmp"
up="$tmp/v8-$VER"
[ -d "$up/src" ] || { echo "error: bad crate (no src/)"; exit 1; }

# Copy every upstream src file verbatim, skipping our shim set.
echo "  copying vendored src/"
( cd "$up/src" && find . -type f ) | sed 's|^\./||' | while read -r rel; do
  dst="src/$rel"
  is_ours "$dst" && continue
  mkdir -p "$(dirname "$dst")"
  cp "$up/src/$rel" "$dst"
done

# Copy the referenced gen binding files.
echo "  copying gen/"
for g in "${GEN_FILES[@]}"; do
  cp "$up/gen/$g" "gen/$g"
done

# Apply our patches (fail loudly — a reject means upstream changed a file we patch).
echo "  applying patches/rusty_v8-*.patch"
for p in patches/rusty_v8-[0-9]*.patch; do
  [ -e "$p" ] || continue
  git apply "$p" || { echo "FATAL: $p did not apply — upstream changed a patched file; reconcile by hand"; exit 1; }
done

echo "rusty_v8 $VER synced. Review 'git diff', then build deno to surface any"
echo "newly-declared but unimplemented v8__* symbols (undefined-symbol link errors)."
