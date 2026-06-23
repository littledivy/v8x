#!/usr/bin/env bash
# Clone + patch + build the vendored WebKit JavaScriptCore (JSCOnly port) so the
# `vendor_jsc` feature can link it. WebKit main uses newer clang warning flags
# that Apple clang rejects under -Werror; we patch those out. ~1GB clone (shallow)
# + a multi-minute build. Re-run is incremental (ninja).
set -euo pipefail
cd "$(dirname "$0")/.."
WK=vendor/webkit

if [ ! -d "$WK/.git" ] && [ ! -d "$WK/Source/JavaScriptCore" ]; then
  git clone --depth 1 https://github.com/WebKit/WebKit.git "$WK"
fi

# Patch 1: C++ compiles go through clang-wrapper — inject -Wno-unknown-warning-option.
WRAP="$WK/Source/cmake/clang-wrapper"
if ! grep -q "v82jsc" "$WRAP"; then
  perl -0pi -e 's/if \[\[ "\$WK_UNIFIED_SOURCES_BUNDLE_POLICY" != NoBundle \]\]; then\n    exec "\$\@"\nfi/if [[ "\$WK_UNIFIED_SOURCES_BUNDLE_POLICY" != NoBundle ]]; then\n    # v82jsc: tolerate unknown warning flags under -Werror.\n    __cc="\$1"; shift\n    exec "\$__cc" -Wno-unknown-warning-option "\$\@"\nfi/' "$WRAP"
fi

# Patch 2: C compiles bypass the wrapper — drop the unsupported flag at the source.
FLAGS="$WK/Source/cmake/WebKitCompilerFlags.cmake"
sed -i '' 's/^    WEBKIT_PREPEND_GLOBAL_COMPILER_FLAGS(-Wno-character-conversion)/    # v82jsc: removed (Apple clang lacks this flag)\n    # WEBKIT_PREPEND_GLOBAL_COMPILER_FLAGS(-Wno-character-conversion)/' "$FLAGS"

# Static build: emits libJavaScriptCore.a / libWTF.a / libbmalloc.a so the
# `vendor_jsc` feature links JSC INTO the binary (self-contained, no dylib).
"$WK/Tools/Scripts/build-jsc" --jsc-only --release --cmakeargs="-DENABLE_STATIC_JSC=ON"

# WebKit splits JSC into JavaScriptCore + JavaScriptCoreJIT targets; the JIT
# objects are linked directly into bin/jsc, never archived. Bundle them into
# libJavaScriptCoreJIT.a so the Rust build can force_load them.
REL="$WK/WebKitBuild/JSCOnly/Release"
find "$REL/Source/JavaScriptCore/CMakeFiles/JavaScriptCoreJIT.dir" -name '*.o' \
  ! -name '*pch_obj*' > /tmp/v8x_jitobjs.txt
libtool -static -o "$REL/lib/libJavaScriptCoreJIT.a" -filelist /tmp/v8x_jitobjs.txt
echo "Static JSC built: $REL/lib/{libJavaScriptCore,libJavaScriptCoreJIT,libWTF,libbmalloc}.a"
