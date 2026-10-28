# Performance harness

A characterization, not a proof. It answers "is the search getting cheaper, and
are we using the machine we are on?" — two questions that move for different
reasons and must not be collapsed into one headline.

Run on demand. Not in CI: a full run takes a couple of minutes and the numbers
are only comparable on an otherwise-idle machine.

## The three questions

| | What it measures | How stable |
|---|---|---|
| **Per-board cost** | How efficient the search is, one core on one board | Very — repeats land within 1–2%, because a single-threaded process gets a full core even on a busy machine |
| **Thread sweep** | How much of the machine concurrent solves can actually use | Noisy at high thread counts, where the run contends with everything else; read it to two significant figures |
| **Verify** | That the solver still gets the right answer | Exact |

## Commands

```bash
./dev-build.sh --ci run --release --features bench --bin solver-bench -- run
./dev-build.sh --ci run --release --features bench --bin solver-bench -- run --quick --no-sweep
./dev-build.sh --ci run --release --features bench --bin solver-bench -- sweep
./dev-build.sh --ci run --release --features bench --bin solver-bench -- verify
./dev-build.sh --ci run --release --features bench --bin solver-bench -- compare BASE.json NOW.json
```

Release matters. A debug build is roughly two orders of magnitude slower and
does not even rank the boards in the same order, so its "hardest board" is not
the release build's hardest board.

`--quick` measures only the median-cost board, where cost is **nodes searched**
rather than wall time. Both rank the same ten solves, but nodes is a property of
the deal and the search, so two `--quick` runs agree on which board they mean
however busy the machine is. Ranking by a single unrepeated timing did not: two
boards 6% apart traded places from run to run, and an A/B over them averaged two
different workloads.

Use `--quick` while iterating. Use the full corpus for anything recorded, and
for any A/B -- ten paired per-board ratios are what make a difference
attributable, because a real change moves every board by roughly the same
fraction and noise does not.

## Instructions, when the question is "did I remove work"

```bash
solver-bench cost <file.pbn>
```

Instructions retired, cycles, nodes and wall for a corpus, in one pass. The
first of those is nearly deterministic -- 0.015% over five repeats on a machine
at load average 5, against 1.45% for wall clock -- because it is a property of
the code rather than of the machine. One run of it beats fifteen timed ones for
the question "does this change do less work", and it survives a busy machine.

It is not a substitute for timing. Instructions are not time, and the ratio
between them is the interesting part: the `convert_suit` change measured -5.94%
instructions but -2.12% cycles and -1.89% wall, because what it deleted was
cheap parallel ALU work and IPC fell from 2.80 to 2.70. Fewer instructions with
*more* cycles means a change traded compute for stalls, which is worth knowing
early. Iterate with `cost`, record with `run`.

### Prefer cycles to wall clock for "is it faster"

Cycles are the middle term, and they are the one to quote. Twelve repeats on a
loaded machine:

| | spread | CV |
|---|---|---|
| instructions | 0.017% | 0.004% |
| cycles | 1.010% | 0.344% |
| wall | 1.131% | 0.404% |
| **wall per cycle** | **0.270%** | 0.080% |

That last row is the important one. Wall per cycle is nearly constant, so on a
machine whose work stays on the performance cores, cycles and wall carry the
same information and cycles buys almost nothing -- and the 1% both of them
still wander is *real*, not jitter: it is `cycles/instruction` moving by the
same 1.007%, which is cache and predictor state genuinely differing as other
processes come and go. No counter averages that away.

Cycles earn their keep in the case that silently ruins a wall-clock result:
work landing on an efficiency core. The same corpus, forced there with
`taskpolicy -b`:

| | P-cores | E-cores | inflated by |
|---|---|---|---|
| instructions | 31.219e9 | 31.451e9 | 0.74% |
| cycles | 11.629e9 | 15.881e9 | 36.6% |
| wall | 2,918 ms | 12,014 ms | **312%** |

Effective clock 3.99 GHz against 1.32 GHz. Cycles absorb the frequency
difference entirely and leave only the efficiency core's lower IPC, so an
excursion that makes wall clock wrong by a factor of four makes cycles wrong by
a factor of 1.4 -- and a minimum-of-three rejects it easily, where in wall
clock it can hide inside a mean.

So: **cycles as the headline for speed, wall for anything user-facing** (a
person waits on seconds, not cycles), and instructions to ask whether the work
itself changed. Cycles are never worse than wall and occasionally much better,
which is the whole argument for them.

## Wall clock and CPU, always both

Every measurement records wall clock *and* CPU time (`getrusage`, so it counts
every thread). This is not redundancy. Wall clock alone cannot tell "we used
four cores well" from "we got faster on one core", and those are separate
pieces of work with separate fixes.

The `cores` column is CPU divided by wall: 1.00 is single-threaded, 4.00 means
four cores were saturated for the whole run. Measured, never inferred from a
thread-count flag — inferring it is precisely how the first round of figures in
issue #13 went wrong.

`compare` reports the two geometric means separately for the same reason. A
change that moves the CPU figure made the search itself cheaper or dearer; a
change that moves only wall clock changed how well we parallelise.

## Reading the thread sweep

The sweep runs **equal work per thread** — every thread solves the same board
the same number of times. Never one board per thread: board cost in this corpus
spans 16 ms to 300 ms, nearly 20x, so a board-per-thread sweep measures the
spread of board difficulty and reports a number that looks like scaling and is
not. An early version of this measurement did exactly that and reported 1.29x
where the truth was 6.9x.

The distinction that matters in the curve is **plateau versus regression**:

- A curve that **flattens** past the physical performance-core count is
  ordinary saturation. On an 8P+4E machine, expect near-linear scaling to 8 and
  then a slow climb as the efficiency cores contribute perhaps a third of a
  performance core each.
- A curve that **turns down** — more threads, less throughput — is contention
  on shared state, and that is a bug. The harness says so explicitly.

**Sweep runs must be long enough or the curve is fiction.** At a few hundred
milliseconds a run is mostly thread startup and the final join. The harness
pilots the *widest* configuration (the fastest in wall-clock terms, so the one
most at risk of being all overhead) and grows the workload until even that runs
past `--min-sweep-seconds`, so the trap is closed by default.

To check a curve is real, vary the workload and see whether its shape holds. If
the turnover moves with workload size, the cost is per-run; if it stays put, it
is contention.

Because each run sizes its own workload, **absolute sweep times are not
comparable between runs** — only the speedup curves are, each being normalised
to its own single-threaded point. `compare` says so when the two differ.

## Best of N

The fastest run is the least contaminated by whatever else the machine was
doing, so it is the headline. The median and the spread are kept beside it, and
a spread over 15% is flagged with `!` — that means the machine was busy, not
that the code changed. `compare` treats a change smaller than either run's own
spread as "within noise" rather than reporting it as a result.

## The corpus

`corpus.json` — ten real boards from `fixtures/bench-boards.lin`, spanning 16 ms
to 300 ms single-threaded. Committed, so results are comparable across
revisions.

Every board carries its double-dummy table, and `verify` re-checks all twenty
entries. Those tables were not taken on trust: they are the ones independently
pinned in `web/src/lib/fixtures/bench-v2.json`, and the deals here were
converted from the LIN fixture and confirmed to reproduce them exactly. So the
corpus is a correctness fixture as well as a workload, and a run that is fast
and wrong says so.

## Comparing engines: `solver-bench engines`

This port against DDS-compatible libraries loaded at run time -- upstream DDS
2.9, the ddss fork, any build exporting `CalcAllTablesPBN` -- on the same deals,
in one process, by one timer:

```bash
cargo run --release --features bench --bin solver-bench -- engines     --dds-lib path/to/ddss/dds.dll --dds-lib path/to/dds_oob.dll     --threads 16,36,64 --runs 3 --json bench/results/engines-<machine>.json
```

Each engine solves the whole batch (default: the 200 lock-step deals; `--pbn`
for your own) at each `--threads` cap, and the report has the shape of ddss's
`dtest --verify` summary with this port as the baseline: time, tables per
second, speed relative to this port (above 1.00x is faster), and the number of
the twenty cells per table that disagree -- with every engine and between each
pair of libraries. Any mismatch fails the run.

Give every engine the same explicit thread count on Windows. `--threads 0`
means "all cores" to each engine, and Rust's count of all cores is one
processor group (64 on a 128-thread machine) where DDS counts all of them.

Use `--runs 3` or more. A single run is not a measurement on a busy many-core
machine: the first run here, with `--runs 1`, had ddss at 3.0-3.5 s on 64 and
128 threads, and best of three puts it at 1.7 s.

On a 64-core Threadripper PRO 5995WX, Windows 11, 2026-10-28, best of three,
200 deals, every cell matching. `results/engines-5995wx.json` is this port's
default build; `results/engines-5995wx-native.json` is built with
`RUSTFLAGS="-C target-cpu=native"` (see `docs/cli-guide.md`). The ddss DLL is
built `-march=native` either way.

| threads | this port, native | this port, default | ddss | DDS 2.9 |
|---|---|---|---|---|
| 16 | **3.11 s** | 3.67 s | 3.43 s | 4.67 s |
| 36 | **1.55 s** | 1.82 s | 1.67 s | 3.63 s |
| 64 | **1.34 s** | 1.52 s | 1.68 s | 3.42 s |

The ddss and DDS 2.9 columns are from the native run; the default run's agree
within 2%. ddss is flat from 36 threads up, which matches the 36 its own
`dtest --thread-sweep` picks on this machine; this port keeps gaining to 64.
Built for the CPU, this port is ahead at every count. At default settings, ddss
is ahead at 16 and 36. On one thread the three were within 3% in the
first run (DDS slightly ahead, as `CLAUDE.md` records on the M4).

### A larger batch: 2,000 deals

On 200 deals the slowest single (deal, strain) item -- which cannot be split --
sets the wall time from about 64 threads up, so those rows compare tails more
than throughput. `results/engines-5995wx-2000.json` is 2,000 random deals
(`comparison/gen-corpus.py`, seed 2026), default build, engine order rotated
each round; best of three at 128-32 threads, best of two at 16 and 8:

| threads | this port | ddss | DDS 2.9 |
|---|---|---|---|
| 128 | **7.28 s** | 7.69 s | 33.3 s |
| 64 | 9.63 s | **9.25 s** | 33.0 s |
| 32 | 18.2 s | **17.1 s** | 35.3 s |
| 16 | 35.2 s | **33.9 s** | 44.5 s |
| 8 | 70.7 s | **67.6 s** | 72.3 s |
| 1 (one run) | 559 s | **547 s** | 547 s |

Within 2-7% of ddss throughout, ahead at 128. Two fixes got it there. The batch
scheduler now starts the hardest-looking deals first (`Hands::hardest_first`,
DDS's own fanout estimate): in file order one 4.5 s strain near the end of the
file started last, and this port was 1.5x behind ddss at 64-128 threads. And
the library's batch workers are now kept between batches: spawned per batch,
each one left its search memory behind (~3 GB per 600 deals on 64 threads),
and later batches in the same process slowed as it grew.

## Comparing against other solvers

Everything above is this port measured against itself, which is what the ten
curated boards are for. Comparing *products* -- this port, DDS 2.9 and the C++
reference -- is a different job with a different corpus and different
methodology, and it lives in `comparison/`. Start with
`comparison/METHODOLOGY.md`.

## Files

```
bench/
  corpus.json                     the boards, with their pinned DD tables
  README.md                       this file
  comparison/
    METHODOLOGY.md                how the three-solver comparison is measured
    RESULTS.md                    what it currently says
    gen-corpus.py                 the shared random corpus, from a seed
    build-reference.sh            build macroxue/bridge-solver for comparison
    ref-latency.patch             its per-deal timing instrumentation
    throughput.sh                 whole-set wall clock at N threads
  results/
    BASELINE.md                   the current reference numbers, and what they mean
    <name>.json                   a recorded run
```

Results default to `bench/results/<git describe>.json`. Note that two runs from
different dirty working trees describe identically, so pass `--json` explicitly
when recording a before-and-after that is not yet committed.

`compare` warns when two results were taken against different corpus versions
or on different machines, because in either case the per-board numbers describe
different work.
