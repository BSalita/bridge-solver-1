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
# Prints the deals that differ, first divergence first, and exits nonzero if
# any do.
set -euo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
COMMIT=${1:?usage: upstream-lockstep.sh COMMIT}
TSV=fixtures/divergence/upstream-nodes.tsv
col=$(head -1 "$TSV" | tr '\t' '\n' | grep -n -x "$COMMIT" | cut -d: -f1 || true)
[ -n "$col" ] || { echo "no column $COMMIT in $TSV" >&2; exit 2; }

ours=$(mktemp); trap 'rm -f "$ours"' EXIT
./dev-build.sh --ci run --release --features bench --bin solver-bench -- \
  nodes fixtures/divergence/lockstep-200.pbn 2>/dev/null \
  | awk '$1 ~ /^[0-9]+$/ && NF == 2' > "$ours"

awk -v col="$col" -v commit="$COMMIT" '
  NR == FNR { if (FNR > 1 && $1 != "total") want[$1] = $col; next }
  { got[$1] = $2; total += $2; wtotal += want[$1]
    if ($2 != want[$1]) { bad++; printf "deal %3d: ours %d, upstream %d (%+d)\n", $1, $2, want[$1], $2 - want[$1] } }
  END {
    printf "%s: ours %d, upstream %d, %d of 200 deals differ\n", commit, total, wtotal, bad
    exit bad > 0
  }' "$TSV" "$ours"
