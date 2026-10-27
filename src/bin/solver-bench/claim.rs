//! `solver-bench claim`: the claim checker over fixed random workloads.
//!
//! Each workload is a seeded stream of random endings, so a run is the same
//! claims every time and verdict strings can be diffed between builds. The
//! verdicts must never change where both builds decide; a change that turns
//! `V` into `X` (or back) is a bug, not a speed-up. `?` is a claim the budget
//! ran out on, which the checker rejects.
//!
//! The workloads:
//!
//! * `strong-dd` - declarer and dummy hold the high cards, and declarer claims
//!   the double-dummy number of tricks. Closest to a real claim.
//! * `decl-dd`, `def-dd` - random hands, declarer or the opening leader
//!   claiming exactly the double-dummy number. The hard case: right at the
//!   boundary, where a claim fails only because the claimer must guess.
//! * `decl-all` - random hands, declarer on lead claiming every remaining
//!   trick: the workload as Adam Wildavsky described timing his single-dummy
//!   solver on (bridge-dev, 2026-10-27). On random hands nearly every such
//!   claim fails double dummy too, so this mostly times the refutation.
//! * `rest-dd` - "the rest are mine": strong hands, declarer on lead, dealt
//!   again until double dummy takes every trick, then claiming them all. The
//!   claim a student actually makes, and the one that needs a proof.

use std::time::Instant;

use bridge_solver::cards::rank_of;
use bridge_solver::{ClaimPosition, ClaimVerdict, Hands, Solver, NORTH, NUM_SEATS, NUM_SUITS};

/// One kind of claim.
#[derive(Clone, Copy)]
struct Workload {
    name: &'static str,
    /// Deal declarer's side the high cards.
    strong: bool,
    /// The opening leader claims, rather than declarer.
    defender: bool,
    /// Claim every trick rather than the double-dummy number, with declarer
    /// on lead.
    all: bool,
    /// Deal again until double dummy takes every trick.
    makeable: bool,
}

const WORKLOADS: [Workload; 5] = [
    Workload {
        name: "strong-dd",
        strong: true,
        defender: false,
        all: false,
        makeable: false,
    },
    Workload {
        name: "decl-dd",
        strong: false,
        defender: false,
        all: false,
        makeable: false,
    },
    Workload {
        name: "def-dd",
        strong: false,
        defender: true,
        all: false,
        makeable: false,
    },
    Workload {
        name: "decl-all",
        strong: false,
        defender: false,
        all: true,
        makeable: false,
    },
    Workload {
        name: "rest-dd",
        strong: true,
        defender: false,
        all: true,
        makeable: true,
    },
];

/// Deals tried for a `makeable` case before giving up.
const MAX_REDEALS: usize = 10_000;

/// A 64-bit LCG; good enough to shuffle a deck, and the same everywhere.
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) % n as u64) as usize
    }
}

/// A claim to check: the position, and the tricks claimed.
struct Case {
    position: ClaimPosition,
    tricks: usize,
}

fn case(w: Workload, size: usize, rng: &mut Lcg) -> Result<Case, String> {
    for _ in 0..MAX_REDEALS {
        if let Some(c) = deal_case(w, size, rng)? {
            return Ok(c);
        }
    }
    Err(format!(
        "{}: no makeable deal of {size} in {MAX_REDEALS} tries",
        w.name
    ))
}

/// One random claim, or `None` if `w.makeable` and double dummy falls short.
fn deal_case(w: Workload, size: usize, rng: &mut Lcg) -> Result<Option<Case>, String> {
    let mut deck: Vec<usize> = (0..52).collect();
    for i in (1..52).rev() {
        let j = rng.below(i + 1);
        deck.swap(i, j);
    }
    let mut cards = deck[..size * NUM_SEATS].to_vec();
    let declarer = rng.below(NUM_SEATS);
    let leader = if w.all {
        declarer
    } else {
        (declarer + 1) % NUM_SEATS
    };
    let trump = rng.below(NUM_SUITS + 1);

    let mut hands = Hands::new();
    if w.strong {
        // Rank plus noise, highest first; the top half goes to declarer's side.
        let mut keyed: Vec<(usize, usize)> = cards
            .drain(..)
            .map(|c| (rank_of(c) + rng.below(6), c))
            .collect();
        keyed.sort_by_key(|&(k, _)| std::cmp::Reverse(k));
        let order = [
            declarer,
            (declarer + 2) % NUM_SEATS,
            (declarer + 1) % NUM_SEATS,
            (declarer + 3) % NUM_SEATS,
        ];
        for (i, &(_, c)) in keyed.iter().enumerate() {
            let seat = if i < 2 * size {
                order[i % 2]
            } else {
                order[2 + i % 2]
            };
            hands[seat].add(c);
        }
    } else {
        for (i, &c) in cards.iter().enumerate() {
            hands[i % NUM_SEATS].add(c);
        }
    }

    let claimer = if w.defender { leader } else { declarer };
    let double_dummy = |hands: Hands| {
        let ns = Solver::new(hands, trump, leader).solve() as usize;
        if claimer % 2 == NORTH % 2 {
            ns
        } else {
            size - ns
        }
    };
    let tricks = if w.all { size } else { double_dummy(hands) };
    if w.makeable && double_dummy(hands) < size {
        return Ok(None);
    }
    let position = ClaimPosition::from_play(&hands, trump, declarer, claimer, leader, &[])
        .map_err(|e| e.to_string())?;
    Ok(Some(Case { position, tricks }))
}

/// Run every workload at every size and print one line each, then a total.
pub fn claim_report(
    sizes: &[usize],
    count: usize,
    budget: u64,
    only: Option<&str>,
) -> Result<(), String> {
    if let Some(name) = only {
        if !WORKLOADS.iter().any(|w| w.name == name) {
            let names: Vec<&str> = WORKLOADS.iter().map(|w| w.name).collect();
            return Err(format!(
                "unknown workload {name:?}; one of {}",
                names.join(", ")
            ));
        }
    }
    if let Some(&bad) = sizes.iter().find(|&&s| s == 0 || s > 13) {
        return Err(format!("size {bad} is not 1 to 13 cards per hand"));
    }
    println!("budget {budget} nodes, {count} claims per row");
    println!(
        "{:<10} {:>4} {:>4} {:>4} {:>4} {:>12} {:>10} {:>9}  verdicts",
        "workload", "size", "V", "X", "?", "nodes", "ms", "ms/claim"
    );
    let (mut all_nodes, mut all_secs) = (0u64, 0f64);
    for (wi, w) in WORKLOADS.iter().enumerate() {
        if only.is_some_and(|name| name != w.name) {
            continue;
        }
        for &size in sizes {
            // Seeded per workload and size, so adding a size or a workload
            // leaves every other row's claims unchanged.
            let mut rng = Lcg(0x5eed ^ ((wi as u64) << 32) ^ size as u64);
            let (mut valid, mut invalid, mut undecided) = (0, 0, 0);
            let (mut nodes, mut secs) = (0u64, 0f64);
            let mut verdicts = String::with_capacity(count);
            for _ in 0..count {
                let c = case(*w, size, &mut rng)?;
                let start = Instant::now();
                let result = c
                    .position
                    .check(c.tricks, budget)
                    .map_err(|e| e.to_string())?;
                secs += start.elapsed().as_secs_f64();
                nodes += result.nodes;
                verdicts.push(match result.verdict {
                    ClaimVerdict::Valid => {
                        valid += 1;
                        'V'
                    }
                    ClaimVerdict::Invalid => {
                        invalid += 1;
                        'X'
                    }
                    ClaimVerdict::Undecided => {
                        undecided += 1;
                        '?'
                    }
                });
            }
            all_nodes += nodes;
            all_secs += secs;
            println!(
                "{:<10} {size:>4} {valid:>4} {invalid:>4} {undecided:>4} {nodes:>12} {:>10.1} {:>9.2}  {verdicts}",
                w.name,
                secs * 1000.0,
                secs * 1000.0 / count as f64
            );
        }
    }
    println!("total nodes {all_nodes}, {:.1} ms", all_secs * 1000.0);
    Ok(())
}
