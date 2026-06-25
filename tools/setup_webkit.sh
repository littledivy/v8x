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

# Static build. USE_THIN_ARCHIVES=OFF emits real self-contained archives.
"$WK/Tools/Scripts/build-jsc" --jsc-only --release \
  --cmakeargs="-DENABLE_STATIC_JSC=ON -DUSE_THIN_ARCHIVES=OFF"
REL="$WK/WebKitBuild/JSCOnly/Release"

# The build emits libJavaScriptCore.a but NOT the split-out JavaScriptCoreJIT
# target's archive (its objects — which include the LLInt/IPInt assembly deno
# needs — are left loose). Bundle them into libJavaScriptCoreJIT.a ourselves.
# IMPORTANT: link the final binary with Apple's ld (-fuse-ld=/usr/bin/ld), NOT
# ld64.lld — lld reorders/strips the LLInt opcode assembly, breaking the exact-
# offset RELEASE_ASSERT in IPInt::initialize() (SIGKILL at startup). And the
# binary must be code-signed with the com.apple.security.cs.allow-jit
# entitlement (JSC JIT). See the v82jsc README / deno [patch] notes.
JITDIR="$REL/Source/JavaScriptCore/CMakeFiles/JavaScriptCoreJIT.dir"
if [ -d "$JITDIR" ] && [ ! -f "$REL/lib/libJavaScriptCoreJIT.a" ]; then
  find "$JITDIR" -name '*.o' -print0 \
    | xargs -0 xcrun libtool -static -o "$REL/lib/libJavaScriptCoreJIT.a"
fi
echo "Static JSC built: $REL/lib/{libJavaScriptCore,libJavaScriptCoreJIT,libWTF,libbmalloc}.a"
