# The release profile: LTO and one codegen unit

Measured 2026-09-01 on an Apple M4 Pro (12 logical, 8P+4E), corpus `corpus-v1`,
best of 5. Supersedes `cache-fix.json` for per-board costs.

`cache-fix.md` closed the algorithmic gap and left one number outstanding: the
search tree was the same size as the C++ reference's, but each node cost
**1.120x** as much. That residue was not a Rust tax. It was Cargo's defaults.

## What was wrong

`Cargo.toml` had no `[profile.release]` section, so the release build took
Cargo's defaults: **`codegen-units = 16` and no LTO**. The crate was split into
sixteen units with no inlining across the splits, while the reference is built
`-O3` from a single translation unit. The search is one hot call graph spread
over `search.rs`, `bridge_solver.rs`, `cards.rs` and `pattern.rs`, so the split
falls exactly across the hot path.

```toml
[profile.release]
lto = "fat"
codegen-units = 1
```

## What it bought

Controlled A/B, both binaries built and run in the same session minutes apart,
best of 3 over the full corpus:

```
overall wall: 0.923x (-7.7%)
overall cpu : 0.923x (-7.7%)
```

Every one of the ten boards improved, in a tight band from -7.1% to -8.9%. That
consistency is what makes it attributable: a codegen change should move all
boards by roughly the same fraction, and it does.

Against the last recorded result (`cache-fix.json`), on a quiet machine:

```
overall wall: 0.914x (-8.6%)
overall cpu : 0.915x (-8.5%)
```

## How current is the reference we match?

The lock-step below is against `macroxue/bridge-solver` at 2026-01-31. Upstream
is **49 commits ahead**, to 2026-08-31, **27 of them touching `solver.cc`** --
including several in exactly the code this work changed: `Bubble up broader
patterns`, `Tune lead ordering`, `Simpler cutoff cache index`, `ShapeEntry holds
patterns of four seats`.

Built from `origin/master` and run over the same 200 deals:

| | tables | nodes | wall |
|---|---|---|---|
| upstream Jan (what we match) | — | 296,689,028 | 21,940 ms |
| upstream Aug | **identical on all 200** | 298,896,515 (**+0.74%**) | 20,730 ms (**-5.5%**) |

Three things follow, and the middle one is the surprise.

**No correctness has been missed.** Seven months of upstream work produces the
same twenty-entry table on every one of the 200 deals. `bac53be`, the one commit
that reads like a correctness fix, guards `all_cards.Suit(trump)` against
`trump == NOTRUMP`, where `Suit(4)` extracts a range that is empty in a 52-bit
mask anyway -- latent UB, not behaviour. Ours is structurally the same and
equally benign: `mask_of(NOTRUMP)` is `0x1FFF << 52`, which no card bit reaches.

**Upstream's search got slightly bigger, not smaller.** Every one of the 200
deals changed node count, and the total rose 0.74%. So matching current upstream
would mean adopting a marginally *worse* tree.

**Its gains are implementation, and they transfer without matching the tree.**
A free-list pool for `Vector<T>`'s backing storage (`610e9da`), a single 64-bit
cache index (`8a01aa8`), a narrower hash tag (`448a1f5`), skipping a re-hash
between lookup and update (`24ecd16`). That first one is aimed squarely at what
this port's remaining 13% per-node gap looks like.

### Which commits actually changed the tree

Upstream's own history bisects by node count without porting anything: build
each commit that touches `solver.cc`, measure, and the deltas fall out. Deals
1-40, full table, in `upstream-node-sweep.txt`. **Four commits of twenty-seven
moved the search. The other twenty-three did not.**

| commit | delta | |
|---|---|---|
| `b83aa4b` Tune lead ordering | +291,656 | +0.50% |
| `14ab4c7` Clear caches for ultra-freakish hands | +99,448 | +0.17% |
| `205aaca` Bubble up broader patterns | +112,847 | +0.19% |
| `990a665` Simpler cutoff cache index, without seat | **-563,418** | -0.96% |
| `0216224` Add my suit to cutoff index when following suit | **+563,418** | exactly undone |

The last pair is the interesting one: dropping the seat from the cutoff index
made the search *smaller* -- a coarser key shares entries across positions, and
since the cutoff cache only supplies a move-ordering hint, sharing it loosely is
free -- and adding the suit back the next day restored the count to the digit.
Net zero across the two.

So the +0.86% is three deliberate changes and nothing accidental.

`14ab4c7` repays a closer look, because its message names the half that costs
nothing. It bundles two unrelated changes: a cache reset between cells when a
deal holds four or more voids, and a narrowing of two lead-classification tests
from `partnership_cards` (both hands, every suit) to `our_suits` (this suit,
from the *playable* cards). **No deal in 1-40 has four voids -- none has even
three -- so the reset never fires there, and the whole +99,448 is the lead
change.** It is not freak-specific at all. Porting the two separately would let
the reset in for free and put the lead change on its own merits.

### The freak work is about the memory tail

Isolating the two commits on `deals/freak/deal.1`, a full twenty-cell solve:

| build | time | peak RSS |
|---|---|---|
| before `14ab4c7` | 36.5 s | **1,220 MB** |
| `14ab4c7`, clear caches when 4+ voids | 43.2 s | **559 MB** |
| `189f86b`, stricter reset condition | 31.3 s | 1,114 MB |

So it is neither correctness nor throughput: a freak deal can take **over a
gigabyte**, the reset more than halves that at a 19% cost in time, and the
follow-up relaxes the trigger to buy the time back while keeping a little of the
saving. Ordinary deals pay the 0.17% and never reach the reset, because none of
them holds four voids.

**We have a worse version of the same problem.** Same deal, same twenty cells,
and -- since we are in lock-step -- the same tree:

| | peak RSS | time |
|---|---|---|
| C++ (Jan, what we match) | 1,107 MB | 33.3 s |
| ours | **1,613 MB** | 39.7 s |

**1.46x the memory for an identical search.** That is the same root cause as the
1.127x per-node time, seen from a different angle: a `Pattern` holds a
`Vec<Pattern>` -- pointer, length and capacity, each child separately
heap-allocated -- where the reference has a packed custom `Vector` drawing from
a pool. Memory magnifies it because the pattern cache is where the port's extra
weight accumulates.

That makes `610e9da`, the free-list pool for `Vector<T>`'s backing storage, the
clear first port: it is in the twenty-three that do not touch the tree, and it
addresses the one structural difference that both remaining measurements point
at.

And the freak deals themselves settle the wider question: the August build
returns the same twenty entries as January's on all four, as it does on the 200
random deals. Across everything that can be tested here, seven months of
upstream work changed no answer. Its timing on them is mixed rather than
uniformly better -- `deals/freak/deal.1` is 14% faster, `deal.2` 39% *slower*
and using more memory.

**Everything else is free.** All twenty-three no-change commits include every
one of the speed-ups: the free-list pool for `Vector<T>` (`610e9da`), the
narrower hash tag (`448a1f5`), skipping the re-hash between lookup and update
(`24ecd16`), the single 64-bit cache index (`8a01aa8`), `ShapeEntry` holding
four seats (`bf4a521`), the SSE4.1 vectorisation (`67d59d5`, x86 only).
Upstream's 5.5% is available **without touching the tree at all**.

So the order is not a rebase: port the twenty-three, which lock-step verifies by
construction, and take the four one at a time on their merits.

Two smaller notes. The `Have(1)` implicit conversion documented below is
**still present** in current upstream, so matching it remains right. And
upstream **relicensed to MIT OR Apache-2.0** in August (`dc2d4df`), the same
pairing as this repo, which retires the licence question that shaped
`fixtures/divergence`.

Against current upstream rather than January's, our margin is 24,731 ms against
20,730 ms -- **1.193x**.

## The pooled pattern vector

Porting the reference's `Vector<T>` and its free-list pool (`610e9da`), the
first of the twenty-three commits that do not touch the tree. Measured first,
which changed the design: on `deals/freak/deal.1` the 589 MB gap was 36 MB of
hash table, 923 MB of `Pattern` structs and roughly 737 MB of allocator slack
and headers, so both halves of the reference's answer were needed rather than
the pool alone.

`PatternVec` is a pointer and a `u32` length and capacity where `Vec` is three
words, taking `Pattern` from 64 bytes to the reference's exact 56 -- 115 MB over
15.1M nodes -- and its blocks come from per-power-of-two free lists refilled in
8 KB slabs, so a block is exactly its capacity and carries no header.

| | Pattern | peak RSS on freak.1 | that deal | 200 deals |
|---|---|---|---|---|
| before | 64 B | 1,696 MB | 34.70 s | 24,731 ms |
| after | **56 B** | **1,361 MB** | **33.74 s** | **23,480 ms** |
| C++ reference | 56 B | 1,107 MB | 33.3 s | 21,630 ms |

**Per-node cost 1.127x → 1.086x**, and 57% of the memory gap closed. Node counts
are still exactly 296,689,028 and all twelve fixtures still report `none`, which
is what makes the timing comparable at all.

What is left of the memory gap -- about 254 MB -- is pool retention rather than
per-node waste, and blocks are recycled rather than freed, so a thread holds its
peak for its lifetime. `drain_pool()` is public for that boundary and is worth
wiring into the wasm build, where holding 1.3 GB after one hard deal on a page
that stays open would be fatal.

## Lock-step, and what the timing then means

All 200 deals of the reference's own corpus search the same tree: **296,689,028
nodes against 296,689,028, exactly**. Eight fixtures in `fixtures/divergence`
hold that in place, identical in trace and cache digests to 100,000 iterations,
and `first-divergence.sh` fails if any of them moves.

That is what makes the timing below a measurement rather than a mixture. Three
interleaved rounds, best of three, same 200 deals, single-threaded:

| | best of 3 | |
|---|---|---|
| C++ reference | 21,940 ms | — |
| ours | 24,731 ms | **1.127x the reference**, 1.187x DDS |
| DDS 2.9 | 20,836 ms | reference is 1.053x DDS |

**The 1.127x is pure per-node cost.** Identical trees, identical node counts, so
there is nothing else left in it: no search-size difference, no pruning
difference, no cache-hit difference. Rounds agreed to within 1.7%.

It also matches the decomposition made before lock-step -- 1.162x wall over
1.038x nodes implied 1.120x per node -- which is a decent check that the
decomposition was sound.

Worth noting separately: on this machine the C++ reference is **5% slower than
DDS**, against its README's claim of being 1.28x faster. See the note on that
claim below.

### What is left

One thing, and it is now the only thing: a node costs us about 13% more than it
costs the reference. The most likely culprit is entry weight -- a `Pattern`
holds a `Vec<Pattern>` where the reference has a packed, custom `Vector`, and
`ShapeEntry` is correspondingly fatter, so every pattern-cache probe touches
more memory. The 39 `panic_bounds_check` sites in the two hottest functions are
the other lead.

## Against the C++ reference, measured## Against the C++ reference, measured

The 1.120x per-node figure in `cache-fix.md` was the last soft number here, and
the arithmetic that turned it into "~1.03x after the release profile" was
wrong. Measured directly, on the upstream project's own 200 random deals
(`1k_deals/deal.1` to `deal.200`, converted to PBN and cross-checked cell by
cell against the C++ solver's own output), single-threaded, best of 3:

| solver | 200 deals | vs ours |
|---|---|---|
| ours | 23.96 s | — |
| C++ reference (macroxue) | 20.62 s | **1.16x** |
| DDS 2.9 | 20.03 s | **1.20x** |

The build, recorded so the number has provenance — the makefile's own PGO path
is GCC-specific (`.gcda`) and cannot run under Apple clang:

```
clang++ -std=c++17 -O3 -march=native -o solver solver.cc
```

The C++ solver is single-threaded and solves one deal per process, so it is
timed per invocation from its own internal clock. Startup was measured at
2.1 ms (`-o`, read-and-exit), 1.6% of a deal, and external wall minus startup
agreed with the self-reported sum to within 1% across two passes.

**PGO turned out not to matter.** Built the clang way — `-fprofile-generate`,
train on `hard_deals/deal.8` as the makefile does, `llvm-profdata merge`,
`-fprofile-use` — it produced identical tables and 20.96 s against 20.62 s, with
the order reversing on the second pass. Within noise. The earlier speculation
that PGO explained part of the gap is dead, and the plain `-O3` build above is
representative.

### Two conclusions

**We are 1.17x the C++ original, not 1.03x.** The old figure was ns-per-node
over 194 cells truncated at 1000 nodes each — a slice thin enough to be mostly
cache-cold startup, and it does not predict whole-table cost. This number is
whole tables on random deals, which is the workload that matters.

**The upstream faster-than-DDS claim does not reproduce on this machine.**
Here the C++ solver and DDS are within 2% of each other (1.02x). That claim was
measured on an AMD Ryzen 7 5800H, but the comparison scripts were never
committed (one commit added the README, two PNGs and a 50,000-line log, and
nothing in that repo invokes DDS), and no DDS version or build flags were
recorded, so it cannot be checked directly. The README now says 1.36x, with an
Aug-2026 note claiming 1.5x; the 1.28x below is the figure their published log
supports.

**The x86 reading was backwards, and is too small anyway.** An earlier version
of this paragraph said "DDS carries x86-specific paths". It does not: DDS 2.9's
55 source files contain no `immintrin`, no `_pext`/`_pdep`, no `__m128`/`__m256`,
no popcount intrinsic and no `__x86_64__` guard. It is portable C++ and gains
nothing from being on a Ryzen. **The solver is the one with the x86 fast path** —
`PackBits`/`UnpackBits` are `_pext_u64`/`_pdep_u64` under `#ifdef __BMI2__` and a
bit-at-a-time loop otherwise, and its makefile adds `-mbmi2` by grepping
`/proc/cpuinfo`, so a Linux Zen 3 build gets the instructions and an aarch64
build cannot. Zen 3 is also the generation where AMD made PEXT fast rather than
microcoded, so the 5800H is the best case for it. See "How much is the missing
PEXT actually worth" below for the measurement: about 5%, not 36%.

Their published log does reproduce *internally*: parsing all 5000 deals gives a
geometric mean of 1.24x and a median of 1.25x against a claimed 1.28x, and the
advantage grows with difficulty — 1.23x at 0.05-0.10 s rising to 1.53x above
half a second. A least-squares fit gives `dds = -32.5 ms + 1.60 x solver`, so a
fixed per-invocation overhead inflating DDS is ruled out: the intercept is
negative. Whatever produced their result, it was not measurement overhead.

### The gap decomposes: 3.8% extra search, 12% per node

`cache-fix.md` established that we walk the reference's tree node-for-node.
That was measured over 194 cells truncated at a thousand nodes each, and **it
does not hold on full solves**. Counting nodes on the same 200 deals -- both
counters increment at the top of `EvaluatePlayableCards`, so they count the
same event -- gives:

| | ours / C++ |
|---|---|
| nodes searched | **1.038x** |
| wall clock | **1.162x** |
| implied ns per node | **1.120x** |

Not one of the 200 deals matches node-for-node. The per-deal ratio runs from
0.699x to 1.384x with a median of 1.021x, and we search *fewer* nodes than the
reference on 70 of them. This is not a missing prune -- that would be one-sided
-- it is small divergences in move ordering and cache state that compound
differently on different deals.

They compound with size, which is the part that matters:

| C++ nodes | deals | ours / C++ |
|---|---|---|
| < 300k | 23 | 1.019x |
| 300k - 1M | 70 | 1.025x |
| 1M - 3M | 84 | 1.026x |
| 3M - 10M | 23 | **1.083x** |

That lines up with the two other places the hard deals stand out: our per-board
spread against DDS is worst on the corpus's two most expensive boards, and the
reference's own advantage over DDS widens on hard deals. Whatever diverges,
it diverges more the longer the search runs.

### One cause found and fixed: declarer order

The reference solves a strain's four cells in lead order W, E, N, S -- that is,
declarers **S, N, W, E**, partners adjacent. We solved them N, E, S, W,
alternating sides.

That is not cosmetic, because the caches are shared across a strain and the
MTD(f) seed chains from one cell to the next: `seed_from` hands the next search
`ns_tricks + 1`, so ordering partners adjacently means each seed comes from a
cell whose NS trick count is usually the same or within one. Alternating sides
means every second seed is derived from the opponents declaring, which is a
poor guess and costs MTD(f) iterations.

Matching the reference's order took 310.4M nodes to 307.9M -- **0.8% fewer, for
a two-line change**, with all ten corpus tables unchanged. It is the ordering
the reference chose on purpose.

### The rest is chaotic, not systematic

Per-cell counts over the first 50 deals (`solver-bench nodes --per-cell` against
the reference's `-S1` output, which prints one block per cell):

| strain | C++ nodes | ours | ratio |
|---|---|---|---|
| NT | 13,561,599 | 13,745,262 | 1.014x |
| spades | 16,127,483 | 18,942,747 | 1.175x |
| hearts | 11,613,030 | 12,023,012 | 1.035x |
| diamonds | 16,935,775 | 17,305,948 | 1.022x |
| clubs | 14,232,095 | 14,534,830 | 1.021x |

Spades looks like a systematic outlier and is not: `deal.8` alone supplies about
three quarters of that excess.

The individual cells say what is really going on. The worst are 1.6x to 2.5x
(`deal.1` NT East, 34,811 against 85,732) and the best are far more extreme in
our favour -- `deal.7` NT East is 56,343 nodes for the reference and **2,267**
for us, 0.04x. Deviations that large in both directions, cancelling to 1.056x in
aggregate, are the signature of chaotic sensitivity: a tie broken differently in
move ordering changes which cutoff fires, and the subtree either collapses or
does not. It is not a missing prune and it is not strain-specific.

### Traced: the fast-tricks estimate under-counts

`deal.72`, notrump, declarer South is the cleanest possible target -- the first
cell of its strain in both, so cold caches and an identical `GuessTricks` seed,
78,779 nodes against our 151,962, while the deal's four suit strains agree
within 1%.

Both solvers trace that one cell with a five-line deal file (the deal, then `N`,
then `W`), which `solver-diag -f` and the instrumented C++ both read as an
explicit trump and lead:

```
solver-xray -f deal72.ntw -m0 -X 40000     # xray/bridge-solver.cc
solver-diag -f deal72.ntw -X 40000
```

The traces agree line for line -- once `remaining=[...]`, which the two format
differently, is normalised out -- until **line 208**:

```
C++ :  FAST_TRICKS: depth=20 seat=1 raw=9 capped=8 trump=4
ours:  FAST_TRICKS: depth=20 seat=1 raw=8 capped=8 trump=4
```

**Our fast-tricks estimate is low.** Over the aligned prefix it differs on 7.1%
of calls and is *never* high: 20 cases of -1 and 4 of -2. The cap hides it every
time in that prefix, which is why the searches stay identical for another
hundred-odd records, and then the traces lose structural alignment.

That is the whole shape of the problem, and it explains every symptom:

- **It cannot produce a wrong answer.** A low fast-trick count is a conservative
  estimate; it only fails to prune. Correctness tests pass, node counts do not.
- **It is chaotic.** `min(fast_tricks, remaining)` masks the difference most of
  the time. When it does not, a beta cut that the reference takes is missed and
  a whole subtree is searched — or the reverse, once cache contents diverge,
  which is why we are sometimes far *below* the reference (`deal.7` NT East,
  2,267 nodes against 56,343).
- **It compounds with depth**, matching the 1.019x → 1.083x trend by deal size.

The estimator is `fast_tricks_from_seat` in `search.rs`, against `FastTricks`
in the reference. Reading them side by side, the structure matches — the
`SuitFastTricks` cases, the `pd_entry` handling and the argument swap on the
second call are all faithful. Two differences are visible and neither is the
NT case above, so the cause is still to be found by instrumenting the per-suit
`my_tricks`/`pd_tricks` at this position:

1. `for card in all_suit.iter()` walks `self.hands.all_cards()`, the *live*
   remaining cards, where the reference walks `trick->all_cards`, the snapshot
   taken at the start of the trick. These agree at a trick boundary, which
   depth 20 is, so this is not the divergence above — but they part company
   mid-trick, and that is worth fixing regardless.
2. `max_suit_winners` starts at `self.num_tricks` where the reference uses
   `TOTAL_TRICKS`. Equal for a full deal, since `num_tricks` is the largest hand
   at construction, but not for `new_mid_trick` or `analyse_play`, and a smaller
   value truncates the suit harder and under-counts. Latent, and in the same
   direction as the symptom.

### An implicit conversion in the reference's lead ordering

`deal.134` diverged at iteration 9 and `deal.1` at 29, both on move ordering,
both with an identical playable set and no cutoff cards. The reference put a
whole suit in a higher lead bucket than we did.

The cause is one line of the reference:

```cpp
(pd_suit.Have(a) && lho_suit.Have(k) &&
 (pd_suit.Have(q) || our_suits.Have(Cards().Add(q).Add(j))))
```

`Have` takes an `int`. `Cards` declares a non-explicit `operator bool()`. So the
card *set* collapses to `true`, promotes to `1`, and the call is `Have(1)` --
"do we hold card index 1". The q/j test never happens, nor the j/t test in the
branch below it.

We had implemented the intent, which is better bridge and the wrong answer for
this purpose. Matching the reference takes `deal.134` to lock-step, moves
`deal.1`'s first divergence from iteration 29 to 6735, and moves node counts
from 0.9911x to 1.0011x. It is deterministic, not undefined -- the set is never
empty, so it is always exactly `Have(1)` -- and restoring the intended test is
a one-line change whenever matching stops being the goal.

`deal.1`'s remaining divergence is a different thing again: same score, same
cutoff, different `rank_winners` (`8130000002` against `1c8010000002`). A fifth
mechanism, and it feeds the pattern cache, which is where `deal.10` and
`deal.4` differ too.

### Three mechanisms, not one

Tracing the same cell (notrump, lead West) on sixteen deals, first 6,000 xray
iterations, and classifying where each trace first parts company:

| first divergence | deals | what differs |
|---|---|---|
| none in the traced prefix | 8 | — |
| `FAST_TRICKS` | 4 (`.4`, `.10`, `.12`, `.72`) | our raw estimate is lower |
| `MOVE_ORDER_AFTER` | 2 (`.1`, `.134`) | same playable set, different order |
| `PATTERN_HIT` | 1 (`.38`) | the reference hits, we miss |

**Move ordering** (`deal.1`, depth 28): identical `playable=[H2 D9 CT C9 C6 C2]`
and no cutoff card, and the reference orders `[CT C2 H2 D9 C9 C6]` while we put
`D9` first. Relative order is otherwise preserved, so we are promoting `D9` into
a higher-priority bucket than the reference does — a lead-classification
difference, in `order_leads`.

**Pattern cache** (`deal.38`, depth 16): the reference reports
`PATTERN_HIT ... bounds=[0,7] adj_upper=10 UPPER_CUT` and prunes. We miss, and
go on to compute fast tricks and store.

Every preceding trace line is identical, which first read as "both caches hold
the same entries, so our lookup is narrower". **That was wrong.** Digesting the
cache contents shows they had already parted company 445 iterations earlier, at
1190 against the miss at 1635. The miss is a consequence of cache drift the
trace could not see, not a lookup bug, and the thing to find is the write that
differs around iteration 1190.

### Digesting the caches, not just the trace

The trace records what the search *computed*; it says nothing about what the
caches *kept*. Those diverge at different moments, so `solver-diag -C <n>` (and
the same flag on the instrumented reference) emits a content digest of both
caches every `n` iterations. It XORs across live slots -- table size, hash
function and probe order all differ between the two and must not matter -- and
walks each pattern tree pre-order, because the tree's shape is what decides
whether `lookup` matches.

Neither indicator dominates:

Across all seven fixtures, after the probing fix (`cutoff_cache` and
`pattern_cache` are the only caches the search holds -- `plays[]` and `tricks[]`
are per-node scratch rebuilt from the path -- so the digest covers all of it):

| deal | trace differs at | caches differ at |
|---|---|---|
| `deal.134` | iter 9 | iter 20 |
| `deal.1` | iter 29 | iter 40 |
| `deal.72` | iter 28 | iter 4550 |
| `deal.12` | iter 1930 | iter 4260 |
| `deal.10` | iter 3716 | **iter 890** |
| `deal.4` | iter 4507 | **iter 1410** |
| `deal.38` | none | none |

An earlier version of this table covered four deals and concluded the trace
usually fires first. With all seven it is four to two, and where the caches win
they win by thousands of iterations. **`deal.10` and `deal.4` are labelled
`FAST_TRICKS` by the trace and that label is a downstream symptom**: bisecting
their cache divergence shows the pattern caches holding the *same number* of
entries with different digests, so it is the tree contents that differ, not the
slot allocation the probing fix addressed. A fourth mechanism, in
`Pattern::update`.

### Root cause of the pattern-cache divergence: no probing

Bisecting `deal.38` with `-C 1180:1195` pins it to a single step. The digests
are identical at iteration 1188 and differ at 1189:

```
c++   iter=1189 cutoff=59/eeb7d6b187ad13b2 pattern=74/63846b1f49f46558
ours  iter=1189 cutoff=59/eeb7d6b187ad13b2 pattern=73/6a0d824c48bed25b
```

The cutoff caches still agree. The reference's pattern cache goes from 73 live
entries to **74** — it took a fresh slot. Ours stays at **73** with a changed
digest — we wrote over an entry that was already there.

`PatternCache` does no probing at all. It is direct-mapped:

```rust
pub fn lookup(&mut self, shape: u64, seat_to_play: Seat) -> Option<&mut ShapeEntry> {
    let idx = self.index(hash);
    let entry = &mut self.entries[idx];
    if entry.hash == hash { Some(entry) } else { None }   // one slot, then give up
}

pub fn get_or_create(&mut self, shape: u64, seat_to_play: Seat) -> &mut ShapeEntry {
    let idx = self.index(hash);
    if self.entries[idx].hash != hash {
        self.entries[idx].reset(hash);                     // evict the incumbent
    }
    ...
}
```

The reference linear-probes on both paths, never evicts, and resizes at 75%
load:

```cpp
for (int d = 0; ; ++d) {
  Entry& entry = entries[(index + d) & (size - 1)];
  if (entry.hash == hash) return &entry;
  if (entry.hash == 0) { ++load_count; entry.Reset(hash); return &entry; }
}
```

So every hash collision costs us an entry the reference keeps. That is the
`PATTERN_HIT` miss at iteration 1635: we had evicted it.

It also explains the shape of the whole problem. Collisions get more frequent
as the table fills, so the loss grows with the length of the search — which is
the 1.019x to 1.083x trend by deal size, and why the reference's advantage over
DDS widens on hard deals while ours narrows. `CutoffCache` already probes and
resizes; the pattern cache is the one that did not get it.

### The goal is lock-step, not fewer nodes

Worth stating, because it changes what counts as progress: searching *fewer*
nodes than the reference is a divergence too, not a win. The target is the same
tree, node for node, on a full solve. Only once the two are in lock-step does a
timing difference mean anything, because only then is it measuring the same
work. Improvements on top of that come later and separately.

By that measure, node-count parity over the 200 deals now reads:

| | nodes | vs C++ |
|---|---|---|
| C++ reference | 296,689,028 | — |
| direct-mapped `PatternCache` | 307,943,054 | 1.0379x |
| + probing | 294,034,361 | 0.9911x |
| + matching `order_leads` | **297,018,481** | **1.0011x** |

### PatternCache probing closed the node gap without buying any time

`PatternCache` now linear-probes and resizes at 75% load, like `CutoffCache`
and like the reference. Over the same 200 deals:

| | nodes | vs C++ |
|---|---|---|
| C++ reference | 296,689,028 | — |
| ours, direct-mapped | 307,943,054 | 1.0379x |
| ours, probing | **294,034,361** | **0.9911x** |

**The node excess is gone** -- 4.5% fewer, and now marginally below the
reference. `deal.38` reports no divergence at all in 6,000 iterations and is
kept in the fixture set as a regression case.

**It bought no time.** Interleaved best-of-15, alternating binaries:
81.6 / 81.1 / 81.7 ms direct-mapped against 81.6 / 81.3 / 81.8 ms probing. Flat
to within 0.3%, and the 200-deal figure did not move either.

So the trade is exact: 4.5% fewer nodes for 4.5% more cost per node, and our
per-node figure against the reference goes from 1.120x to roughly 1.17x. The
likely reason is that our entry is far heavier than the reference's -- a
`Pattern` holds a `Vec<Pattern>` where the reference has a packed, custom
`Vector`, so a table that grows instead of evicting costs us more than it costs
them.

It is worth keeping anyway, on two grounds that are not speed: it is what the
reference does, and it makes node counts a trustworthy signal again for the
divergences that remain. But it does mean the whole remaining gap is now
per-node cost, and making `ShapeEntry` cheaper is the obvious next lever.

All three are safe and all three cost nodes. A low fast-trick estimate, a missed
pattern hit and a worse move order can only fail to prune, never return a wrong
answer, which is exactly why every correctness test passes while node counts do
not.

### Are they hand-specific? Is the order relevant?

**The mechanisms are deterministic properties of the code, not of particular
hands** — but which deals hit them varies enormously. Half the sample never
diverged in 6,000 iterations, and on `deal.72` the fast-tricks estimate differs
on 7.1% of calls against 1.3% on `deal.10` and 0.1% on `deal.4`.

**They are order-independent.** `fast_tricks_from_seat` and the lead ordering
are pure functions of the position — hands, trump, seat to play — and read no
cache, so a given position always diverges the same way however the search
reached it and whatever order the twenty cells are solved in. The traces
reproduce exactly, run to run.

**Their cost is not.** How many nodes a divergence ends up costing depends
entirely on the order, because caches are shared across a strain and the MTD(f)
seed chains from cell to cell. `deal.10` went from 1.36x to 1.02x purely from
solving declarers in the reference's order, with no estimator touched. So the
bugs are order-independent and the damage is not.

The ~12% per-node cost remains the larger half of the gap, and the 39
`panic_bounds_check` sites below are the only concrete lead on that half.

## Scaling, one to twelve threads

Board 4, equal work per thread, best of 5:

| threads | speedup | cores busy | vs `cache-fix` |
|---|---|---|---|
| 1 | 1.00x | 1.00 | 1.00x |
| 2 | 1.93x | 2.00 | 1.00x |
| 3 | 2.82x | 3.00 | 1.02x |
| 4 | 3.70x | 3.99 | 1.03x |
| 5 | 4.61x | 4.99 | 1.03x |
| 6 | 5.50x | 5.99 | 1.04x |
| 7 | 6.35x | 6.96 | 1.03x |
| 8 | 7.25x | 7.95 | 1.04x |
| 9 | 7.44x | 8.83 | 1.02x |
| 10 | 7.69x | 9.66 | — |
| 11 | 7.74x | 10.20 | — |
| 12 | **7.97x** | 10.61 | — |

Monotonic throughout, near-linear to 8, then a slow climb as the four
efficiency cores join. Ordinary saturation, the shape `bench/README.md`
describes, and slightly *better* than `cache-fix` at every thread count.

An earlier recording of this same sweep showed a turnover to 0.84x at twelve
threads. That was contention, not the code: a Parallels VM and other load were
running, and the affected rows carried 33-42% spreads. With the machine quiet
the curve is clean. Recorded here as a worked example of the failure mode the
README warns about.

## Against DDS 2.9, single-threaded

Best of 5, all ten tables agreeing:

```
board     ours 1t    dds 1t  ours/dds1
    1        29.1      27.0      1.08x
    2        67.0      83.3      0.80x
    3       203.4     115.4      1.76x
    4        90.0      94.6      0.95x
    5       130.5      92.9      1.40x
    6       114.7     107.6      1.07x
    7        79.3      88.4      0.90x
    8        15.0      18.3      0.82x
    9        22.0      30.1      0.73x
   10       262.8     176.1      1.49x

Per core we are 1.06x DDS's cost, geometric mean over 10 boards.
```

Two figures, because they answer different questions. The **geometric mean is
1.06x** — the typical board. The **corpus total is 1.22x** (1013.8 ms against
833.7 ms), because the sum is dominated by boards 3 and 10, the two where we are
worst. Someone solving a large batch feels the second number.

The **spread is the interesting part**, and it is not noise: 0.73x to 1.76x. We
are faster than DDS on four boards and around half its speed on two. That is not
uniform per-node overhead — it is algorithmic, and it is where the remaining
work is. Worth checking whether node counts track DDS *per board* rather than
only in aggregate.

DDS's threading backend here is GCD, which `IsIMPL()` does not cover, so
`thrMax = min(1, ncores) = 1` and the column is genuinely single-threaded.

## Against DDS across threads

Deal-level throughput: 240 distinct generated deals, both solvers given the same
list at the same thread count, best of 3. `solver-bench reference
--dds-threads N --throughput-deals 240`, one process per point.

| threads | ours ms | cores | dds ms | cores | ours/dds |
|---|---|---|---|---|---|
| 1 | 28885 | 1.00 | 25806 | 1.00 | 1.12x |
| 2 | 15051 | 1.99 | 13186 | 1.99 | 1.14x |
| 4 | 8094 | 3.93 | 6883 | 3.94 | 1.18x |
| 6 | 5637 | 5.75 | 4775 | 5.74 | 1.18x |
| 8 | 4499 | 7.53 | 4012 | 7.45 | 1.12x |
| 10 | 4099 | 9.23 | 3693 | 8.80 | 1.11x |
| 12 | 3825 | 10.34 | 3484 | 9.77 | **1.10x** |

**Threading is a wash.** The two solvers put almost exactly the same number of
cores to work at every point, and the ratio stays between 1.10x and 1.18x from
one thread to twelve. Whatever gap remains is per-node cost, not parallelism,
and it does not grow with core count — which is the thing worth knowing before
telling anyone to run this on their own machine.

Note the ratio on random deals (1.12x at one thread) is worse than on the
curated corpus (1.06x geometric mean). The corpus was selected to span a range
of difficulty, not to be representative; random deals are the better model of a
real file, and the honest headline number.

Getting to this table took three corrections, each of which would have produced
a confidently wrong published figure.

**One deal per call gave DDS five work items.** `dds.rs` called
`CalcAllTablesPBN` with `no_of_tables = 1`. `CalcAllTables` flattens the request
into one item per (deal, strain) pair before scheduling, so a single deal offers
five items no matter how many threads are configured — most idle, wall time set
by the slowest strain. The old measurement showed DDS jumping between 2.7x and
4.2x; that was load imbalance, not scaling. `solve_tables` now batches, chunked
at forty (DDS enforces `count * noOfTables <= 200`, and all five strains means
forty deals).

**Repeating the corpus handed DDS a free win.** The first throughput workload
was the ten-board corpus repeated eight times, and it measured DDS at **4.70x
our speed**. That number was entirely an artefact: `DetectCalcDuplicates` and
`CopyCalcSingle` de-duplicate identical deals inside a batch and copy the
result rather than solving again, so DDS solved ten deals per chunk while we
solved eighty. The workload is now distinct deals from a seeded generator, and
five of them are checked against DDS before timing — a generator emitting
well-formed but wrongly ordered deals would otherwise produce plausible
timings for work nobody did.

**DDS's macOS default pins it to the efficiency cores.** With the batch fixed,
DDS still flatlined at 3.97 cores and an identical wall time from four threads
up, while reporting twelve threads made. The cause is in `System.cpp`:

```c
dispatch_apply(numThreads,
  dispatch_get_global_queue(DISPATCH_QUEUE_PRIORITY_BACKGROUND, 0), ...)
```

Background QoS on Apple Silicon is confined to the efficiency cores, of which
an M4 Pro has exactly four. DDS's GCD backend therefore cannot exceed four
cores on this machine however it is configured. That is a property of how DDS
is packaged on macOS, not of DDS's scaling, and building a comparison on it
would have been indefensible. `reference` now calls `SetThreading(5)` — DDS's
STL backend, plain `std::thread` at default QoS, scheduled across all cores
like ours — and `--dds-threading 3` restores GCD for anyone who wants to see
the difference.

Worth flagging separately, because it is not our problem to fix but it is real:
**a Mac user driving DDS through its default GCD path gets four cores.**

### Measuring DDS at more than one thread count

DDS cannot survive a second `SetResources`: it tears its per-thread memory down
with `memory.Resize(0, ...)` before rebuilding, and the rebuild does not always
happen — the next solve then hits `Memory::GetPtr: 0 vs. 0` and DDS calls
`exit(1)`, taking the harness with it. `reference` now seeds the initial call
with the thread count it will measure and rejects a multi-valued
`--dds-threads` with an explanation rather than dying. **A curve means one
process per point.**

## Where the per-node time actually goes

First sampled profile of the release build. `samply record` at the default
1 kHz over the 200-deal lock-step corpus, single-threaded on the M4 Pro:
22,938 samples, 22.6 s of CPU. `perf` does not exist here; `samply` needs
`--unstable-presymbolicate` to put symbols in a `--save-only` profile, and
release needs `CARGO_PROFILE_RELEASE_DEBUG=true` to have any. Debug info
changes no codegen, so the timings stay comparable.

Self time, inlinees folded into their caller:

| | self |
|---|---|
| `evaluate_playable_cards` | 26.0% |
| `search_with_cache` | 16.1% |
| `Pattern::lookup` | 13.8% |
| `RelativeHands::convert_suit` | 12.1% |
| `Pattern::update` | 7.2% |
| `order_cards_static` | 6.9% |
| `Pattern::get_rank_winners` | 6.5% |
| `search_at_trick_start` | 6.2% |
| `PatternVec::clear` | 2.1% |
| everything else | < 1% each |

The shape to notice: the search proper is about half, and the pattern tree —
`lookup`, `convert_suit`, `update`, `get_rank_winners` — is the other 40%.
Optimisation attention has gone to the search; the arithmetic says the tree is
the equal partner.

### The two leads the profile actually supported

**`Pattern::lookup` scans children as an array of structs.** 13.8% self, and
66% of that sits on two instructions — the `ldr x8, [x26], #0x38` that walks
the children and the loop-back it feeds. `Pattern` is 56 bytes (`Hands` 32,
`Bounds`, `PatternVec` 16), and the scan streams all 56 per child while the
rejection test reads only the first 8: `is_subset_of` bails on `hands[0]` for
most children, which is exactly why those two addresses dominate. At 56 bytes
a cache line holds barely one child. Splitting the first subset key into its
own contiguous array beside the children — structure of arrays for the scan
key only — would cut the streamed footprint about sevenfold on the rejection
path. It does not reorder children, so lock-step should survive, but that
needs proving rather than assuming.

**Tried, and it costs 1.1%.** The West hand of each child was split into a
contiguous `u64` array living after the patterns in the same pooled block, and
the rejection scan in both `Pattern::lookup` and `Pattern::update` reads that
array instead of striding the structs. Lock-step survived exactly, as
predicted — 296,689,028 nodes, `none` on all twelve fixtures, with a
`debug_assert` in the scan holding the keys to their patterns. It was still a
loss: full corpus, ABBA-interleaved, geomean of per-board minima **1.011**
against base, replicated at **1.015** in a second three-way run, and nine of
the ten boards slower individually.

The mechanism the lead missed is the one it created. A key is 8 bytes on top
of a 56-byte `Pattern`, so the tree — the very structure the profile names as
the working set — grows 14%, and the scan now walks two streams where it
walked one. Narrowing the rejection path does not pay for either. The patch is
recoverable from this commit's history if a future change makes `Pattern`
small enough for the arithmetic to flip.

**`pack_bits` is a software PEXT.** `convert_suit` is 12.1%, and its body is
four `pack_bits` calls, one per seat. aarch64 has no PEXT, so the fallback
runs a bit at a time over the mask: one iteration per remaining card in the
suit, a serial dependency chain, four times over the same mask. Two
independent savings are visible without changing any result:

- The mask is identical across the four seats. A Hacker's-Delight parallel
  suffix `compress` splits into a mask-dependent setup and a cheap per-source
  apply; the setup would be paid once instead of four times.
- The four hands *partition* the suit's remaining cards, so the fourth packed
  value is the complement of the first three. If `all_cards` really is the
  union of the four hands at every call site — worth checking, not assuming —
  that is a quarter of the calls gone for free.

Both are arithmetic identities rather than search changes, so the node count
should not move.

**Tried, and it is worth 3.6%.** Neither of the two shapes above, as it turns
out: the parallel-suffix `compress` is the wrong tool, because it costs a
fixed ~24 operations where the walk costs one iteration per card still out in
the suit, and by mid-search a suit is three or four cards. What the leads had
right was that the *mask is shared*. So `convert_suit` now walks the mask once
and tests all four seats inside that one walk, rather than walking it four
times:

```rust
while m != 0 {
    let lowest = m & m.wrapping_neg();
    for seat in [WEST, NORTH, EAST] {
        if hands[seat].value() & lowest != 0 { packed[seat] |= bit; }
    }
    bit <<= 1;
    m &= m - 1;
}
packed[SOUTH] = (bit - 1) & !(packed[WEST] | packed[NORTH] | packed[EAST]);
```

The partition property does hold, and it was checked rather than assumed: all
three call sites build `all_cards` as `hands.all_cards()` (the third adding the
partial trick's cards back to both sides of the equation), so South is the
complement and is never walked. A `debug_assert` now states that precondition
at the top of `convert_suit`, and the three seats in the loop mean the inner
tests are independent.

Measured full corpus, ABBA-interleaved, geomean of per-board minima **0.964**
against base, replicated at **0.961**; all ten boards improved, in a band from
0.954 to 0.970. On the 200-deal lock-step corpus, alternating whole runs, best
23.90 s against 23.13 s — **-3.2%**, with c2 ahead in every one of four rounds.
Both invariants exact: 296,689,028 nodes and `none` on all twelve fixtures.

`pack_bits` had no other caller and is gone from the crate. Its body now lives
in `pattern.rs`'s test module as the oracle for
`convert_suit_matches_four_pack_bits`, which checks the walk against four
independent compresses over 32,768 constructed suit splits — and which fails on
mutation of either the East term of the loop or the South complement. Keeping
it written down also records what to reinstate on an x86-64 target built with
BMI2, where four `_pext_u64` instructions would beat this walk.

### Pooled retention is a footprint question, not a speed one

The ~254 MB the pool holds never showed up in this profile: `pool_alloc`,
`pool_free` and `reserve_exact_class` together are under 0.6%, and
`PatternCache::new` another 0.7%. Retention is worth fixing for memory, but
nothing here suggests it is costing time.

## How much is the missing PEXT actually worth

The question the upstream claim raises is whether an aarch64 Mac is simply the
wrong machine to check it on. It is a worse machine for the solver than a Zen 3
Linux box, for a reason that is real and specific — and the size of it can be
measured rather than argued about.

**The mechanism.** `PackBits`/`UnpackBits` compile to `_pext_u64`/`_pdep_u64`
under `__BMI2__` and to a loop over the mask's set bits otherwise, one iteration
per card still out in the suit. The makefile adds `-mbmi2` when
`/proc/cpuinfo` says `bmi2`, so a Linux Zen 3 build gets single instructions;
aarch64 has no equivalent and takes the loop. DDS, by contrast, has no
architecture-specific code at all, so the same hardware does nothing for it.
The asymmetry is entirely on the solver's side, and it is in its favour on x86.

### The reference was being timed one process per deal, and that was wrong

The first pass here ran the reference as `solver -f <deal>` once per deal, 500
times, and charged it 0.98 s of process startup measured with `-o`. Both halves
of that were wrong. `-o` prints the deal and exits without solving, so it never
allocates the caches, and the real cost is larger.

The reference's `common_bounds_cache` and `cutoff_cache` are **globals**, and
`Solve` calls `Reset()` on them per trump — which clears the entries but keeps
whatever size `Resize()` has grown them to. So a process that solves many deals
allocates once and runs every deal after the first with pre-grown tables, while
a process per deal starts from `bits = 15`/`16` and re-grows every time.

Patching `main` to read a multi-deal hands file — four lines a deal, the format
it already parses, caches still reset per trump inside `Solve` — gives
byte-identical tables and is **4.5% faster**: 500 random deals in 55.23 s as 500
processes against 52.75 s in one. On the 200-deal corpus it is 2.2%.

That matters twice over. It was unfair to the reference, because DDS was
already being measured in one process by a harness that links it. And it was
unfair in *our* favour on the headline ratio, since this port also runs all its
deals in one process. Every reference figure quoted here is now single-process.

**A second asymmetry it exposed, in the reference's favour. Now closed.**
`solve_table_inner` in `src/par.rs` built `CutoffCache::new(16)` and
`PatternCache::new(16)` fresh for every strain — five allocations a deal, a
thousand over the 200-deal corpus — where the reference allocates once for the
life of the process and resets. Cache size demonstrably does not affect node
counts here (we hold exact lock-step at 296,689,028 while starting from
different sizes than the reference), so hoisting those allocations was expected
to be pure profit. It largely is, and it came with one correction to the reading
above: keeping the grown capacity *unconditionally*, the way the reference does,
is a bad trade on a run of mixed deals, because over 99% of strains never grow
past the base size and the few that do leave every later strain clearing a table
four times the one it needs. See `cache-reuse.md`.

**The measurement.** Patching the reference's `ConvertToRelativeSuit` to walk
the shared mask once for all four seats instead of once per seat — the same
change this port made, verified to give identical tables on 25 deals — is worth
**2.0%** to the reference on 500 random deals (54.75 s to 53.63 s, best of
three interleaved rounds). That replaces four walks with one heavier walk, so it
removes something over half of the packing cost; PEXT would remove all of it,
plus the `UnpackBits` loops that `GetRankWinners` runs. Scaling accordingly puts
the whole BMI2 advantage at roughly **5%, and under 10% on any reading.**

A sampled profile of the reference on aarch64 agrees that this is the right
order. `clang -O3`, freak.1, 31,456 samples, self time:

| | self |
|---|---|
| `Pattern::Lookup` | 30.8% |
| `Play::EvaluatePlayableCards` | 18.8% |
| `Trick::ConvertToRelativeSuit` | 13.8% |
| `Play::SearchWithCache` | 9.2% |
| `Pattern::Update` | 7.5% |
| `Pattern::GetRankWinners` | 5.7% |

`ConvertToRelativeSuit` at 13.8% looks like more headroom than 5%, but most of
that self time is the per-seat `ClearSuit`/`Add`/shift work that PEXT does not
touch — which is exactly why the 2.0% patch measurement is the number to trust
over the profile share.

**So the architecture is a real effect and much too small.** It would move the
solver-to-DDS ratio here from about 1.01x to about 1.06x. Upstream claims 1.36x.
Roughly 5 points of a 36-point gap are explained by the missing PEXT, another 1
to 2 by PGO, and the 4.5% process-per-deal error was mine rather than anything
about the machine. Around 30 points are not explained.

**What has been ruled out as well.** PGO, which upstream's default `make` target
uses and this comparison originally did not: rebuilding the reference with
clang PGO, trained on `hard_deals/deal.8` as its makefile does, is worth about
1% and nothing on best-of (21.24 s plain against 21.27 s PGO). DDS batching:
this harness hands DDS forty deals per `CalcAllTablesPBN` call while the
reference runs one deal per process, but forcing the batch to one changes DDS's
500-deal time from 53.3 s to 52.9 s, so the batching is not flattering it. DDS's
build: `-O3 -flto -mtune=generic`, which is not a handicapped one. And deal
distribution: on 500 random deals rather than the curated corpus this port is
1.06x DDS instead of 1.12x, a move in the expected direction and far too small
to matter.

What is left unexplained is on the other side of a measurement nobody can
reproduce: their DDS version, build flags, and how it was invoked were never
recorded, and the comparison scripts were never committed.

### The numbers, 500 seeded random deals

Single-threaded, M4 Pro, 2026-09-02. Deals generated from a fixed seed and
converted to the reference's format; the conversion round-trips exactly.

| | 500 deals | vs DDS |
|---|---|---|
| C++ reference, PGO, one process | **52.3 s** | **1.01x faster** |
| DDS 2.9 | 52.9–53.3 s | 1.00x |
| C++ reference, plain `-O3`, one process | 53.3 s | 1.01x slower |
| C++ reference, plain `-O3`, 500 processes | 55.2 s | 1.04x slower |
| this port | 57.1 s | 1.08x slower |

Given every advantage its own build system offers — PGO, and one process
reading a multi-deal file — the reference edges DDS by about 1%. Upstream
claims 36%.

## Where the three solvers stand, measured together

2026-09-02, M4 Pro, single-threaded, the reference's own 200 deals. All three
re-measured in one sitting, because the previous standing had been carried
forward by scaling an older figure and the three numbers had not been taken
together.

| | 200 deals | vs reference | |
|---|---|---|---|
| C++ reference | **21.64 s** | 1.000x | `75b4619`, clang PGO, one process over a multi-deal file |
| C++ reference, one process per deal | 22.11 s | 0.979x | the same binary, invoked 200 times |
| **this port** | **23.09 s** | **1.067x** | |
| DDS 2.9 | **20.25 s** | 1.069x faster | in-process, STL threading, 1 thread |

Node counts are identical between the reference and this port — 296,689,028
either way — so the 1.067x is per-node cost with nothing else in it.

**This supersedes a 1.020x figure recorded earlier the same day.** That one
compared this port, which solves all 200 deals in one process, against the
reference invoked 200 times, and so charged the reference a process startup and
a cache re-grow per deal that this port never paid. Correcting it, and building
the reference with the PGO its own makefile defaults to, moves the honest
number from 1.02x to **1.07x**. The lesson is the ordinary one and it was
learned the wrong way round here: a ratio between two programs is only a
measurement when both are run the way you would actually run them.

**Method.** Five interleaved rounds, best of five, alternating within each
round rather than run in blocks; the machine was not idle, and the ordering is
what makes the ratio stand up. The port was timed through `solver-bench nodes`
and cross-checked against the exact committed binary, which agreed to 0.03 s.
DDS is the odd one out and is measured differently on purpose: `solver-bench
reference --throughput-pbn` links it in-process so both solvers are timed by
the same code on the same deals, which is worth more than putting it in the
same table as an external process.

**One asymmetry left, and it runs against this port.** Its figure is taken from
the node-counting path, which the reference is not paying for (`-S` was off).
That flatters the reference slightly, so 1.067x is if anything a mild
overstatement of the gap. The process-per-deal asymmetry that used to sit here
has been removed rather than corrected for: see "The reference was being timed
one process per deal" below.

**Reproducing the reference.** `solver.cc` was untouched upstream from
2025-09-01 to 2026-01-31, so `75b4619` *is* the 2026-01-31 state this port
lock-steps against; 27 commits have touched it since. The reference reads one
deal per file in its own layout — North, West, East, South, one hand per line,
suits space-separated, a void written `-`. Converting `lockstep-200.pbn` to it
round-trips exactly on all 200 deals, 30 of which contain a void, and deal 1's
table reproduces the reference's own pinned `1k_deals/RESULTS` entry.

## Two ways the harness used to mislead you

Both were hit while measuring the two leads above, both produced a confidently
wrong number before being caught, and both are now fixed. Recorded because the
failure modes are worth recognising elsewhere: each one reported success.

**`--quick` did not measure the same board twice.** Its doc comment said the
median-cost board was "chosen deterministically", and `median_board` timed
every board **once**, unrepeated, and took the median of those timings. Boards
4 (~81 ms) and 7 (~76 ms) are 6% apart, inside the drift on a loaded machine,
so the choice flipped between them and an A/B mean silently averaged two
different workloads. It flipped three times in twenty runs here and made a 1%
regression look like a dead heat.

`median_board` now ranks by **nodes searched**. It is the same ten solves, but
nodes is a property of the deal rather than of the machine, so the answer does
not move: ten consecutive `--quick` runs on a loaded machine now name the same
board ten times. Note that the node-median and the time-median are different
boards — nodes and time are not perfectly correlated across a corpus spanning
16 ms to 300 ms — so `--quick` numbers from before this change compare only
with each other.

Even fixed, `--quick` is one board and so one ratio. For A/B use the full
corpus: about 5 s per pass at `--runs 5`, cheap enough to interleave eight
rounds a side, and ten paired ratios instead of one. The across-the-board
consistency is what separates a result from drift, exactly as it did for the
LTO change above.

**`first-divergence.sh` would happily check a stale binary.** `XRAY` was
required, but `DIAG` defaulted to `./target/release/solver-diag` and nothing
rebuilt it. That binary needs `--features cli`, so the obvious
`./dev-build.sh --ci build --release --bin solver-diag` *fails*, leaves
whatever was there before, and the script then reports `none` on all twelve
fixtures — against the previous revision's code. One of the runs in this
session did exactly that, against a binary three hours old.

The script now builds `solver-diag` itself, with the right features, unless
`DIAG` is set explicitly — which is the case where someone means a particular
binary, an older revision say, and that one is checked for existence instead.
Silent success against stale code is the worst failure mode a divergence check
can have.

## Four things that looked wrong and were not

Recorded because each is a plausible next guess, and each cost a measurement to
rule out.

**The per-node atomic reads.** `evaluate_playable_cards` carried 32 loads of
`XRAY_LIMIT`/`XRAY_COUNT` and `search_with_cache` another 17, and an atomic load
can be neither hoisted out of a loop nor CSEd. Hoisting them into `Search`
fields read once at construction: **no win**, slower within noise.

**The dead tracing.** Between them the two hottest functions held 61 calls into
`format_inner`, `_eprint` and `__rust_dealloc` for `eprintln!` blocks that never
fire — around 4,000 instructions of never-executed code interleaved with the hot
path. Feature-gating it away removed all of it (61 → 3 calls, binary 16 KB
smaller) and was **performance-neutral**: best-of-15 interleaved A/B, 83.4 /
83.8 / 83.8 ms against 86.2 / 83.2 / 82.6 ms. Cold never-taken branches are free
on this core, and the I-cache never fetches the lines they sit on.

**`-C target-cpu=native`.** Neutral. rustc already defaults to `apple-m1` on
`aarch64-apple-darwin`, and popcount and bit-scan are ARM64 baseline. This lever
only matters on x86-64, where the default target lacks `popcnt` and BMI.

**The `panic_bounds_check` sites.** The lead below was right about the
mechanism and wrong about the size. Taking the mask from `entries.len() - 1`
at the point of use does elide the check: the `CutoffCache` probe loop loses
its `cmp`/`b.hs` pair and drops from ten instructions to eight, confirmed in
the disassembly. It bought **nothing** — ABBA-interleaved over the full
200-deal corpus, base 23.29 s mean against 23.39 s, which is inside the drift.
The profile had already said so: those two instructions took 0 of 22,938
samples, and summing *every* bounds-check guard in the whole binary — all 405
of them, found by walking the disassembly for conditional branches into a
`panic_bounds_check` block — accounts for 0.985% of samples in total. That is
the ceiling on perfect elision everywhere, and the real recovery is less,
because a predicted compare-and-branch beside a load that is missing cache is
close to free on this core.

The change was kept anyway, on other grounds: it lets the `mask` field go
entirely, from both `CutoffCache` and `PatternCache`, leaving the length as
the single source of truth. Both lock-step invariants hold — 296,689,028 nodes
and `none` on all twelve fixtures.

The lesson generalises. Counting `panic_bounds_check` call sites counts *cold
blocks*, which is not the same as counting hot compare-and-branches, and on a
wide out-of-order core stalled on memory it is nearly uncorrelated with time.

Already clean, and worth not re-checking: there is **no per-node allocation**
(`OrderedCards` is a fixed `[u8; 13]`, both caches are flat `Box<[T]>`,
`PartialTrick`'s `Vec` is construction-only) and **no `HashMap`** —
`CutoffCache` is a hand-rolled open-addressed table with linear probing.

## PGO on this port: 7.5% of the instructions, 1.7% of the time

Measured 2026-09-04, M4 Pro, rustc 1.96.0. Left undone until now because the
comparison in `bench/comparison/RESULTS.md` builds the C++ reference with the
PGO its makefile defaults to and this port with plain `cargo build --release`,
which is an asymmetry the document admits but never quantified.

Two-stage build, entirely conventional:

```bash
RUSTFLAGS="-Cprofile-generate=$PROF" cargo build --release --features bench --bin solver-bench
$BIN nodes train/deals.pbn                     # 150 random deals, seed 777001
llvm-profdata merge -o merged.profdata "$PROF"/*.profraw
RUSTFLAGS="-Cprofile-use=$PWD/merged.profdata" cargo build --release ...
```

`llvm-profdata` must be the one from `rustup component add llvm-tools-preview`
(here LLVM 22.1.2-rust-1.96.0), not Homebrew's or Xcode's -- the profile format
is versioned against rustc's LLVM. The training deals are from a seed nothing
else in this repo uses, so the profile is not fitted to the boards it is then
scored on.

**It changes no answers.** Both binaries report 296,689,028 nodes on
`lockstep-200.pbn` and 439,977,777 on the 300-deal scoring corpus. PGO is
codegen only; it cannot reach the search, which is why this experiment was safe
to run against the lock-step invariants rather than around them.

| | plain | PGO | |
|---|---|---|---|
| instructions, 300 deals | 379.96 G | 351.45 G | **-7.50%** |
| cycles per node | 313.91 | 310.11 | **-1.21%** |
| wall, 300 deals | 34,084 ms | 33,726 ms | -1.05% |
| wall, curated corpus, geomean | | | **-1.69%** |
| IPC | 2.65 | 2.50 | |

Best of five interleaved rounds for the first three, and for the last, four
interleaved `run --no-sweep --runs 5` rounds over the ten curated boards. That
row is the trustworthy one: **all ten boards improved**, in a band from -3.9% to
-0.1%. The instruction figure is trustworthy in a different way -- five rounds
spread 0.06%, because it is a property of the code.

**The instruction count overstates the win by more than fourfold, and that is
the finding.** PGO deleted 7.5% of the instructions retired and bought 1.7% of
the time, because IPC fell from 2.65 to 2.50: what it removed was cheap work the
machine was already doing in parallel with something else. This is the same
shape as the `convert_suit` change (-5.94% instructions, -1.89% wall) and more
extreme -- 4.4x overstatement against that one's 3.1x.

Read the other way, it is a statement about where the remaining per-node cost
lives: **this search is not front-end bound.** Better branch layout, better
inlining and hot/cold splitting -- everything PGO has to offer -- are worth
under two points here, so the residue against the C++ reference is not going to
be found in instruction count or code layout. That is worth knowing before the
next attempt at it, and it is consistent with `## Where the per-node time
actually goes`.

**Declined, on a threshold set before the numbers were in.** Under 3% was to be
dropped; 5% or more was to be productionised as an opt-in build path. It landed
at 1.7%, and the costs are not nominal:

* **It would describe a binary this crate's users never build.** `dealer3` and
  `bridge-wrangler` get whatever `cargo build --release` gives them. Reporting a
  PGO number in `RESULTS.md` would be exactly the error `METHODOLOGY.md` refuses
  to make about the reference, inverted -- and fixing that means a supported
  `--pgo` path, a regenerable profile and documentation, not a benchmark flag.
* **It would blunt `solver-bench cost`, which the rest of this document runs
  on.** Instruction counts are trustworthy to 0.015% here, and that is what
  makes small changes attributable at all. Under PGO an unrelated edit shifts
  which code is hot, the profile fits worse, and a phantom regression appears
  that has nothing to do with the change. Retraining per A/B is the fix and it
  is not free.
* **CI carries it forever after.** Either a checked-in `.profdata` that goes
  quietly stale, or a doubled build.

For the record on the other side of the comparison: PGO on the C++ reference
measured about 1% and nothing on best-of (21.24 s plain, 21.27 s PGO, above).
So the build asymmetry in `RESULTS.md` is worth roughly a point in the
reference's favour, not the seven that separate us.

## x86-64: the BMI2 path was never built

Measured 2026-10-28 on an AMD Threadripper PRO 5995WX (Zen 3), Windows, rustc
1.99.1. Everything above was measured on aarch64, which has no PEXT, so the
x86 side of the `target_feature = "bmi2"` code had never been timed -- and in
practice never built: Rust's default `x86_64` target enables neither BMI2 nor
POPCNT, so the PDEP path in `unpack_bits` was dead code in every ordinary
x86 build.

`convert_suit` now has a BMI2 form: one `_pext_u64` each for West, North and
East, South still the complement. The single mask walk stays for every other
target. `convert_suit_matches_four_pack_bits` passes under both.

The 200-deal lock-step corpus, `solver-bench nodes`, single-threaded. Three
rounds, all five builds running at the same time in each round so load hits
them equally; best of three:

| build | wall | |
|---|---|---|
| default `x86_64` | 56.27 s | — |
| `-C target-cpu=x86-64-v3` | 51.44 s | -8.6% |
| `-C target-cpu=native` | 50.58 s | -10.1% |
| native + PEXT `convert_suit` | **47.14 s** | **-16.2%** |
| the same + `panic = "abort"` | 47.46 s | none |

Every build reported 296,689,028 nodes. The order held in all three rounds,
and each build's three times agreed within 2%.

`panic = "abort"`, previously untried, made no difference.

The flags are not the default and should not be: release binaries have to run
on any x86-64, and Zen 1 and Zen 2 run PEXT in microcode, slowly enough that a
BMI2 build would likely lose there. `docs/cli-guide.md` gives the
build-it-yourself command.

## Still open

- **The per-board spread** against DDS, above: 0.73x to 1.76x on the curated
  corpus. Not noise, and not uniform overhead. Worth checking whether node
  counts track DDS per board rather than only in aggregate.
- **A re-profile after the `convert_suit` change.** It removed roughly three
  quarters of the work in a function that was 12.1% self, so the ranking above
  is stale at the top; `evaluate_playable_cards` and `search_with_cache` are
  now a larger share of a smaller total, and the next lead should be drawn
  from a fresh sample rather than from this table.
- **`Pattern::get_rank_winners`**, 6.5% self and never examined. It is the one
  pattern-tree entry in the profile with no lead attached to it.
- **`panic = "abort"`** was tried on x86-64 and bought nothing; see the BMI2
  section above. Not yet tried on aarch64.
- **PGO** is now measured rather than assumed. See the section above: 7.5% of
  the instructions, 1.7% of the time, and declined on that basis.

## A note on method

Below about 6%, the default `run` cannot attribute anything on a machine that is
not idle. Same-binary repeats swung 4-6% on the geometric mean under load, and
one such swing initially read as a 5.6% regression from a change that turned out
to be neutral.

What worked instead: build both binaries, keep both, and alternate them
A/B/A/B/A/B with `--quick --runs 15`. That held to roughly 1% even under load,
because the two builds see the same interference. `compare` cannot do this
today — it reads two JSON files recorded at different times, which is precisely
the design that cannot interleave.
