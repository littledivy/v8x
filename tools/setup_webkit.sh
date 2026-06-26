#!/usr/bin/env bash
# Init the pinned WebKit submodule, apply our patches, and build the static
# JSCOnly JavaScriptCore the `vendor_jsc` feature links. WebKit main uses newer
# clang warning flags Apple clang rejects under -Werror; the patches drop them.
# Submodule init fetches WebKit at the pinned commit (~GB); build is multi-minute
# (incremental on re-run).
set -euo pipefail
cd "$(dirname "$0")/.."
WK=vendor/webkit

# `--patches-only`: init the submodule + apply patches, then stop (no build).
# Used by build.rs to patch the source even when a PREBUILT lib archive is in
# place (the glue compiles against the patched headers). The full run also does
# the patch step, so it stays idempotent.
PATCHES_ONLY=0
[ "${1:-}" = "--patches-only" ] && PATCHES_ONLY=1

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

if [ "$PATCHES_ONLY" = "1" ]; then
  echo "WebKit patches applied (source only; no build)."
  exit 0
fi

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

# Keep the offlineasm LLInt/IPInt assembly intact under the deno binary link's
# `-Wl,-dead_strip`. The WASM in-place interpreter's opcode handlers are reached
# ONLY via computed jumps from the asm blob (no symbol references), and the
# objects carry MH_SUBSECTIONS_VIA_SYMBOLS, so the linker dead-strips those
# handlers as "unused" -> WASM executes garbage (add(2,3) => junk) even though
# JS is fine. The standalone `jsc` never sets -dead_strip so it isn't hit. Clear
# the flag on the two objects that hold the asm so ld keeps each whole when it's
# referenced. (jsc itself is unaffected; only the dead_strip'd deno link needs
# this.) Idempotent: a no-op once the bit is already clear.
python3 - "$REL/lib" <<'PY'
import sys, os, struct, subprocess, tempfile, shutil
libdir = sys.argv[1]
MH_SUBSECTIONS_VIA_SYMBOLS = 0x2000
targets = {
    "libJavaScriptCore.a": {"LowLevelInterpreter.cpp.o"},
    "libJavaScriptCoreJIT.a": {"UnifiedSource-llint-1.cpp.o"},
}
for arc, members in targets.items():
    path = os.path.join(libdir, arc)
    if not os.path.exists(path):
        continue
    work = tempfile.mkdtemp()
    try:
        subprocess.run(["ar", "x", os.path.abspath(path)], cwd=work, check=True)
        order = subprocess.run(
            ["ar", "t", os.path.abspath(path)], capture_output=True, text=True
        ).stdout.split()
        changed = False
        for m in order:
            if os.path.basename(m) not in members:
                continue
            p = os.path.join(work, os.path.basename(m))
            if not os.path.exists(p):
                continue
            with open(p, "r+b") as f:
                if struct.unpack("<I", f.read(4))[0] != 0xFEEDFACF:
                    continue
                f.seek(24)
                flags = struct.unpack("<I", f.read(4))[0]
                if flags & MH_SUBSECTIONS_VIA_SYMBOLS:
                    flags &= ~MH_SUBSECTIONS_VIA_SYMBOLS
                    f.seek(24)
                    f.write(struct.pack("<I", flags))
                    changed = True
        if changed:
            # `ar t` lists the archive symbol table (`__.SYMDEF`) on some
            # toolchains; libtool rejects it ("not an object file"). Keep only
            # extracted members that actually exist on disk.
            objs = [
                os.path.join(work, os.path.basename(m))
                for m in order
                if os.path.basename(m) != "__.SYMDEF"
                and os.path.exists(os.path.join(work, os.path.basename(m)))
            ]
            os.remove(path)
            subprocess.run(
                ["xcrun", "libtool", "-static", "-o", path] + objs, check=True
            )
            print(f"  patched MH_SUBSECTIONS_VIA_SYMBOLS off {members} in {arc}")
    finally:
        shutil.rmtree(work, ignore_errors=True)
PY

echo "Static JSC built: $REL/lib/{libJavaScriptCore,libJavaScriptCoreJIT,libWTF,libbmalloc}.a"
