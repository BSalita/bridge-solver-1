# The claim checker: design notes

How `src/claim.rs` decides whether a bridge claim is valid, why it is built the
way it is, what was measured, and what was tried and dropped. Read this before
changing the claim search. The code comments explain *how*; this explains
*why*, and records the dead ends, so that nobody has to rediscover them.

Contents: [The question](#the-question) · [The model](#the-model) ·
[Defender claims](#defender-claims) · [The search](#the-search) ·
[Making it fast](#making-it-fast) · [Tried and dropped](#tried-and-dropped) ·
[A bug worth remembering](#a-bug-worth-remembering) · [Testing](#testing) ·
[The benchmark](#the-benchmark) · [Known limits](#known-limits) ·
[Credits](#credits)

---

## The question

When a player claims, "can the claiming side take N tricks?" has to be answered
**without assuming where the unseen cards are**. A double-dummy solver gives the
wrong answer: it sees every card, so it happily "proves" a claim that only works
because a finesse is onside or a suit breaks. A claim is valid only if the
claimer has a line that wins N tricks *however* the unseen cards lie.

The definition used here is the one BBO's claim checker uses, as Lorand Dali
described it on the bridge-dev list (see [Credits](#credits)):

- a double-dummy-style search for "can declarer always make N tricks?";
- the hidden hands are not fixed: a hidden hand may play any card, except from
  a suit it has already shown out of;
- there is a compute budget, and a claim that exhausts it is rejected, even if
  it might be valid.

## The model

**The claimer sees two hands**: their own and dummy's. The other two hands'
remaining cards form one **pool**, split between those two seats in a way the
claimer does not know.

When a pooled seat is to play it may play **any pool card**, provided some split
of what is left is still consistent with:

- each pooled seat's remaining hand size, and
- every suit each pooled seat has shown out of, in this search or in the play
  so far. Playing off-suit marks the seat void in the led suit.

Consistency is a two-bin assignment problem, and Hall's condition settles it
exactly and cheaply (`can_split`). The cards only seat A can hold must fit in A,
likewise for B, and no card may be barred from both.

**Choosing the layout lazily is the same as fixing it in advance.** The
claimer's decisions depend only on cards already seen. So a strategy that beats
every lazily chosen defence also beats every fixed layout. The reverse holds
too, because each lazy line of play ends in some fixed layout. "Valid" here
therefore means exactly: the claimer has one line that takes N tricks against
every layout consistent with what has been seen. That is the property a claim
needs, and it is why the model is sound rather than approximate.

The search is exhaustive within its budget, so it is **perfect** in the sense
Adam Wildavsky used on the list: it accepts any claim with a sure-trick
solution, and only such claims.

## Defender claims

BBO supports claims by a defender. The thread did not say how it models them,
and we have not verified what BBO does.

A defender sees their own hand and dummy, so the pool is **declarer's hand and
the defender's partner's**. The claimer cannot see partner's cards and cannot
steer partner's play. So the search gives **partner's choices to the
adversary**, along with declarer's and dummy's, while **partner's tricks still
count** for the claiming side.

A defender's claim is therefore valid only if it works whatever partner holds
and however partner plays.

**Considered and rejected:** assuming partner plays sensibly for the defence.
That accepts more claims, but "sensibly" has no sound definition: partner
cannot see the claimer's hand either. Modelling it properly means a
three-information-set game, much harder than this search. The strict model is
provably sound: it never accepts a claim that a partner could fail to cooperate
with. It was chosen deliberately (October 2026), and every other part of the
search treats both kinds of claim identically through `Roles`.

The roles in brief:

| | Visible | Controlled by the claimer | Pooled |
|---|---|---|---|
| Declarer (or dummy) claims | declarer, dummy | declarer, dummy | both defenders |
| A defender claims | that defender, dummy | that defender only | declarer, partner |

## The search

An AND/OR search: "can the claiming side take `need` of the remaining tricks?"

- **At a controlled seat** (OR), the claimer needs one card that works.
- **At any other seat** (AND), the adversary needs one card that beats the
  claim. For a pooled seat, that includes choosing the layout.
- A **transposition table at trick boundaries** stores `(lo, hi)` per position:
  claims of `lo` tricks or fewer are proven, claims of `hi` or more refuted. One
  entry answers every target, which `max_claim` exploits.
- **Equivalent cards** are collapsed: of a run of touching cards held by one
  visible hand, only one is tried. For pooled cards this needs care; see
  [A bug worth remembering](#a-bug-worth-remembering).
- **A node budget** (`DEFAULT_CLAIM_BUDGET`, 2M nodes) bounds the work. Running
  out gives `Undecided`, which callers treat as rejection, as BBO does. It
  never gives a false `Invalid`.

The claim search is **separate from the double-dummy search** in `search.rs`.
That search must stay node-for-node in lock-step with the C++ reference (see
`CLAUDE.md`), so the claim checker never changes it. It only *calls* it, for
the layout test below. The rules of play (legal cards, trick winner, play
validation) live in `play.rs` and are shared with the card-play analysis.
`search.rs` keeps its own copy of the trick-winner test, on its hot path.

## Making it fast

Measured on a fixed set of 270 random claims: declarer and defender claims at
8, 10 and 13 cards per hand, each claiming *exactly* the double-dummy number
(the hard boundary case). The budget was 20M nodes, so most claims could finish.

| Step | Time | Undecided |
|---|---|---|
| Plain search, after the run-equivalence fix | 90 s | 57 of 270 |
| + layout test (below) | 33 s | 19 |
| + more layouts at the root | 25 s | — |
| + cashing bound over both hands | 15 s | 7 |
| + fast hasher | 11 s | 7 |

The steps, in the order they were added:

**1. Refute with a layout first (`Layouts`, `Search::layout_refutes`).** A
claim must hold against every layout. So if double dummy holds the claiming
side below its target on **any one** layout, the claim is invalid. The
double-dummy solver is fast, so before searching we try:
- the eight layouts that give one pooled seat a whole suit (bad breaks), then
- 64 random layouts.

Measured first, as an experiment: 80–90% of invalid claims are refuted by a
single layout within 64 random tries, and about 55% within 8. The rest fail only
because the claimer would have to *guess*: every individual layout is
double-dummy makeable, so this test cannot catch them, and the search must.

This only ever answers "refuted" or "don't know", so it can never make a valid
claim look invalid. Its nodes count toward the budget.

**2. A cashing bound over both of declarer's hands (`Search::cash`).** Valid
claims are the expensive kind, because proving one means beating every
adversary choice. The original sure-trick bound counted winners in the leading
hand only. `cash` is a small single-player search over declarer's two visible
hands:
- it leads only winners, so the opponents can never win a trick by following
  suit, only by ruffing;
- it crosses between hands by overtaking;
- it chooses partner's follows and discards;
- it draws trumps first, pessimistically assuming one hand holds all of the
  opponents' trumps, so each round draws only one.

Every plan it finds works whatever the adversary does. That is the property
that makes it a sound lower bound, even though it tries only some plans. With the
layout test already in place, it took 30 strong-hand claims at 13 cards from
8.3 s to 65 ms.

**3. Bounds from DTAC (October 2026), all ideas only, no code.** These moved
the full benchmark by −9.4% nodes and settled two undecided claims.
- **Length winners.** `cash` tracks the opponents' outstanding cards
  pessimistically: each round of a suit removes only their *lowest* card of
  it, as if one hand held them all and kept the high ones. Their real holding
  is then never longer and never higher, suit by suit, so whatever wins
  against the tracked set wins for real. Small cards count once the suit is
  exhausted (the 2 of AKQ2).
- **Ruffs.** With no opponent trump left, partner may ruff a card of a suit
  it is void in, to win a trick with a loser or to cross between hands.
- **Opponents on lead (`claimer_sure_after_lead`).** For declarer's claims:
  - every suit the leader may hold must be won for certain, either by a top
    card that cannot be ruffed or by a ruff that cannot be overruffed;
  - the bound is the worst case over those suits of 1 + `cash` from the hand
    that won;
  - if any suit cannot be won for certain, the bound is 0.

  This is DTAC's "opponents lead" AND node.

**4. A fast hasher.** The transposition tables use a multiply-rotate hash
(rustc's `FxHasher` scheme) instead of SipHash: 13% faster, with identical node
counts.

**Move ordering** matters too. On lead, the claimer cashes winners first and
the adversary leads high. Later in a trick, a seat wins cheaply when that helps
its side and plays low otherwise. Ordering never changes a verdict.

## Tried and dropped

Measured and found to be worth nothing; please don't re-add without new
evidence:

- **Layout tests inside the search.** Running the double-dummy layout test at
  inner trick boundaries (at 4, 5 or 7+ tricks left), with "killer" layouts
  that had refuted a sibling, and with fresh random layouts per node.
  - It barely reduced nodes and cost time.
  - The reason: a claim that no layout refutes at the start fails by a guess,
    and stays that way deeper down.
  - The killer list did nothing at all, because a layout that refutes at the
    root ends the search there.
  - So the test runs once, before the search.
- **More than 64 random layouts.** 256 and 1,024 gave the same node totals as
  64 plus the bad breaks.
- **DTAC's "claim all but one" mode** as a bound. Bethe's own timings average
  0.75 s with a 140 s maximum, and the exact search already handles giving up a
  trick.
- **DTAC's minimum-holding formula**: (combined length) ÷ (cards per defender),
  in integer division, as the fewest cards a defender must hold in a suit. It is
  **unsound** when combined length equals cards per defender: it gives 1 where
  the truth is 0, since one defender could hold only that suit. A side-suit
  winner would then be counted as safe from a ruff when it is not. The correct
  value is max(0, L − n), and ours comes from `can_split`.
- **Treating low cards as equals** (a speed-up Adam Wildavsky described for his
  solver). It makes results inexact, so we don't. The exact equivalence is
  described next.

## A bug worth remembering

The first version collapsed touching **pool** cards (say ♠KQJ) into one move,
as is correct for a visible hand. It is wrong for the pool. The pool spans two
seats, so the card played decides what the *other* pooled seat can still do in
the same trick:
- if one hand plays the J, the other can still beat it with the K;
- if it plays the K, the other can't.

The fix (`push_run_ends`): when the other pooled seat has yet to play to the
trick, offer the run's top card, its bottom card and one from the middle. Every
middle card offers the same, so one stands for all of them. Once the other
pooled seat has played, a single card per run is enough again.

The bug could only make the checker **accept** claims it should have rejected,
for declarer claims too. The property test of the time did not catch it. That
is why the [reference test](#testing) exists.

## Testing

The shortcuts (equivalent cards, bounds, the layout test) exist only for speed.
So the main guard is a **reference comparison**: the same search with every
shortcut off, trying every card, must give the same verdict.

- `shortcuts_never_change_a_verdict`: 400 random endings of 2–4 cards.
  - It covers every claimer, every strain, and claims made mid-trick (up to
    three cards of the trick already played).
  - Each claim is also run with the layout test switched on at every size.
- `bounds_hold_where_ruffs_decide`: 300 endings dealt from only two or three
  suits, one of them trumps, with a defender on lead. Ruffing and overruffing
  positions are rare in four-suit endings. A deliberately unsound ruff rule
  passed the first test and fails this one.
- Hand-built cases for each behaviour: a finesse is not a claim; a known void
  makes it one; a defender can't count on partner; and so on.
- `a_claim_never_exceeds_double_dummy`, plus a check that some claims fall
  strictly below it, so the comparison isn't trivially satisfied.

**Every new shortcut has been checked by breaking it on purpose** and
confirming a test fails. Do the same for the next one. Breakages caught so far:
- overcounting `cash`;
- calling a tie a refutation;
- removing the opponents' whole suit per round instead of the lowest card;
- leading side suits while trumps are out;
- treating a ruffable winner as safe when a defender leads.

## The benchmark

Fixed claims, identical everywhere: `src/claim_bench.rs`.

- **Workloads.**
  - `rest-dd`: strong hands, declarer claims the rest, double dummy makes it.
    This is the claim a player actually makes.
  - `strong-dd`: strong hands, declarer claims the double-dummy number.
  - `decl-dd`, `def-dd`: random hands, declarer or the opening leader claiming
    exactly the double-dummy number.
  - `decl-all`: random hands, declarer claims every trick; nearly all are bad
    claims.
- **Presets.** `quick` and `full` are shared by the web page and the native
  tool.
- **Per-claim seeds.** Every claim is generated from its own seed (workload
  name, size, index), so any claim can be produced alone, in any order, on any
  platform. That is what lets `bridge-claim bench` spread claims over every
  core, and the web page run them one at a time, while testing exactly the
  same claims.
- **Determinism is a correctness check.** Verdicts and node counts depend only
  on the claim and the budget.
  - `web/src/lib/fixtures/claim-bench-v1.json` holds the native results for
    the full preset.
  - The web page checks every claim against it; a difference means a bug in
    the browser build, not a slow device.
  - `claim_bench::tests::matches_the_web_fixture` fails when the search changes
    without the fixture being regenerated with `scripts/claim-bench-fixture.sh`.
- **Only the check is timed**, not claim generation, which runs double-dummy
  solves.

**Where to run it:**
- `bridge-claim bench` runs the native tool on every core. It ships in each
  release.
- [`claim-bench.html`](https://bridge-craftwork.com/bridge-solver/claim-bench)
  runs it in a browser, single-threaded.
- In wasm under Node, checking is about 1.4× slower than native on one thread.
- A finished web run sends one anonymous `claimbench` record (see
  `functions/t.ts` and the privacy page), which feeds a panel on the stats page.

**Comparing with other solvers.** Adam Wildavsky reported his exhaustive
single-dummy solver's average times for "random positions where the goal is all
of the remaining tricks", on an 18-core M5 Max: 7 cards 50 ms, 8 cards 170 ms,
9 cards 190 ms, 10 cards 9.6 s, 11 cards 137.6 s. Our `decl-all` workload
decides all such claims in about a millisecond each on one thread. That is not
a fair comparison:
- random "all the tricks" claims mostly fail double dummy too, so they are
  rejected quickly by the layout test;
- we don't know how his positions were generated, or whether his solver
  computes the number of sure tricks or only yes/no.

`rest-dd` is the fairer workload.

## Known limits

- **Undecided claims are mostly hard valid ones.** Proving that something holds
  against every adversary line is costly when the claim needs more than
  cashing. An example: South claims 1 trick of 10 with clubs as trumps, while
  East holds ♣AQ862 over North's ♣K54. It is valid, but took 285M nodes. Such
  claims are rare in practice and are rejected, as on BBO.
- **`max_claim` is slow early in a hand.** It tests 1, 2, 3… tricks in turn,
  and proving small claims when an opponent is on lead is the expensive kind.
  A specific claim, the normal case, is unaffected.
- **No ruff while trumps are out.** Ruffing safety would need a *lower* bound
  on the opponents' holding, which the pessimistic tracking cannot give. So
  side suits wait until trumps are drawn.
- **Defender claims use no cashing bound across both hands**, since partner is
  pooled. They rely on the leading hand's top winners, the layout test, and the
  search.

## Credits

The claim checker's design draws on a bridge-dev discussion (groups.io, thread
"BBO claim", October 2026) and on earlier published work:

- **Lorand Dali** described how BBO's claim checker works. That description is
  the model here: unfixed hidden hands, show-outs respected, the special case
  of claiming all tricks, and a node budget that rejects.
- **Soren Hein** asked the question that started the thread.
- **Gal Hegedus** pointed to DTAC.
- **Paul M. Bethe**, *DTAC: A method for planning to claim in Bridge*, MS
  thesis, New York University, 2010; code at
  [pmbethe09/DTAC](https://github.com/pmbethe09/DTAC) (Apache-2.0). The source
  of the length-winner, ruff and opponents-on-lead bounds. Ideas only; no DTAC
  code is used.
- **Adam Wildavsky** described his single-dummy solver and its timings, the
  reference point for the benchmark discussion above.
- The double-dummy search used by the layout test is this repository's port of
  **Hanhong Xue**'s [bridge-solver](https://github.com/macroxue/bridge-solver).
