#!/bin/bash
#
# Build Bob Salita's faster search for timing against this port:
#
#   bench/comparison/build-bsalita.sh <BSalita/bridge-solver checkout at b12bb3f> [OUTDIR]
#
# His own build.sh makes the PGO'd library (libbsolver.dylib), which is what
# his users run. bsalita-bench200.cc then drives it over a whole corpus in one
# process. A second, non-PGO binary adds a node counter after each search, so a
# run can confirm it searched the lock-step tree (183,796,287 nodes on
# fixtures/divergence/lockstep-200.pbn) before anything is timed.
#
# On macOS, build.sh stops at its line 160 under the system bash 3.2 (an empty
# array under `set -u`), after the library is built and before its command-line
# programs are linked. The library is all this needs, so that failure is
# tolerated as long as a fresh library came out of it.
set -euo pipefail

SRC=$(cd "${1:?usage: build-bsalita.sh <checkout> [outdir]}" && pwd)
OUT=$(mkdir -p "${2:-$(pwd)/bsalita-build}" && cd "${2:-$(pwd)/bsalita-build}" && pwd)
HERE=$(cd "$(dirname "$0")" && pwd)
[ -f "$SRC/solver.h" ] && [ -f "$SRC/build.sh" ] || { echo "no solver.h/build.sh in $SRC" >&2; exit 1; }

rm -f "$SRC/libbsolver.dylib"
( cd "$SRC" && bash build.sh ) || true
[ -f "$SRC/libbsolver.dylib" ] || { echo "build.sh produced no libbsolver.dylib" >&2; exit 1; }
cp "$SRC/libbsolver.dylib" "$OUT/"

OPTS=(-std=c++17 -O3 -fno-stack-protector -DNDEBUG -pthread -I"$SRC")
clang++ "${OPTS[@]}" -o "$OUT/bench200" "$HERE/bsalita-bench200.cc" \
  -L"$OUT" -lbsolver -Wl,-rpath,@loader_path

# The counting build: his solver.cc with one line added after each search.
work=$(mktemp -d); trap 'rm -rf "$work"' EXIT
python3 - "$SRC/solver.cc" "$HERE/bsalita-bench200.cc" "$work" <<'PY'
import sys
solver, driver, work = sys.argv[1:]
src = open(solver).read()
key = "      seat_done(trump, lead_seat, ns_tricks);\n"
if src.count(key) != 1:
    sys.exit(f"expected one seat_done call in solver.cc, found {src.count(key)}")
src = ("#include <stdint.h>\nuint64_t g_bench_nodes = 0;\n"
       + src.replace(key, "      g_bench_nodes += min_max.Nodes();\n" + key))
open(f"{work}/solver.cc", "w").write(src)
d = open(driver).read()
d = ("#include <stdint.h>\nextern uint64_t g_bench_nodes;\n"
     + d.replace("  return 0;\n}",
                 '  fprintf(stderr, "nodes %llu\\n", (unsigned long long)g_bench_nodes);\n'
                 "  return 0;\n}"))
open(f"{work}/bench200.cc", "w").write(d)
PY
clang++ "${OPTS[@]}" -w -o "$OUT/bench200-count" "$work/solver.cc" \
  "$SRC/pool.cc" "$SRC/claim.cc" "$SRC/claim-engine.cc" "$work/bench200.cc"

cat <<EOF

built $OUT/bench200        PGO library, one process for a whole corpus
      $OUT/bench200-count  no PGO, prints "nodes N" on stderr; check it first:
  $OUT/bench200-count fixtures/divergence/lockstep-200.pbn >/dev/null
EOF
