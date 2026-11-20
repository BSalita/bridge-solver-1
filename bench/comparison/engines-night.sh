#!/bin/bash
# Overnight run of Bob Salita's `solver-bench engines` harness on this Mac,
# capped at half the machine (6 of 12 logical CPUs).
#
# Engines: bridge-solver 2.2.0 (inside Bob's harness, node-for-node checked),
# Bob's C++ (b12bb3f, its own PGO build.sh), DDS 2.9 (clang -O3 -flto).
#
# Jobs:
#   random 2,000 deals (Bob's gen-corpus.py, seed 2026) at 1 and 6 threads,
#     all three engines, ROUNDS rounds, best clean run kept;
#   Hanhong's 33 hard + long deals at 1 thread, ours and Bob's C++ only
#     (DDS 2.9 takes minutes per hard deal), one round.
#
# Waits until START_AT, then starts each run only when no other process is
# above BUSY_PCT CPU and the 1-minute load is below MAX_LOAD; a run disturbed
# by another busy process is thrown away and repeated. Stops at STOP_AT.
# Resumable: finished runs are skipped.
#
# Layout of the directory it runs in (target/engines-bench on 2026-11-19):
#   bs1/    Bob's harness fork (BSalita/bridge-solver-1, 3b5327f) with this
#           repo's v2.2.0 merged in; no solver file changed in the merge.
#           `cargo build --release -p solver-bench`, then check its port still
#           searches 183,796,287 nodes on fixtures/divergence/lockstep-200.pbn.
#   bob/    Bob's C++ at b12bb3f built by its own build.sh (PGO), giving
#           build/libdds-single.dylib.
#   dds29/  DDS 2.9 sources built with clang -O3 -flto into libdds29.dylib.
#   corpus/deals.pbn  Bob's gen-corpus.py with 2000 deals, seed 2026.
#   hard/hard.pbn     Hanhong Xue's hard_deals (19) and long_deals (14).
# Results go to out/ and summary.txt. See "Bob's 2,000 random deals" in
# bench/results/release-profile.md.
set -e
cd "$(dirname "$0")"
W=$PWD
START_AT=${START_AT:-01:00}
STOP_AT=${STOP_AT:-07:00}
ROUNDS=${ROUNDS:-3}
BUSY_PCT=${BUSY_PCT:-80}
MAX_LOAD=${MAX_LOAD:-4}
OUT=$W/out
mkdir -p "$OUT"
BENCH=$W/bs1/target/release/solver-bench
BOB="--dds-lib cpp-bob=$W/bob/build/libdds-single.dylib"
DDS="--dds-lib dds29=$W/dds29/libdds29.dylib"

log() { echo "$(date '+%F %T') $*"; }
epoch_at() { # next occurrence of HH:MM
  local t; t=$(date -j -f '%Y-%m-%d %H:%M' "$(date +%F) $1" +%s)
  [ "$t" -le "$(date +%s)" ] && t=$((t + 86400))
  echo "$t"
}
notify() { zsh -i -c "pushover \"$1\" \"Engines benchmark\"" >/dev/null 2>&1 || true; }

start=$(epoch_at "$START_AT")
if [ -z "$NOW" ]; then
  log "waiting until $START_AT"
  while [ "$(date +%s)" -lt "$start" ]; do sleep 60; done
fi
stop=$(epoch_at "$STOP_AT")
log "started; stopping by $STOP_AT"

OURS='solver-bench|run-night|caffeinate'
busiest() { ps -Ao pcpu=,comm= | grep -v -E "$OURS" | sort -rn | awk 'NR==1{print $1, $2}'; }
load1() { sysctl -n vm.loadavg | awk '{print $2}'; }
busy_now() {
  local pct; pct=$(busiest | awk '{print $1}')
  awk -v p="$pct" -v l="$(load1)" -v bp="$BUSY_PCT" -v ml="$MAX_LOAD" 'BEGIN{exit !(p>bp || l>ml)}'
}

# name|threads|pbn|engines
JOBS=()
for r in $(seq 1 "$ROUNDS"); do
  # Alternate the engine order between rounds.
  if [ $((r % 2)) = 1 ]; then E="$BOB $DDS"; else E="$DDS $BOB"; fi
  JOBS+=("random-t1-r$r|1|$W/corpus/deals.pbn|$E")
  JOBS+=("random-t6-r$r|6|$W/corpus/deals.pbn|$E")
done
JOBS+=("hard-t1-r1|1|$W/hard/hard.pbn|$BOB")

for job in "${JOBS[@]}"; do
  IFS='|' read -r name threads pbn engines <<<"$job"
  [ -f "$OUT/$name.json" ] && continue
  while :; do
    if [ "$(date +%s)" -gt "$stop" ]; then log "stop time reached before $name"; break 2; fi
    if busy_now; then log "waiting: busiest $(busiest), load $(load1)"; sleep 60; continue; fi
    log "start $name"
    # shellcheck disable=SC2086
    "$BENCH" engines --pbn "$pbn" --threads "$threads" --runs 1 $engines \
      --json "$OUT/$name.json.tmp" > "$OUT/$name.txt" 2>&1 &
    pid=$!
    intruder=""
    while kill -0 $pid 2>/dev/null; do
      b=$(busiest)
      if awk -v p="${b%% *}" -v bp="$BUSY_PCT" 'BEGIN{exit !(p>bp)}'; then intruder=$b; fi
      sleep 5
    done
    rc=0; wait $pid || rc=$?
    if [ -n "$intruder" ]; then
      log "discarded $name: $intruder was busy" | tee -a "$OUT/discarded.log"
      rm -f "$OUT/$name.json.tmp"; continue
    fi
    if [ $rc != 0 ] && ! grep -q 'engines disagree' "$OUT/$name.txt"; then
      log "harness failed on $name"; cat "$OUT/$name.txt"; notify "harness failed on $name"; exit 1
    fi
    # A result mismatch still leaves valid timings: keep the run, record it.
    grep -E 'mismatched cells|agree on all' "$OUT/$name.txt" | sed "s/^/$name: /" >> "$OUT/agreement.log"
    mv "$OUT/$name.json.tmp" "$OUT/$name.json"
    log "done $name"
    break
  done
done

# Summary: per job type, each engine's best run, and every agreement line.
python3 -I - "$OUT" > "$W/summary.txt" <<'PY'
import json, os, re, sys, collections
out = sys.argv[1]
best = collections.defaultdict(dict)
for f in sorted(os.listdir(out)):
    if not f.endswith('.json'): continue
    kind = re.sub(r'-r\d+\.json$', '', f)
    d = json.load(open(os.path.join(out, f)))
    def walk(x):
        if isinstance(x, dict):
            if 'engine' in x and any(k in x for k in ('ms', 'time_ms', 'elapsed_ms')):
                yield x
            for v in x.values(): yield from walk(v)
        elif isinstance(x, list):
            for v in x: yield from walk(v)
    for row in walk(d):
        ms = row.get('ms', row.get('time_ms', row.get('elapsed_ms')))
        e = row['engine']
        if e not in best[kind] or ms < best[kind][e]:
            best[kind][e] = ms
for kind, rows in sorted(best.items()):
    print(f"== {kind} (best run)")
    base = next((v for k, v in rows.items() if k.startswith('bridge-solver-rs')), None)
    for e, ms in sorted(rows.items(), key=lambda kv: kv[1]):
        rel = f"  {base / ms:.2f}x vs bridge-solver-rs" if base else ""
        print(f"  {e:28s} {ms / 1000:9.2f} s{rel}")
print()
print("== agreement (every run)")
p = os.path.join(out, 'agreement.log')
print(open(p).read() if os.path.exists(p) else "(none)")
PY
log "summary written"
cat "$W/summary.txt"
notify "Engines benchmark finished: see target/engines-bench/summary.txt"
