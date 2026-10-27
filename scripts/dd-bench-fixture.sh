#!/bin/sh
# Regenerate web/src/lib/fixtures/dd-bench-v1.json: the native tables and node
# counts the analysis benchmark page checks the browser build against.
#
# Run after any change to the search. Tricks and node counts are deterministic,
# so a stale fixture makes every browser report its results as wrong;
# `dd_bench::tests::matches_the_web_fixture` fails until this is run.
set -eu
cd "$(dirname "$0")/.."
./dev-build.sh --ci build --release --features cli --bin bridge-solver
target/release/bridge-solver --bench full --json | python3 -c '
import json, sys
d = json.load(sys.stdin)
out = {
    "_comment": "Native bridge-solver results for the analysis benchmark: per deal, the "
    "20-cell double-dummy table as hex digits (rows N E S W, columns C D H S NT) and the "
    "nodes searched. Both are deterministic, so the browser build must reproduce them "
    "exactly. Regenerate with scripts/dd-bench-fixture.sh whenever the search changes.",
    "version": d["version"],
    "tricks": d["tricks"],
    "nodes": d["nodes"],
}
json.dump(out, open("web/src/lib/fixtures/dd-bench-v1.json", "w"), separators=(",", ":"))
print("wrote %d deals, %d nodes" % (len(out["nodes"]), sum(out["nodes"])))
'
