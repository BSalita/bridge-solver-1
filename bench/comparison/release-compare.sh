#!/bin/bash
#
# Time released bridge-solver versions against Hanhong Xue's C++ and Bob
# Salita's C++ on the 200 lock-step deals, single-threaded, one process per
# solver run, the way each is actually used:
#   - releases: the macOS arm64 CLI downloaded from GitHub, on a tagged PBN
#     (so their times include reading the file and the par calculation);
#   - Hanhong's C++ (macroxue/bridge-solver): clang PGO trained as its makefile
#     does, driven by upstream-driver.cc to solve all 200 in one process;
#   - Bob's C++ (BSalita/bridge-solver): his own build.sh with PGO, via
#     build-bsalita.sh's bench200.
#
#   bench/comparison/release-compare.sh [options]
#     --work DIR        build and result directory (default target/release-compare)
#     --releases "A B"  release tags, oldest first (default "v2.1.0 v2.2.0")
#     --upstream REV    macroxue/bridge-solver revision (default 630705d)
#     --bob REV         BSalita/bridge-solver revision (default b12bb3f)
#     --rounds N        timed rounds (default 8)
#     --setup-only      build and check, do not time
#
# Setup steps are skipped when their output exists, so a second run only
# times. Before timing it checks that every solver gives the same table on all
# 200 deals, and prints Bob's node count (183,796,287 at b12bb3f); it stops if
# the tables differ. Then ROUNDS rounds of every solver, rotating which goes
# first. A run starts only when no other process is above 80% CPU (waiting up
# to 300 s) and is marked dirty if one appears while it runs. Results:
# DIR/runs.tsv (every run) and DIR/summary.txt (best and median per solver,
# clean runs only, with speed relative to the newest release).
#
# Needs: gh (authenticated), git, clang++, xcrun llvm-profdata, macOS
# /usr/bin/time -l (it reports instructions retired and cycles).
set -uo pipefail
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
HERE=$ROOT/bench/comparison
WORK=$ROOT/target/release-compare
RELEASES="v2.1.0 v2.2.0"
UPSTREAM=630705d
BOB=b12bb3f
ROUNDS=8
SETUP_ONLY=
while [ $# -gt 0 ]; do
  case $1 in
    --work) WORK=$2; shift 2 ;;
    --releases) RELEASES=$2; shift 2 ;;
    --upstream) UPSTREAM=$2; shift 2 ;;
    --bob) BOB=$2; shift 2 ;;
    --rounds) ROUNDS=$2; shift 2 ;;
    --setup-only) SETUP_ONLY=1; shift ;;
    *) sed -n '2,/^set -uo/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 2 ;;
  esac
done
mkdir -p "$WORK"
WORK=$(cd "$WORK" && pwd)
P=$ROOT/fixtures/divergence/lockstep-200.pbn
TAGGED=$WORK/lockstep-200-tagged.pbn
log() { echo "== $*" >&2; }
die() { echo "release-compare: $*" >&2; exit 1; }

# --- setup ------------------------------------------------------------------
awk '/^N:/{n++; printf "[Board \"%d\"]\n[Dealer \"N\"]\n[Vulnerable \"None\"]\n[Deal \"%s\"]\n\n", n, $0}' \
  "$P" > "$TAGGED"

for v in $RELEASES; do
  bin=$WORK/rel/$v/bridge-solver-macos-aarch64
  [ -x "$bin" ] && continue
  log "downloading $v"
  mkdir -p "$WORK/rel/$v"
  gh release download "$v" -R bridge-craftwork/bridge-solver \
    -p bridge-solver-macos-aarch64 -D "$WORK/rel/$v" --clobber || die "download $v failed"
  chmod +x "$bin"
  xattr -d com.apple.quarantine "$bin" 2>/dev/null || true
done

UP=$WORK/upstream-$UPSTREAM
if [ ! -x "$UP/driver" ]; then
  log "building Hanhong's C++ at $UPSTREAM (clang PGO)"
  [ -d "$UP/src" ] || git clone -q https://github.com/macroxue/bridge-solver.git "$UP/src" || die "clone upstream failed"
  git -C "$UP/src" checkout -q "$UPSTREAM" || die "no upstream revision $UPSTREAM"
  prof=$UP/prof
  rm -rf "$prof"; mkdir -p "$prof"
  opts=(-std=c++17 -O3 -fno-stack-protector)
  clang++ "${opts[@]}" -fprofile-generate="$prof" -o "$prof/solver.p" \
    "$UP/src/solver.cc" "$UP/src/solver-cli.cc" || die "upstream instrumented build failed"
  # Its makefile's training run.
  (cd "$UP/src" && "$prof/solver.p" -if deals/hard/deal.8 >/dev/null) || die "upstream training run failed"
  xcrun llvm-profdata merge -o "$prof/default.profdata" "$prof"/*.profraw || die "profdata merge failed"
  # The driver's own main() has no profile, which clang warns about; harmless.
  clang++ "${opts[@]}" -I"$UP/src" -fprofile-use="$prof/default.profdata" \
    -Wno-profile-instr-unprofiled -Wno-backend-plugin \
    -o "$UP/driver" "$HERE/upstream-driver.cc" "$UP/src/solver.cc" || die "upstream driver build failed"
fi

BOBDIR=$WORK/bob-$BOB
if [ ! -x "$BOBDIR/bench200" ]; then
  log "building Bob's C++ at $BOB (his build.sh, PGO)"
  [ -d "$BOBDIR/src" ] || git clone -q https://github.com/BSalita/bridge-solver.git "$BOBDIR/src" || die "clone Bob's fork failed"
  git -C "$BOBDIR/src" checkout -q "$BOB" || die "no revision $BOB in Bob's fork"
  bash "$HERE/build-bsalita.sh" "$BOBDIR/src" "$BOBDIR" > "$BOBDIR/build.log" 2>&1 \
    || die "build-bsalita.sh failed; see $BOBDIR/build.log"
fi

# --- check ------------------------------------------------------------------
# Common form for a table: 20 numbers, strains NT,S,H,D,C, each with declarers
# S,N,W,E, i.e. opening leaders W,E,N,S -- the order both C++ drivers use.
log "checking tables"
"$BOBDIR/bench200-count" "$P" 2>"$WORK/bob-nodes.txt" >/dev/null
echo "Bob's C++ $BOB: $(grep -o 'nodes [0-9]*' "$WORK/bob-nodes.txt")" >&2
"$BOBDIR/bench200" "$P" > "$WORK/t-bob.txt" || die "Bob's bench200 failed"
"$UP/driver" "$P" | awk '{o=""; for(i=2;i<=NF;i++){v=((i-2)%4>=2)?13-$i:$i; o=o (i>2?" ":"") v} print o}' \
  > "$WORK/t-upstream.txt" || die "upstream driver failed"
for v in $RELEASES; do
  "$WORK/rel/$v/bridge-solver-macos-aarch64" -j 1 --input "$TAGGED" 2>/dev/null |
    sed -n 's/^\[DoubleDummyTricks "\(.*\)"\]/\1/p' |
    awk 'BEGIN{split("1 0 3 2",ord," ")} {o=""; for(t=0;t<5;t++) for(k=1;k<=4;k++){
           c=substr($0,ord[k]*5+t+1,1); o=o (o==""?"":" ") index("0123456789abcdef",tolower(c))-1} print o}' \
    > "$WORK/t-$v.txt"
done
n=$(wc -l < "$WORK/t-bob.txt" | tr -d ' ')
[ "$n" = 200 ] || die "Bob's driver gave $n tables, not 200"
for f in upstream $RELEASES; do
  cmp -s "$WORK/t-bob.txt" "$WORK/t-$f.txt" || die "$f's tables differ from Bob's (diff $WORK/t-bob.txt $WORK/t-$f.txt)"
done
echo "all solvers agree on all 200 tables" >&2
[ -n "$SETUP_ONLY" ] && exit 0

# --- time -------------------------------------------------------------------
names=() cmds=()
for v in $RELEASES; do names+=("bridge-solver $v"); cmds+=("$WORK/rel/$v/bridge-solver-macos-aarch64 -j 1 --input $TAGGED"); done
names+=("Hanhong C++ $UPSTREAM"); cmds+=("$UP/driver $P")
names+=("Bob C++ $BOB"); cmds+=("$BOBDIR/bench200 $P")
k=${#names[@]}
out=$(mktemp); trap 'rm -f "$out"' EXIT
busy() { ps -Ao pcpu=,comm= | awk '$1>80 && $2 !~ /bridge-solver|driver|bench200/ {print $1, $2; exit}'; }
one() {  # one <index>
  local waited=0 hit="" pid
  while [ -n "$(busy)" ] && [ $waited -lt 300 ]; do sleep 2; waited=$((waited + 2)); done
  # shellcheck disable=SC2086  # the command strings hold paths without spaces
  /usr/bin/time -l ${cmds[$1]} >/dev/null 2>"$out" &
  pid=$!
  while kill -0 $pid 2>/dev/null; do [ -z "$hit" ] && hit=$(busy); sleep 0.5; done
  wait $pid || hit="${hit:+$hit; }exit $?"
  printf '%s\t%s\t%s\t%s\t%s\n' "${names[$1]}" \
    "$(awk '/ real /{print $1}' "$out")" \
    "$(awk '/instructions retired/{print $1}' "$out")" \
    "$(awk '/cycles elapsed/{print $1}' "$out")" "${hit:+dirty: $hit}"
}
case $WORK in *" "*) die "work directory must not contain spaces" ;; esac
case $ROOT in *" "*) die "repository path must not contain spaces" ;; esac
log "timing $k solvers, $ROUNDS rounds (about a minute a round)"
printf 'solver\twall_s\tinstructions\tcycles\tnote\n' > "$WORK/runs.tsv"
for r in $(seq 1 "$ROUNDS"); do
  for i in $(seq 0 $((k - 1))); do one $(( (i + r - 1) % k )) | tee -a "$WORK/runs.tsv" >&2; done
done

# --- summary ----------------------------------------------------------------
newest="bridge-solver ${RELEASES##* }"
{
  echo "200 lock-step deals, one thread, $(sysctl -n machdep.cpu.brand_string), $(date '+%F %H:%M')"
  echo "best and median of clean runs; speed relative to $newest (higher is faster)"
  echo
  awk -F'\t' -v ref="$newest" '
    NR > 1 && $5 == "" { w[$1] = w[$1] " " $2; c[$1] = c[$1] " " $4 / 1e9; order[$1] = order[$1] ? order[$1] : ++n }
    function sorted(s, a,   m, i, j, t) { m = split(s, a, " "); for (i = 2; i <= m; i++) for (j = i; j > 1 && a[j-1] > a[j]; j--) { t = a[j]; a[j] = a[j-1]; a[j-1] = t } return m }
    function med(a, m) { return m % 2 ? a[(m+1)/2] : (a[m/2] + a[m/2+1]) / 2 }
    END {
      for (s in order) { m = sorted(w[s], a); bw[s] = a[1]; mw[s] = med(a, m); runs[s] = m; sorted(c[s], b); bc[s] = b[1]; mc[s] = med(b, m) }
      printf "%-26s %5s %16s %16s %8s %8s\n", "solver", "runs", "wall best/med s", "Gcycles best/med", "x wall", "x cycles"
      for (i = 1; i <= n; i++) for (s in order) if (order[s] == i)
        printf "%-26s %5d %8.2f/%-7.2f %8.2f/%-7.2f %8.3f %8.3f\n", s, runs[s], bw[s], mw[s], bc[s], mc[s], bw[ref] / bw[s], bc[ref] / bc[s]
    }' "$WORK/runs.tsv"
  dirty=$(awk -F'\t' 'NR > 1 && $5 != ""' "$WORK/runs.tsv" | wc -l | tr -d ' ')
  echo
  echo "dirty runs left out: $dirty"
} | tee "$WORK/summary.txt"
