#!/bin/bash
#
# Alternate this port and Bob Salita's C++ on the 200 lock-step deals,
# single-threaded, each as one process solving all 200, both measured the same
# way: /usr/bin/time -l, which reports wall, instructions retired and cycles.
#
#   bench/comparison/bsalita-ab.sh <dir from build-bsalita.sh> [ROUNDS]
#
# Prints one TSV row per run. Rounds alternate which solver goes first. Gate:
# a run starts only when no other process is above 80% CPU (waiting up to
# 120 s), and is marked dirty if one is seen while it runs.
set -uo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"

BOBDIR=${1:?usage: bsalita-ab.sh <dir from build-bsalita.sh> [ROUNDS]}
ROUNDS=${2:-8}
P=fixtures/divergence/lockstep-200.pbn
OURS=(./target/release/solver-bench cost --runs 1 "$P")
BOB=("$BOBDIR/bench200" "$P")
[ -x "${OURS[0]}" ] || { echo "build it: ./dev-build.sh --ci build --release --features bench --bin solver-bench" >&2; exit 1; }
[ -x "${BOB[0]}" ] || { echo "no bench200 in $BOBDIR" >&2; exit 1; }
out=$(mktemp); trap 'rm -f "$out"' EXIT

busy() {  # the busiest other process above 80% CPU, or nothing
  ps -Ao pcpu=,comm= | awk '$1>80 && $2 !~ /solver-bench|bench200/ {print $1, $2; exit}'
}
one() {  # one <label> <command...>
  local label=$1 waited=0 hit="" pid
  shift
  while [ -n "$(busy)" ] && [ $waited -lt 120 ]; do sleep 2; waited=$((waited + 2)); done
  /usr/bin/time -l "$@" >/dev/null 2>"$out" &
  pid=$!
  while kill -0 $pid 2>/dev/null; do
    [ -z "$hit" ] && hit=$(busy)
    sleep 0.5
  done
  wait $pid
  printf '%s\t%s\t%s\t%s\t%s\n' "$label" \
    "$(awk '/ real /{print $1}' "$out")" \
    "$(awk '/instructions retired/{print $1}' "$out")" \
    "$(awk '/cycles elapsed/{print $1}' "$out")" \
    "${hit:+dirty: $hit}"
}

printf 'solver\twall_s\tinstructions\tcycles\tnote\n'
for r in $(seq 1 "$ROUNDS"); do
  if [ $((r % 2)) -eq 1 ]; then one ours "${OURS[@]}"; one bob "${BOB[@]}"
  else one bob "${BOB[@]}"; one ours "${OURS[@]}"; fi
done
