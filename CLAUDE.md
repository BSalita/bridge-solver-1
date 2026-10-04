# CLAUDE.md

This file provides guidance to Claude Code when working with this repository.

## Project Overview

<!-- TODO: Add project description -->

## Build & Test Commands

**Use `./dev-build.sh` for local development builds, not bare cargo.** This repo depends on sibling bridge crates as git dependencies, with gitignored `[patch]` overrides in `.cargo/config.toml` redirecting them to the local checkouts in `../`. Cargo never lets a `[patch]` override an existing `Cargo.lock` pin, so bare `cargo build` silently compiles the GitHub revisions of those crates instead of your local edits — and if the patches do take effect, they rewrite `Cargo.lock` with local-path entries that must never be committed (CI has no sibling checkouts). The script keeps a separate local lock (`.cargo/dev.lock`), swaps it in around the cargo call, verifies each patched crate resolved to a local checkout, and leaves the committed `Cargo.lock` untouched.

```bash
./dev-build.sh                    # cargo build, against local sibling checkouts
./dev-build.sh build --release    # any cargo subcommand + args pass through
./dev-build.sh test               # cargo test
./dev-build.sh clippy -- -D warnings   # lint
cargo fmt --check                 # no dependency resolution; bare cargo is fine
```

For CI-parity builds (pre-commit checks, release verification) use `./dev-build.sh --ci test` (any cargo subcommand works after `--ci`) — it temporarily disables the local patches and builds with the committed lock's git pins. **Avoid bare cargo for anything that resolves dependencies** (build/test/check/run): with the patches present, a same-version patch is applied immediately and silently rewrites `Cargo.lock` to local-path entries, while a version mismatch makes the patches silently ignored — both wrong. The committed `Cargo.lock` must always pin `git+https://` sources for the internal crates; never commit a lock where those entries have lost their `source =` lines.

## Pre-commit Requirements

Before committing, always run and fix:
1. `cargo fmt --all` - Format all code
2. `./dev-build.sh --ci clippy --all-targets -- -D warnings` - Fix all clippy warnings
3. `./dev-build.sh --ci test` - Ensure all tests pass (CI parity: patches disabled, committed lock's git pins)

## Code Standards

- No `unwrap()` or `expect()` outside test code - use proper error handling
- No `println!()` in library code (CLI binaries are OK)
- All public functions must have doc comments (`///`)
- All `unsafe` blocks must have a comment explaining why they're safe
- Prefer editing existing files over creating new ones

## Git Configuration

Use SSH for all GitHub operations:
- Clone/push/pull: `git@github.com:bridge-craftwork/repo.git` (not `https://`)
- Remote URLs should use SSH format

## Related Projects

All located at `/Volumes/Express2T/Development/GitHub/`:

| Project | Description | Relationship |
|---------|-------------|--------------|
| [bridge-types](../bridge-types) | Core bridge types | upstream dependency |
| [Bridge-Parsers](../Bridge-Parsers) | PBN/LIN file parsing | sibling |
| [pbn-to-pdf](../pbn-to-pdf) | PDF generation | downstream |
| [bridge-wrangler](../bridge-wrangler) | CLI tool | downstream |
| [dealer3](../dealer3) | Hand generator | sibling |

## Notifications

Send Pushover notifications when work is blocked or completed:

```bash
pushover "message" "title"    # title defaults to "Claude Code"
```

**When to notify:**
- Waiting for user input or permission
- Task completed after extended work
- Build/test failures that need attention
- Any situation where work is paused and user may not notice

## Lock-step with the C++ reference

This is a port of `macroxue/bridge-solver`, and as of 2026-09-02 it searches the
same tree as that project at its 2026-01-31 state — **node for node, on all 200
deals of the reference's own corpus**. That is a property worth keeping, because
it turns any behavioural change into an immediately visible one.

Two invariants must hold after any change that could touch the search:

```bash
./dev-build.sh --ci run --release --features bench --bin solver-bench --   nodes fixtures/divergence/lockstep-200.pbn
```

must total exactly **296,689,028 nodes**, and

```bash
XRAY=<path to built xray/solver-xray> fixtures/divergence/first-divergence.sh
```

must report `none` for all twelve fixtures. See `fixtures/divergence/README.md`
for what each fixture isolates and how to build the instrumented reference.

Searching *fewer* nodes is a divergence too, not an improvement. Getting to
lock-step took five fixes, three of which were ordering bugs in the pattern
tree: `Pattern::lookup` returns the first child that matches, so child order is
semantics rather than housekeeping.

`bench/results/release-profile.md` is the full record — what was measured, what
was tried and found worthless, and where the remaining gap is.

## Performance work

Current standing, single-threaded over those 200 deals, measured 2026-09-02:
the reference **21,640 ms**, this port **23,090 ms**, DDS 2.9 **20,250 ms**.
With identical node counts, **1.067x is pure per-node cost** with nothing else
mixed in.

**Run both solvers the way you would actually run them.** The reference must be
built with the PGO its own makefile defaults to, and pointed at a multi-deal
file so it solves all 200 in one process — its caches are globals that `Reset()`
without shrinking, so a process per deal makes it re-grow them 200 times and
costs it 2.2%. Timing it one deal per process against this port's single-process
run produced a 1.02x that was wrong in our favour. See "Where the three solvers
stand, measured together" and "The reference was being timed one process per
deal" in `release-profile.md` for the method, and for how to rebuild the
reference (`75b4619`, the 2026-01-31 state) and convert deals to its format.

Before optimising, read the "tried and found worthless" list in
`release-profile.md`. Hoisting the per-node atomics, deleting ~4,000
instructions of never-executed tracing, `-C target-cpu=native` and hoisting a
redundant cache-key hash all measured as exactly nothing.

**Check instructions first, then confirm with time.** `solver-bench cost <pbn>`
reports instructions retired, cycles and nodes for a corpus in one pass. On a
machine at load average 5, five repeats of it varied by **0.015%** on
instructions against 1.45% on wall clock, so a single run of it is worth more
than fifteen wall-clock runs and it can be trusted while something else is
building. Instructions retired are a property of the executed code, not of the
machine's mood.

It answers a different question, though, and the gap is informative rather than
noise. The `convert_suit` change measures **-5.94% instructions, -2.12% cycles,
-1.89% wall**: it deleted cheap ILP-friendly ALU work, so IPC fell from 2.80 to
2.70 and the instruction figure overstates the win threefold. So use `cost` to
decide whether an idea is worth pursuing and to catch a change that quietly
adds work -- it is unambiguous about the *sign* -- and use `run` for any number
that gets recorded. A change that removes instructions and raises cycles has
traded compute for stalls, which is what the SoA scan-key experiment did.

**Quote cycles, not wall, when the question is speed.** Wall per cycle is
constant to 0.27% here, so while everything stays on a performance core the two
say the same thing. They stop saying the same thing when a run lands on an
efficiency core: measured with `taskpolicy -b`, that inflates wall by 312% and
cycles by only 36.6%, because cycles absorb the 3.99 GHz-to-1.32 GHz frequency
drop and leave just the lower IPC. Wall clock wrong by four, cycles wrong by
1.4, and a minimum-of-three rejects the latter easily. Keep wall for anything
user-facing, since a person waits on seconds.

Measurement discipline that this repo has learned the hard way:

- On a machine that is not idle, same-binary repeats swing 4–6% on the geometric
  mean. Build both binaries, keep both, and alternate them A/B/A/B. A single
  before/after pair has produced a confident wrong answer here more than once.
- **A/B on the full corpus, not `--quick`.** `run --no-sweep --runs 5` is ~5 s,
  so eight interleaved rounds a side is a couple of minutes, and the ten paired
  per-board ratios are what make a result attributable — a real change moves
  every board by roughly the same fraction, and noise does not. `--quick` is one
  board and so one ratio. Its board choice used to be an unrepeated timing that
  flipped between two boards under load, averaging two workloads into one
  number; it now ranks by nodes searched and is stable, but `--quick` numbers
  from before that change compare only with each other. See "Two ways the
  harness used to mislead you" in `release-profile.md`.
- `perf` does not exist on macOS. Use `samply record`, or Instruments' CPU
  Counters template, for a profile.
