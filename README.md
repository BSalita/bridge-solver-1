# bridge-solver

A fast double-dummy solver for bridge, with par-contract scoring, card-by-card
play analysis and a claim checker. Native binaries for macOS, Windows and Linux,
a Python package, a C library for any other language, a WebAssembly build for
the browser, and a Rust crate to build against.

**[Try it in your browser →](https://bridge-craftwork.github.io/bridge-solver/)**
· [Command-line guide](docs/cli-guide.md)
· [Download a release](https://github.com/bridge-craftwork/bridge-solver/releases)

## What it does

- **A single deal** — `solver-diag` takes one deal and prints the full table of
  twenty results, four declarers by five strains.
- **A file of deals** — `bridge-solver` annotates PBN files in place with
  `DoubleDummyTricks`, `OptimumResultTable`, `OptimumScore` and `ParContract`,
  in the tags Bridge Composer reads.
- **A collection of deals** — point it at a directory and it walks the tree.
  Boards that already carry analysis are passed through byte-for-byte, so
  running it over a library only fills in what is missing.
- **Card by card** — not just how many tricks the deal is worth, but what each
  card played costs against best defence, and what the alternatives were. In the
  crate and in the browser build.
- **Claims** — checks whether a claim holds however the unseen cards lie, for
  declarer or a defender. Try it on your own hands on the
  [claim checker page](https://bridge-craftwork.com/bridge-solver/claim.html):
  paste a PBN, LIN or BBO hand and claim at any point in the play.
  `bridge-claim` does the same from the command line. How it works and why:
  [design notes](docs/claim-checker.md).
- **Benchmarks** — the [benchmarks page](https://bridge-craftwork.com/bridge-solver/bench.html)
  times the analysis and the claim checker in your browser, on fixed deals and
  claims checked against the native tools; `bridge-solver --bench` and
  `bridge-claim bench` run the same work on every core.
- **Par scoring** — the optimum score and the contracts that achieve it, given
  vulnerability: every contract tied at par, and named for a single seat when
  only one partner can take the tricks, the way Bridge Composer writes them.

Files and collections are **multithreaded**, and there is nothing to switch on:
work is spread over every core by default, and `--threads` is there to turn it
down. The unit scheduled is a (deal, strain) pair rather than a whole deal,
which is what keeps the last threads busy rather than waiting on the slowest
deal. **The output is identical at any thread count** — tables are assembled by
index, not in completion order.

## Performance

A full twenty-cell table costs **about 14 ms** for a typical deal on an M4 Pro
— that is across its twelve cores, which is how you would actually run it, and
about 110 ms if you pin it to one. Deal cost varies about tenfold: over a
ten-board sample the single-core range was 15 ms to 263 ms.

On the same machine 200 boards take **2.75 s** on twelve threads against 22.6 s
on one, an **8.2x** speedup. Scaling is near-linear to eight — this M4 Pro has
eight performance cores — and climbs more slowly as the four efficiency cores
join. The [command-line guide](docs/cli-guide.md#threading) has the full sweep.

Freak deals are the exception worth knowing about: several voids and a deep
search can run for **tens of seconds** — 39.7 s and 1.6 GB for the worst in our
corpus. Rare enough not to move a batch's total, but it will be the deal you
wait on.

For context, over the reference's own 200-deal corpus this port runs at 1.067x
the C++ reference's time and 1.14x DDS 2.9's, searching a tree identical to the
reference's node for node.
[`bench/results/release-profile.md`](bench/results/release-profile.md) is the
full record — how all three were measured together, what was tried, and what was
found to be worthless.

## Install

Download the archive for your platform from the
[releases page](https://github.com/bridge-craftwork/bridge-solver/releases); each
holds both binaries and the licences.

| Archive | Platform |
|---|---|
| `bridge-solver-macos-aarch64.tar.gz` | macOS, Apple Silicon |
| `bridge-solver-macos-x86_64.tar.gz` | macOS, Intel |
| `bridge-solver-linux-x86_64.tar.gz` | Linux, x86-64 |
| `bridge-solver-windows-x86_64.zip` | Windows, x86-64 |
| `bridge-solver-wasm.tar.gz` | WebAssembly package, for browsers and Node |

```sh
bridge-solver -i deals.pbn                 # analyse one file, to stdout
bridge-solver -w -i deals.pbn              # ...or rewrite it in place
bridge-solver -w -i Curated/               # annotate a whole tree
bridge-solver -w -i Curated/ -j 4          # ...on four threads
solver-diag  -f deal.txt                   # one deal, full table
bridge-claim bench                         # time the claim checker, every core
bridge-claim check claims.txt              # check claims, one per line
```

Every switch, both file formats and the things the annotator will not touch are
in the **[command-line guide](docs/cli-guide.md)**.

## In the browser

The [demonstration site](https://bridge-craftwork.github.io/bridge-solver/) runs
the same engine compiled to WebAssembly — nothing is sent to a server. Paste a
deal or a LIN file and it will give you the double-dummy table and walk the play
card by card, marking what each one cost. There is also a
[gallery](https://bridge-craftwork.github.io/bridge-solver/gallery.html) of
worked examples.

In wasm a twenty-cell table is ~250 ms and a full per-card analysis of a
complete deal ~128 ms, against ~90 ms for the same analysis natively.

## From Python

```sh
pip install bridge-solver
```

```python
import bridge_solver as bs

deal = "N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72"
bs.dd_table(deal)["N"]                    # {'C': 8, 'D': 7, 'H': 8, 'S': 10, 'NT': 9}
bs.check_claim(deal, "S", "N", 10)        # {'verdict': 'invalid', ...}
```

Prebuilt wheels for Linux, macOS and Windows, Python 3.9 and later; nothing to
compile. See [`python/README.md`](python/README.md).

## From C, or any language that can call C

Each release has `bridge-solver-c-<platform>` archives: a header, shared and
static libraries, and an example. Most calls take and return JSON, the same
requests and responses as the Python and WebAssembly packages, so a binding in
another language stays small. See [`ffi/README.md`](ffi/README.md).

## As a Rust crate

Not yet on crates.io — it depends on `bridge-types` by git, which crates.io does
not allow — so take it from git:

```toml
[dependencies]
bridge-solver = { git = "https://github.com/bridge-craftwork/bridge-solver" }
```

```rust
use bridge_solver::{Hands, Solver, CutoffCache, PatternCache, NOTRUMP, WEST};
use bridge_types::Deal;

let deal = Deal::from_pbn("N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72").unwrap();
let hands = Hands::from_deal(&deal);

let mut cutoff = CutoffCache::new(16);
let mut pattern = PatternCache::new(16);
let solver = Solver::new(hands, NOTRUMP, WEST);
let ns_tricks = solver.solve_with_caches(&mut cutoff, &mut pattern);
```

The solver holds no global state and is safe to drive from many threads at once;
give each its own pair of caches. Optional features: `play-analysis` for
card-by-card analysis, `json` for the JSON interface the other bindings share
(`bridge_solver::json_api`), `cli` for the binaries.

## Attribution and licence

The double-dummy search core is a Rust reimplementation of
[macroxue/bridge-solver](https://github.com/macroxue/bridge-solver) by Hanhong
Xue, corresponding to upstream commit
[`75b4619`](https://github.com/macroxue/bridge-solver/commit/75b4619). Par
scoring, the cardplay analysis, PBN handling, the WebAssembly bindings and the
web application have no upstream counterpart;
[`derivation-audit.md`](derivation-audit.md) sets out which is which, file by
file.

The claim checker (`src/claim.rs`, `bridge-claim`) is this project's own. Its
model is BBO's as Lorand Dali described it on the bridge-dev list, and three of
its bounds adapt ideas, not code, from Paul M. Bethe's
[DTAC](https://github.com/pmbethe09/DTAC) ("DTAC: A method for planning to claim
in Bridge", MS thesis, NYU, 2010), which Gal Hegedus pointed us to.
[`docs/claim-checker.md`](docs/claim-checker.md) has the design, the
measurements and the full credits.

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or the
[MIT license](LICENSE-MIT) at your option — the same pairing upstream adopted in
[`dc2d4df`](https://github.com/macroxue/bridge-solver/commit/dc2d4df). Both files
ship in every release archive.

[`THIRD-PARTY-NOTICES`](THIRD-PARTY-NOTICES) covers the packages a distributed
copy is built from, whose own licences ask the same of us;
`web/public/THIRD-PARTY-NOTICES` is the equivalent for the WebAssembly build,
which is a different dependency graph. Both are generated from what cargo
resolved, by `scripts/third-party-notices.py`, and checked in CI — a hand-kept
notice file goes stale the moment a dependency moves, and a stale notice asserts
something untrue about what a copy contains. They carry no version numbers, and
they only ever grow: CI fails when something resolved that the file does not
credit, which is the direction that matters. Both ship in the release archives.
