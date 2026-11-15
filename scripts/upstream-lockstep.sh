#!/bin/bash
#
# Is the port in lock-step with upstream at COMMIT, deal by deal?
#
#   scripts/upstream-lockstep.sh COMMIT
#
# COMMIT is a column of fixtures/divergence/upstream-nodes.tsv: upstream
# macroxue/bridge-solver at each commit that touched solver.cc from 75b4619
# (the 2026-01-31 state) on, with the nodes it searches on each deal of
# lockstep-200.pbn. A node is one EvaluatePlayableCards call, the port's
# definition; the counts come from upstream built with a counter beside its
# own num_visits, one process per deal (see "Catching up with upstream" in
# bench/results/release-profile.md).
#
# COMMIT may also name a column of bsalita-nodes.tsv: Bob Salita's faster search
# (BSalita/bridge-solver b12bb3f, on top of upstream 630705d), split into its
# seven tree-changing features and enabled one at a time -- bob0-base (none,
# equal to 47d30f3) through bob7-second (all of them). Nodes there are counted
# at Bob's `++nodes`, at the top of EvaluatePlayableCards, which is the port's
# definition too; see "Bob Salita's faster search" in
# bench/results/release-profile.md.
#
# Prints the deals that differ, first divergence first, and exits nonzero if
# any do.
#
#   scripts/upstream-lockstep.sh --freak COMMIT
#
# does the same for the four freak fixtures solved as full 20-cell tables
# (freak-tables.pbn), against upstream-freak-nodes.tsv, which has a column only
# for the commits that change how freak deals are handled. None of the 200
# deals has four voids, so code that keys on that is checked only here. A
# minute or two: these are the slowest deals there are.
set -euo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
TSV=fixtures/divergence/upstream-nodes.tsv
PBN=fixtures/divergence/lockstep-200.pbn
if [ "${1:-}" = --freak ]; then
  TSV=fixtures/divergence/upstream-freak-nodes.tsv
  PBN=fixtures/divergence/freak-tables.pbn
  shift
fi
COMMIT=${1:?usage: upstream-lockstep.sh [--freak] COMMIT}
NDEALS=$(grep -c '^N:' "$PBN")
find_col() { head -1 "$1" | tr '\t' '\n' | grep -n -x "$COMMIT" | cut -d: -f1 || true; }
col=$(find_col "$TSV")
if [ -z "$col" ]; then
  case $TSV in
    fixtures/divergence/upstream-nodes.tsv) TSV=fixtures/divergence/bsalita-nodes.tsv ;;
    fixtures/divergence/upstream-freak-nodes.tsv) TSV=fixtures/divergence/bsalita-freak-nodes.tsv ;;
  esac
  col=$(find_col "$TSV")
fi
[ -n "$col" ] || { echo "no column $COMMIT in $TSV" >&2; exit 2; }

ours=$(mktemp); trap 'rm -f "$ours"' EXIT
./dev-build.sh --ci run --release --features bench --bin solver-bench -- \
  nodes "$PBN" 2>/dev/null \
  | awk '$1 ~ /^[0-9]+$/ && NF == 2' > "$ours"

awk -v col="$col" -v commit="$COMMIT" -v ndeals="$NDEALS" '
  NR == FNR { if (FNR > 1 && $1 != "total") want[$1] = $col; next }
  { got[$1] = $2; total += $2; wtotal += want[$1]
    if ($2 != want[$1]) { bad++; printf "deal %3d: ours %d, upstream %d (%+d)\n", $1, $2, want[$1], $2 - want[$1] } }
  END {
    printf "%s: ours %d, upstream %d, %d of %d deals differ\n", commit, total, wtotal, bad, ndeals
    exit bad > 0
  }' "$TSV" "$ours"
