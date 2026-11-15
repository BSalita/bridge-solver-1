# Divergence fixtures

> **Superseded as the lock-step check (2026-11).** These fixtures, and
> `first-divergence.sh`, compare against the reference as of `75b4619`
> (2026-01-31). The port has since been brought up to upstream `47d30f3`
> (2026-10-01), which searches a different tree, so they no longer report
> `none`. Lock-step is now checked deal by deal against every upstream commit
> with `scripts/upstream-lockstep.sh COMMIT` (and `--freak COMMIT` for the
> four freak deals as full tables), from `upstream-nodes.tsv` and
> `upstream-freak-nodes.tsv`. Rebuilding the instrumented reference at
> `47d30f3` would bring these back; until then they are a record of how
> lock-step was first reached.

Seven positions where our search does not follow the C++ reference
(`macroxue/bridge-solver`) node for node, each one reduced to a single search
from a cold cache.

They exist because the equivalence recorded in `bench/results/cache-fix.md` —
"we walk the same path, node for node" — was established over 194 cells
truncated at a thousand nodes each. It does not hold on full solves. We search
1.038x the reference's nodes for 1.162x its wall clock, so roughly a quarter of
the remaining gap is extra work rather than slower work.

## Running them

```bash
XRAY=~/src/bridge-solver-xray/xray/solver-xray \
  fixtures/divergence/first-divergence.sh
```

Each row reports where the two traces first differ, and whether that matches
what `manifest.tsv` records. `FIXED (no divergence)` means a change closed it;
`MOVED` means it changed shape and the trace is worth re-reading.

Build the reference's instrumented solver with:

```bash
clang++ -std=c++17 -O3 -o solver-xray xray/bridge-solver.cc
```

## Provenance of the deals

The seven deals come from `1k_deals` in `macroxue/bridge-solver`, whose code is
GPL-2.0 while this repo is MIT OR Apache-2.0. That is not a conflict here: a
bridge deal is a random arrangement of cards, not authorship, and carries no
copyright of its own. (Analysis *of* a deal — commentary, a bidding sequence, a
play write-up — can be; the deal cannot.) They are committed here verbatim, with
the trump and opening leader appended.

## What each position isolates

Every row is one search: one deal, one trump, one opening leader, cold caches,
and the MTD(f) guess straight from `GuessTricks`. Nothing precedes it, so there
is no accumulated cache state or cell ordering to reproduce — the divergence is
a property of that position alone.

**`FAST_TRICKS` (deal.72, .12, .10, .4)** — our fast-trick estimate is lower
than the reference's. On `deal.72` it differs on 7.1% of calls and is never
high: twenty cases of -1 and four of -2. `min(fast_tricks, remaining)` masks it
most of the time, which is why the traces survive another hundred records before
losing alignment.

**`MOVE_ORDER_AFTER` (deal.1, .134)** — identical playable set, identical
cutoff cards, different order out. On `deal.1` at depth 28 the reference orders
`[CT C2 H2 D9 C9 C6]` and we put `D9` first, with everything else keeping its
relative position, so we promote `D9` into a higher-priority bucket than
`order_leads` should.

**`PATTERN_HIT` (deal.38)** — at depth 16 the reference reports
`bounds=[0,7] adj_upper=10 UPPER_CUT` and prunes; we miss and go on to compute
fast tricks and store. Every preceding trace line is identical, which reads as
a narrower lookup on our side — and is not. Bisecting the cache digests
(`-C 1180:1195`) pins the real divergence to iteration 1189, 445 earlier, where
the reference's pattern cache takes a fresh slot (73 → 74 entries) and ours
overwrites an existing one (still 73, different digest).

`PatternCache` is direct-mapped: `lookup` checks one slot and gives up,
`get_or_create` evicts whatever is in that slot. The reference linear-probes on
both paths, never evicts, and resizes at 75% load. Every collision costs us an
entry it keeps, which is why the miss appears later and why the loss compounds
as the table fills.

All three fail safely. A low fast-trick estimate, a worse move order and a
missed pattern hit can only fail to prune — none can return a wrong answer,
which is why the correctness suite stays green while node counts do not.

## Two indicators, and why you want both

`-C <n>` on both solvers emits a content digest of the cutoff and pattern caches
every `n` iterations. The trace shows the first difference in what was
*computed*; the digest shows the first difference in what was *kept*, and
neither comes first reliably:

| deal | trace differs at | caches differ at |
|---|---|---|
| `deal.134` | iter 9 | iter 20 |
| `deal.1` | iter 29 | iter 40 |
| `deal.72` | iter 28 | iter 1170 |
| `deal.38` | iter 1635 | **iter 1190** |

`deal.72`'s trace fires at 28 on a fast-trick estimate that both sides then cap
to the same number — visible, but nothing behavioural has happened, and the
caches agree for another eleven hundred iterations. `deal.38` is the reverse,
and it corrected a wrong conclusion: the pattern-cache miss at 1635 looked like
a narrower lookup because every preceding trace line matched, when in fact the
caches had drifted apart 445 iterations earlier.

The digest is comparable across the two implementations because it XORs across
live slots — table size, hash function and probe order all differ and must not
matter — and walks each pattern tree pre-order, since the tree's shape is what
decides whether a lookup matches. It is splitmix64 on both sides, and the
"no card" sentinel is canonicalised (255 here, `TOTAL_CARDS` there).

## Determinism

Both solvers are deterministic here, and this was checked rather than assumed:
three runs of each of the seven positions, on both sides, gave byte-identical
traces, and the first-divergence line was identical every time.

The mechanisms themselves are also independent of processing order.
`fast_tricks_from_seat` and `order_leads` are pure functions of the position —
hands, trump, seat to play — and read no cache, so a position diverges the same
way however the search reached it and whatever order the twenty cells of a table
are solved in.

What *is* order-dependent is the cost. Caches are shared across a strain and the
MTD(f) seed chains from cell to cell, so the same divergence is worth wildly
different numbers of nodes depending on where it lands. Solving declarers in the
reference's order (S, N, W, E — partners adjacent) rather than N, E, S, W took
`deal.10`'s whole-table count from 1.36x to 1.02x without touching any
estimator.
