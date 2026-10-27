# Command-line guide

Three binaries ship in every release.

| Binary | For |
|---|---|
| `bridge-solver` | Annotating PBN files and collections with double-dummy and par analysis |
| `solver-diag` | Solving a single deal from a plain text file, and inspecting the search |
| `bridge-claim` | Checking claims, and benchmarking the claim checker |

All three are self-contained: no runtime, no configuration file, nothing to install
alongside them.

## Getting a binary

Download the archive for your platform from the
[releases page](https://github.com/bridge-craftwork/bridge-solver/releases) and
unpack it. Each archive holds all three binaries plus the licence files.
`bridge-claim` is also attached on its own (`bridge-claim-macos-aarch64`,
`-macos-x86_64`, `-linux-x86_64`, `-windows-x86_64.exe`), for anyone who wants
just that.

| Archive | Platform |
|---|---|
| `bridge-solver-macos-aarch64.tar.gz` | macOS, Apple Silicon |
| `bridge-solver-macos-x86_64.tar.gz` | macOS, Intel |
| `bridge-solver-linux-x86_64.tar.gz` | Linux, x86-64 |
| `bridge-solver-windows-x86_64.zip` | Windows, x86-64 |
| `bridge-solver-wasm.tar.gz` | The WebAssembly package, for browsers and Node |

On macOS the binaries are unsigned, so Gatekeeper will quarantine them on first
run. Clear it with `xattr -d com.apple.quarantine bridge-solver`.

To build instead, see [Building from source](#building-from-source) below.

---

# `bridge-solver` — annotating PBN files

`bridge-solver` fills in double-dummy and par analysis on PBN files, writing
Bridge Composer compatible tags: `DoubleDummyTricks`, `OptimumResultTable`, and
— when the board states its vulnerability — `OptimumScore` and `ParContract`.

```sh
bridge-solver -i deals.pbn                 # analyse one file, to stdout
bridge-solver -i deals.pbn -o out.pbn      # ...or to another file
bridge-solver -w -i deals.pbn              # ...or rewrite it in place
bridge-solver -w -i Curated/               # annotate a whole tree in place
bridge-solver -w -i a.pbn b.pbn Extra/     # several inputs at once
```

## Options

| Option | Effect |
|---|---|
| `-i`, `--input <PATH>...` | Input file(s) or director(ies). **Required.** Directories are searched recursively for `*.pbn`. More than one input requires `--in-place`. |
| `-o`, `--output <FILE>` | Write to this file instead of stdout. Only valid with a single input, and cannot be combined with `--in-place`. |
| `-w`, `--in-place` | Rewrite each input file where it sits. |
| `-j`, `--threads <N>` | Worker threads for solving. Defaults to the machine's available parallelism; `1` solves serially. |
| `--recalculate` | Redo boards that already carry analysis. |
| `--mark-verified` | Set the "double-dummy data has been verified" bit in each annotated board's `[BCFlags]`. |
| `-v`, `--verbose` | Report progress on stderr. |
| `-h`, `--help` | Full help, with the long-form notes. |
| `-V`, `--version` | Version. |

A file is taken as given whatever its extension, so a `.txt` full of PBN can be
named directly; the `*.pbn` filter applies only when walking a directory.

## Vulnerability, and when par appears

`OptimumScore` and `ParContract` need to know who is vulnerable, so they are
written only for a board carrying a `[Vulnerable]` tag the parser recognises.
The `DoubleDummyTricks` table and `OptimumResultTable` do not depend on it and
are written either way.

The accepted values are the ones PBN 2.1 §3.4.10 defines:

| Tag value | Meaning |
|---|---|
| `None`, `Love`, `-` | neither side |
| `NS` | North-South |
| `EW` | East-West |
| `All`, `Both` | both sides |

Case is not significant, and the hyphenated `N-S` and `E-W` are accepted too as
a common informal spelling. Anything else — including bare `N` or `E`, which the
spec does not define — is treated as "no vulnerability stated": the board keeps
its double-dummy table and simply gets no par.

## What `ParContract` says

`OptimumScore` is one number; `ParContract` is not simply that number with a
contract attached, and the two Bridge Composer conventions it follows both cost
us a wrong answer before `fixtures/bridge-composer` settled them.

**Every contract tied at par is listed**, cheapest first, separated by `"; "`:

```
[ParContract "EW 2SX-1; EW 3CX-1"]
[ParContract "NS 4H=; NS 4S="]
```

One contract per strain — the cheapest level in that strain that reaches par,
since bidding the same strain higher is the same contract dearer.

**A single seat is named when only one partner can take the tricks.** Where
North makes nine at notrump and South only eight, the par contract is `N 3N=`
rather than `NS 3N=`: South cannot declare it. When both partners take the same
number of tricks the side is named, which is the common case.

```
[ParContract "N 3N="]      only North makes it
[ParContract "NS 4H="]     either of them does
```

## What it will not touch

The pass is designed to be safe to run over source material you care about.

- **Only what is missing.** A board that already has a `DoubleDummyTricks` tag
  is passed through byte-for-byte. `--recalculate` redoes those too.
- **Nothing else is disturbed.** The file is held as written and the tags are
  spliced into it, rather than the file being reparsed and re-emitted, so `%`
  directives (Bridge Composer's fonts, page setup and colours), `;` comments and
  hand-authored `{...}` commentary all survive exactly as written. Annotating a
  collection only ever adds lines. This is
  `bridge_encodings::pbn::PbnDocument`'s doing, not this tool's: an unedited
  record is emitted from the original bytes, so it round-trips by construction.
  The one thing a comment can see change is which side of it a *new* tag lands
  on, since a tag is ranked against the tags around it and a comment has no rank.
- **Line endings survive.** Each line keeps the ending it was written with, so a
  CRLF file — which is every file Bridge Composer writes — stays CRLF, an LF file
  stays LF, and a file mixing the two keeps each line as it was. The lines the
  tool inserts have no neighbour to copy from, so they take whichever ending the
  file mostly uses. A file that ended without a final newline still does.
- **Incomplete deals are skipped.** Auction-only teaching boards, written as
  `[Deal "N:... ... ... ..."]`, parse into empty hands; they are left alone
  rather than stamped with a fabricated all-zero table and a "Pass" par.
- **Re-runs are no-ops.** A file whose content would not change is not rewritten,
  so mtimes do not churn and a build sees nothing to redo. In-place writes go
  through a temporary file and a rename, so an interrupted run cannot leave a
  half-written file.

## Where the tags go

Bridge Composer's own layout, read off a round trip through it
(`fixtures/bridge-composer`, which is where all of this is evidenced). A board
comes out of Bridge Composer in five groups:

1. the 15 mandatory tags, in the standard's order;
2. supplemental **tag pairs**, sorted alphabetically — custom tags included;
3. `[Auction]` and its calls;
4. `[Play]` and its cards;
5. supplemental **sections**, sorted alphabetically among themselves.

So the three one-line tags — `DoubleDummyTricks`, `OptimumScore`, `ParContract`,
which happen to be alphabetical in that order — go in group 2, sorted among
whatever supplemental tags the board already carries (`BCFlags`, `Generator`, a
custom tag of your own), and `OptimumResultTable` with its twenty rows goes in
group 5, below both `[Auction]` and `[Play]`.

Nothing is inserted inside a section. `[Auction]` and `[Play]` (PBN 2.1 §5.5 and
§5.6) and every `*Table` tag (§7) own the lines beneath them until the next tag
pair, so a header and its data are never separated.

Tags already on the board are placed around, never moved: a board whose
mandatory tags are out of order, or which carries a `*Table` section above its
auction, keeps them exactly where they were. Bridge Composer would reorder both.

Two earlier builds put these tags elsewhere. `--recalculate` moves one of them
to where they belong now, and refuses the other:

- Builds that wrote all four tags as one group above the auction are re-laid
  out: the table belongs below the play, so a board with an auction annotates
  differently again.
- Builds before the `[Auction]` fix ranked `Auction` alphabetically like any
  other tag, and so wrote the whole twenty-row table between `[Auction "N"]` and
  its calls. **Such a board is skipped, with a message naming it.** PBN 2.1 §5.5
  gives a tag every line below it until the next tag pair, so on that board the
  stale `[OptimumResultTable]` owns the auction's calls as well as its own twenty
  rows — replacing it would delete them. Lift the tags out of the auction by hand
  and run again; the board is left byte-for-byte untouched until you do.

## Threading

Solving is spread across cores by default. There is nothing to switch on, and
`--threads` exists mainly to turn it *down*.

```sh
bridge-solver -w -i Curated/          # every core
bridge-solver -w -i Curated/ -j 4     # four
bridge-solver -w -i Curated/ -j 1     # serial
```

**The output does not depend on the thread count.** Tables are assembled by
index rather than in the order the work finished, so every setting produces
identical bytes. If you ever see otherwise, that is a bug worth reporting.

Two details make it scale rather than merely run in parallel:

- **The unit of work is a (deal, strain) pair, not a deal.** A strain — its four
  declarers sharing one pair of caches and a chain of MTD(f) seeds — is the
  smallest piece of a table that can move to another thread without changing the
  search. Deal cost spans roughly tenfold, so scheduling whole deals ends the run
  when the slowest deal ends, with most threads long since idle.
- **Every input is one batch.** All the files named are read and planned before
  any solving starts, so a directory of two hundred one-board files spreads over
  the cores exactly as well as one file of two hundred boards.

Measured on an M4 Pro, 200 boards, annotating to a new file:

| Threads | Wall | Speedup |
|---|---|---|
| 1 | 22.55 s | 1.00x |
| 2 | 11.72 s | 1.92x |
| 4 | 6.10 s | 3.70x |
| 8 | 3.23 s | 6.98x |
| 12 | 2.75 s | **8.20x** |

Near-linear to 8, then a slower climb — the ordinary shape, and the turn is
exactly where it should be: this M4 Pro has 8 performance cores, so the last
four threads are running on efficiency cores.

**Memory is the one reason to turn it down.** Each worker holds its own pair of
caches, and a freak deal — several voids, a very deep search — can take over a
gigabyte on its own. On a corpus full of those, or on a small machine, `-j 4`
may finish where `-j 12` thrashes.

## Bridge Composer

Bridge Composer displays double-dummy analysis whenever the tags are present.
There is no per-board flag to enable it: which parts appear is its report-level
**DDA format** setting (Makeable contracts, Grid (2 rows), Grid (4 rows), Grid
(4 rows, no par), Grid (4 rows, in diagram)). Par is included in every format
except the last two, and "Makeable contracts" and "Grid (4 rows)" show the par
contract as well — which is why `OptimumScore` and `ParContract` are written
alongside the table rather than the table alone.

`--mark-verified` records provenance only. It sets bit `0x00080000` in
`[BCFlags]`, adding the tag if absent and preserving every bit already there. It
is not needed to make the analysis appear.

---

# `solver-diag` — one deal at a time

`solver-diag` reads a single deal in the reference solver's plain text layout and
prints the full table. It is also the way into the search's diagnostics, which is
what the rest of its switches are for.

```sh
solver-diag -f deal.txt
```

## The file format

```
North's hand
West's hand          East's hand
South's hand
Trump                (optional)
Leader               (optional)
```

Each hand is spades, hearts, diamonds and clubs, space-separated, with a void
written `-`. West and East share a line, separated by four or more spaces; if
there is no gap that wide, East is read from the next line instead. Trump is one
of `N S H D C` and leader one of `W N E S`; omit them and every strain and
leader is solved.

```
62 JT765 AKJ5 Q3
AT A32 943 AT962    KQ85 Q9 Q876 J75
J9743 K84 T2 K84
```

```
                          ♠ 62 ♥ JT765 ♦ AKJ5 ♣ Q3
       ♠ AT ♥ A32 ♦ 943 ♣ AT962                  ♠ KQ85 ♥ Q9 ♦ Q876 ♣ J75
                          ♠ J9743 ♥ K84 ♦ T2 ♣ K84
N  5  5  6  6  0.05 s N/A M
S  6  6  7  7  0.02 s N/A M
H  8  8  5  5  0.00 s N/A M
D  6  6  7  7  0.08 s N/A M
C  5  5  8  8  0.01 s N/A M
```

One row per strain — notrump, then spades, hearts, diamonds, clubs — and four
tricks counts across it, one per declarer in the order North, South, East, West.
So North makes 5 notrump and 8 hearts here, East 6 notrump and 5 hearts.

**Watch the seat order if you are converting from PBN.** A PBN deal string runs
clockwise from its named seat, so `N:` gives North, *East*, South, West — not the
North, West, East, South this format wants.

## Options

| Option | Effect |
|---|---|
| `-f <FILE>` | The deal to solve. **Required.** |
| `-V` | Report per-cell search performance. |
| `-X <N>` | Trace the first `N` search iterations. |
| `-C <SPEC>` | Digest the caches over a window: `STEP`, `START:END`, or `START:END:STEP`. |
| `-P` | Disable pruning. |
| `-T` | Disable the transposition table. |
| `-R` | Disable rank skipping. |

`-P`, `-T` and `-R` make the solver slower and are there to isolate which
mechanism is responsible for a disagreement, not to be used in anger. `-X` and
`-C` are the divergence-hunting tools; `fixtures/divergence/README.md` describes
the workflow they belong to.

---

# `bridge-claim` — checking claims

A claim is valid when the claiming side can take the tricks it claims however
the cards it cannot see lie, not just double dummy: declarer sees declarer's
hand and dummy, a defender their own hand and dummy. A defender's claim must
also work whatever partner holds and however partner plays. A claim the search
cannot settle within its node budget is rejected, as BBO's checker does. How
it works, and why: [the claim checker's design notes](claim-checker.md).

## Benchmarking

```sh
bridge-claim bench                  # every workload, 7 to 13 cards, every core
bridge-claim bench --preset quick   # the claims the benchmark web page runs
bridge-claim bench -j 1             # one thread, to compare per-core speed
bridge-claim bench --json           # every claim's verdict, nodes and time
```

The claims are generated from fixed seeds, so every machine checks the same
ones, and the
[benchmark page](https://bridge-craftwork.com/bridge-solver/claim-bench.html)
runs them in a browser. Verdicts (`V` valid, `X` invalid, `?` out of budget) and
node counts never depend on the machine; if yours differ from anyone else's,
that is a bug. Only the times should differ. `--workload`, `--sizes`, `--count`
and `--budget` narrow or change the set.

## Checking your own claims

`bridge-claim check claims.txt` reads one claim per line, seven fields
separated by `|`:

```text
# deal | trump | declarer | claimer | leader | tricks | plays so far
N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72 | S | N | E | E | 1 | HA H5 H3 H6 HK H9 H7 HJ
```

The deal is the hands at the start of play, or an ending with four equal
hands. Trump is `S`, `H`, `D`, `C` or `NT`; seats are `N`, `E`, `S`, `W`, and
the leader leads the first card of the plays. `tricks` counts only tricks
still to play, including one in progress. Here East, having cashed two hearts
against four spades, claims one more with the ♦A. The answer is `Invalid`:
as far as East can see, declarer may be void in diamonds and ruff it.

---

# Building from source

```sh
cargo build --release --features cli
```

puts both binaries in `target/release/`. The `cli` feature is what pulls in
argument parsing; without it the crate builds as a library only.

Working *on* this repository is different — it depends on sibling crates through
git, with local path overrides that bare cargo will silently ignore or, worse,
silently bake into `Cargo.lock`. Use `./dev-build.sh` there, and see `CLAUDE.md`
for why.

# Performance

A full 20-cell table for an ordinary deal takes **about 110 ms** single-threaded
on an M4 Pro, and roughly a tenth of that across twelve cores. Cost varies about
tenfold deal to deal: over a ten-board sample the range was 15 ms to 263 ms.

Freak deals are a different matter. A hand with several voids can search for
**tens of seconds** and take over a gigabyte — 39.7 s and 1.6 GB for the worst
in the corpus. These are rare enough not to affect a batch's total but they will
be the deal you are waiting on.

`bench/results/release-profile.md` is the full record: how this port compares
with the C++ reference and with DDS 2.9, what has been tried, and what was found
to be worthless.
