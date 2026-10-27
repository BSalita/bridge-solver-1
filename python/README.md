# bridge-solver

Contract bridge analysis for Python:

- a fast **double-dummy solver**: the full table of tricks for every declarer
  and strain;
- **par** scoring;
- **card-by-card play analysis**, with what each card played cost;
- a **claim checker**. It decides whether a claim holds *however the unseen
  cards lie*, not just double dummy, the way BBO's checker does. It handles
  claims by declarer and by a defender.

It's the engine behind the
[bridge-solver](https://github.com/bridge-craftwork/bridge-solver) project.
It's written in Rust and comes as a prebuilt wheel for Linux, macOS and
Windows, so there's nothing to compile.

```sh
pip install bridge-solver
```

## Quick start

```python
import bridge_solver as bs

deal = "N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72"

bs.dd_table(deal)["N"]          # {'C': 8, 'D': 7, 'H': 8, 'S': 10, 'NT': 9}
bs.par(deal, "None")            # {'score_ns': 420, 'optimum_score': 'NS 420', 'par_contract': 'NS 4S='}

# North declares 4S. East has cashed the HA and HK. Can East claim a third
# trick with the DA? Not as far as East can see: declarer may be void in
# diamonds and ruff.
plays = "HA H5 H3 H6 HK H9 H7 HJ"
bs.check_claim(deal, "S", "N", 3, plays=plays, claimer="E")["verdict"]   # 'invalid'
bs.max_claim(deal, "S", "N", plays=plays, claimer="E")["guaranteed"]     # 2
```

## Conventions

- **Deals** are PBN: `"N:<north> <east> <south> <west>"`. Each hand is
  `spades.hearts.diamonds.clubs`, e.g. `"AKQT3.J6.KJ42.95"`.
- **Seats** are `"N"`, `"E"`, `"S"`, `"W"`.
- **Strains** are `"C"`, `"D"`, `"H"`, `"S"`, `"NT"`.
- **Cards** are a suit letter then a rank: `"SA"`, `"HT"`.
- **Plays** start with the opening lead. Pass them as a list (`["HA", "H5"]`)
  or as one string (`"HA H5"`). The leader defaults to declarer's left-hand
  opponent.
- **Errors**: anything the solver can't use raises
  `bridge_solver.BridgeSolverError`, a `ValueError`.
- **Threads**: every call releases the GIL while it computes, so threads
  solve in parallel.

## Reference

| Function | Returns |
|---|---|
| `dd_table(deal)` | `{seat: {strain: tricks}}`, the tricks each seat takes as declarer |
| `par(deal, vulnerable="None")` | par score and contracts; `vulnerable` is `"None"`, `"NS"`, `"EW"` or `"All"` |
| `solve(deal, trump, leader)` | North-South's double-dummy tricks with `leader` on lead |
| `check_claim(deal, trump, declarer, tricks, *, plays, leader, claimer, budget)` | `{"verdict": "valid"/"invalid"/"undecided", ...}` |
| `max_claim(deal, trump, declarer, *, plays, leader, claimer, budget)` | `{"guaranteed": n, "undecided": bool, ...}` |
| `PlayAnalyzer().trace(deal, trump, declarer, plays)` | every card played, with its double-dummy cost |
| `PlayAnalyzer().alternatives(deal, trump, declarer, plays, card)` | every card that could have been played there, with its cost |
| `PlayAnalyzer().optimal_line(deal, trump, declarer, plays, start)` | a double-dummy-perfect continuation |

### Claims

- **`tricks` is a total for the whole hand.** In `check_claim` it's the
  claiming side's total, counting tricks already won, as BBO shows it.
- **What "valid" means.** A claim is valid only if the claimer has a line that
  wins those tricks against every layout of the cards they can't see.
  Declarer sees their own hand and dummy. A defender sees their own hand and
  dummy, and their claim must also work whatever partner holds and however
  partner plays.
- **The node budget.** The search has a budget, 2,000,000 nodes by default.
  When it runs out the verdict is `"undecided"`; treat that as rejected, as
  BBO does. Typical claims settle in about a millisecond.
- **Both sides.** `max_claim` gives the most tricks the claimer's side is sure
  of. Call it once with declarer and once with a defender as `claimer`.

How the checker works, and why: [design notes](https://github.com/bridge-craftwork/bridge-solver/blob/main/docs/claim-checker.md).
You can time it in your browser on the
[benchmark page](https://bridge-craftwork.com/bridge-solver/bench.html#claims),
and try claims on your own hands on the
[claim checker page](https://bridge-craftwork.com/bridge-solver/claim.html).

## Licence

MIT OR Apache-2.0. The double-dummy search is a port of Hanhong Xue's
[bridge-solver](https://github.com/macroxue/bridge-solver). See the
repository for full attribution.
