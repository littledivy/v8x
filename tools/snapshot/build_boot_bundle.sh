#!/usr/bin/env bash
# JSC "snapshot": bundle deno's boot-time `ext:` ES modules into ONE plain
# Program (no import/export) that can be evaluated at startup via a script eval
# — no module loader, so the system-JSC build needs no ESM rewriter to boot.
#
#   build_boot_bundle.sh <dump-dir> [out.js]
#
# <dump-dir> is a capture from a real boot:
#   V82JSC_DUMP_MODULES=/tmp/extdump deno run -A trivial.js
# (the engine writes each compiled module, in load order, + a MANIFEST.)
#
# The boot graph is small (~8 ext modules). Two of them — ext:core/mod.js and
# ext:core/ops — are runtime-provided via globalThis.__bootstrap, so they're
# emitted as shims (re-export from __bootstrap; the ops shim re-exports every op
# the boot modules import). Everything else is inlined by esbuild (via
# `deno bundle`). `--format cjs` is required: the esm format leaves `import.meta`
# in, which is invalid in a Program; cjs shims it.
#
# VALIDATED: produces a ~70KB Program (0 top-level import/export) that parses as
# a plain script. Next: generate this at deno BUILD time from the extension
# registry, bake it in, eval at boot (+ JSC bytecode cache), and delete the
# rewriter. See native-modules-plan / the jsc-snapshot memory.
set -euo pipefail
DUMP="${1:?usage: build_boot_bundle.sh <dump-dir> [out.js]}"
OUT="${2:-boot_bundle.js}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cd "$WORK"

# spec -> flat local filename (only the bundled ext modules; core/* are shims).
map() { case "$1" in
  "ext:deno_bundle_runtime/bundle.ts") echo m_bundle.ts;;
  "ext:deno_features/flags.js") echo m_flags.js;;
  "ext:runtime/90_deno_ns.js") echo m_90.js;;
  "ext:runtime/97_navigator_user_agent_data.js") echo m_97.js;;
  "ext:runtime/98_global_scope_shared.js") echo m_98s.js;;
  "ext:runtime/98_global_scope_window.js") echo m_98win.js;;
  "ext:runtime/98_global_scope_worker.js") echo m_98work.js;;
  "ext:runtime_main/js/99_main.js") echo m_99.js;;
  "ext:core/mod.js") echo core_mod.js;;
  "ext:core/ops") echo core_ops.js;;
  *) echo "";; esac; }
SPECS="ext:deno_bundle_runtime/bundle.ts ext:deno_features/flags.js ext:runtime/90_deno_ns.js ext:runtime/97_navigator_user_agent_data.js ext:runtime/98_global_scope_shared.js ext:runtime/98_global_scope_window.js ext:runtime/98_global_scope_worker.js ext:runtime_main/js/99_main.js ext:core/mod.js ext:core/ops"

ENTRY=""
while IFS="$(printf '\t')" read -r file spec; do
  case "$spec" in ext:core/*) continue;; ext:*) ;; *) continue;; esac
  dst="$(map "$spec")"; [ -z "$dst" ] && continue
  cp "$DUMP/$file" "$dst"
  for k in $SPECS; do
    v="$(map "$k")"
    python3 - "$dst" "$k" "$v" <<'PY'
import sys
f,k,v=sys.argv[1:]
s=open(f).read().replace('"%s"'%k,'"./%s"'%v).replace("'%s'"%k,"'./%s'"%v)
open(f,'w').write(s)
PY
  done
  ENTRY="$ENTRY import \"./$dst\";"
done < "$DUMP/MANIFEST"

# core/mod.js + core/ops shims (the ops shim re-exports every imported op).
printf 'const b=globalThis.__bootstrap;\nexport const core=b.core,internals=b.internals,primordials=b.primordials;\n' > core_mod.js
python3 - "$DUMP" <<'PY'
import re, glob, os, sys
dump=sys.argv[1]
ops=set()
for f in glob.glob(os.path.join(dump,"*.js")):
    for m in re.finditer(r'import\s*\{([^}]*)\}\s*from\s*["\']ext:core/ops["\']', open(f).read()):
        for part in m.group(1).split(','):
            n=part.split(' as ')[0].strip()
            if n: ops.add(n)
ops=sorted(ops)
open("core_ops.js","w").write("const o=globalThis.__bootstrap.core.ops;\nexport const "+", ".join(f"{n}=o.{n}" for n in ops)+";\n")
print(f"ops re-exported: {len(ops)}", file=sys.stderr)
PY

printf '%s\n' "$ENTRY" > entry.js
deno bundle --platform deno --format cjs -o "$WORK/out.js" entry.js >&2
cp "$WORK/out.js" "$OLDPWD/$OUT" 2>/dev/null || cp "$WORK/out.js" "$OUT"
echo "boot bundle -> $OUT" >&2
