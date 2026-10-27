#!/bin/sh
# Regenerate web/src/lib/fixtures/claim-bench-v1.json: the native results the
# claim benchmark page checks the browser build against.
#
# Run after any change to the claim search. Verdicts and node counts are
# deterministic, so a stale fixture makes every browser report its results as
# wrong; `claim_bench::tests::matches_the_web_fixture` fails until this is run.
set -eu
cd "$(dirname "$0")/.."
./dev-build.sh --ci build --release --features cli --bin bridge-claim
target/release/bridge-claim bench --preset full --json | python3 -c '
import json, sys
d = json.load(sys.stdin)
out = {
    "_comment": "Native bridge-claim results for the full claim-benchmark preset: per claim, "
    "the verdict letter and nodes searched. Verdicts and nodes are deterministic, so the "
    "browser build must reproduce them exactly. Regenerate with "
    "scripts/claim-bench-fixture.sh whenever the claim search changes.",
    "version": d["version"],
    "budget": d["budget"],
    "rows": {
        "%s/%d" % (r["workload"], r["size"]): {"verdicts": r["verdicts"], "nodes": r["nodes"]}
        for r in d["rows"]
    },
}
json.dump(out, open("web/src/lib/fixtures/claim-bench-v1.json", "w"), separators=(",", ":"))
print("wrote %d rows" % len(out["rows"]))
'
