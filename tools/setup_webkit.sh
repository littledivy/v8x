#!/usr/bin/env bash
# Init the pinned WebKit submodule, apply our patches, and build the static
# JSCOnly JavaScriptCore the `vendor_jsc` feature links. WebKit main uses newer
# clang warning flags Apple clang rejects under -Werror; the patches drop them.
# Submodule init fetches WebKit at the pinned commit (~GB); build is multi-minute
# (incremental on re-run).
set -euo pipefail
cd "$(dirname "$0")/.."
WK=vendor/webkit

# Fetch WebKit at the pinned submodule commit.
if [ ! -d "$WK/Source/JavaScriptCore" ]; then
  git submodule update --init --depth 1 "$WK"
fi

# Apply our WebKit patches (idempotent — skip if already applied). Patch paths
# are relative to the submodule dir, hence ../../ back to the repo root.
for p in patches/webkit-[0-9]*.patch; do
  [ -e "$p" ] || continue
  if ! git -C "$WK" apply --reverse --check "../../$p" 2>/dev/null; then
    git -C "$WK" apply "../../$p" || echo "warn: $p may already be applied"
  fi
done

# Static build. USE_THIN_ARCHIVES=OFF makes the build emit real, self-contained
# archives — including a proper libJavaScriptCoreJIT.a for the split-out JIT
# target (with thin archives the JIT objects are never archived). The Rust build
# (vendor_jsc) force_loads all four .a into the binary; no dylib, no rpath.
"$WK/Tools/Scripts/build-jsc" --jsc-only --release \
  --cmakeargs="-DENABLE_STATIC_JSC=ON -DUSE_THIN_ARCHIVES=OFF"
REL="$WK/WebKitBuild/JSCOnly/Release"
echo "Static JSC built: $REL/lib/{libJavaScriptCore,libJavaScriptCoreJIT,libWTF,libbmalloc}.a"
