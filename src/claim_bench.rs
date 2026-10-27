//! Fixed claim-checking workloads, for benchmarking the claim checker.
//!
//! Every claim is generated from its own seed — its workload, its hand size
//! and its index — so any one of them can be produced alone, in any order, on
//! any platform, and always comes out the same. That is what lets the native
//! `bridge-claim` tool spread claims across threads and the web page run them
//! one at a time, and still test exactly the same claims as each other.
//!
//! The search is deterministic too: a claim's verdict and node count depend
//! only on the claim and the budget, never on the machine. So two runs whose
//! verdicts or nodes differ have found a bug, and only their times may differ.
//!
//! The workloads, by name:
//!
//! * `rest-dd` - "the rest are mine": declarer and dummy hold the high cards,
//!   declarer is on lead, and double dummy takes every trick; declarer claims
//!   them all. The claim a player actually makes, and one that needs a proof.
//! * `strong-dd` - declarer's side holds the high cards and claims the
//!   double-dummy number of tricks, with the opening lead not yet made.
//! * `decl-dd` and `def-dd` - random hands, declarer or the opening leader
//!   claiming exactly the double-dummy number. The hard case: right at the
//!   boundary, where a claim fails only because the claimer must guess.
//! * `decl-all` - random hands, declarer on lead claiming every trick. On
//!   random hands nearly every such claim fails double dummy as well, so this
//!   mostly measures how fast a bad claim is rejected.

use std::fmt;

use super::bridge_solver::Solver;
use super::cards::rank_of;
use super::claim::{ClaimError, ClaimPosition, ClaimVerdict};
use super::hands::Hands;
use super::types::*;

/// One kind of benchmark claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Workload {
    /// Short name, as given on a command line.
    pub name: &'static str,
    /// One line saying what the claims are.
    pub description: &'static str,
    /// Deal declarer's side the high cards.
    strong: bool,
    /// The opening leader claims, rather than declarer.
    defender: bool,
    /// Claim every trick, with declarer on lead, rather than the double-dummy
    /// number with the defence on lead.
    all: bool,
    /// Deal again until double dummy takes every trick.
    makeable: bool,
}

/// Every workload, in the order reports list them.
pub const WORKLOADS: [Workload; 5] = [
    Workload {
        name: "rest-dd",
        description: "strong hands, declarer claims the rest, double dummy makes them",
        strong: true,
        defender: false,
        all: true,
        makeable: true,
    },
    Workload {
        name: "strong-dd",
        description: "strong hands, declarer claims the double-dummy number",
        strong: true,
        defender: false,
        all: false,
        makeable: false,
    },
    Workload {
        name: "decl-dd",
        description: "random hands, declarer claims exactly the double-dummy number",
        strong: false,
        defender: false,
        all: false,
        makeable: false,
    },
    Workload {
        name: "def-dd",
        description: "random hands, the opening leader claims exactly the double-dummy number",
        strong: false,
        defender: true,
        all: false,
        makeable: false,
    },
    Workload {
        name: "decl-all",
        description: "random hands, declarer on lead claims every trick",
        strong: false,
        defender: false,
        all: true,
        makeable: false,
    },
];

/// The workload called `name`.
pub fn workload(name: &str) -> Option<&'static Workload> {
    WORKLOADS.iter().find(|w| w.name == name)
}

/// A named set of rows: every listed workload at every listed size, `count`
/// claims each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    /// Short name, as given on a command line.
    pub name: &'static str,
    /// Workload names, in report order.
    pub workloads: &'static [&'static str],
    /// Cards per hand.
    pub sizes: &'static [usize],
    /// Claims per workload and size.
    pub count: u64,
}

/// The presets the web page and `bridge-claim` share, so a result from one
/// can be set against a result from the other.
///
/// `quick` is what the web page runs by default: well under a second of
/// checking natively, a few seconds in a laptop's browser, longer on a phone. `full` is every workload at 7 to 13 cards; in a browser
/// that is minutes, and it is what the native tool runs on all cores.
pub const PRESETS: [Preset; 2] = [
    Preset {
        name: "quick",
        workloads: &["rest-dd", "strong-dd", "decl-dd", "def-dd", "decl-all"],
        sizes: &[8, 10, 13],
        count: 10,
    },
    Preset {
        name: "full",
        workloads: &["rest-dd", "strong-dd", "decl-dd", "def-dd", "decl-all"],
        sizes: &[7, 8, 9, 10, 11, 12, 13],
        count: 30,
    },
];

/// The preset called `name`.
pub fn preset(name: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.name == name)
}

/// Deals tried for a `makeable` claim before giving up.
const MAX_REDEALS: usize = 10_000;

/// Why a benchmark claim could not be generated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BenchError {
    /// Hand size outside 1 to 13 cards.
    BadSize(usize),
    /// No deal that double dummy makes turned up in [`MAX_REDEALS`] tries.
    NoMakeableDeal {
        /// The workload.
        workload: &'static str,
        /// Cards per hand.
        size: usize,
    },
    /// The generated position was rejected; a bug in the generator.
    Position(ClaimError),
}

impl fmt::Display for BenchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BenchError::BadSize(n) => write!(f, "{n} is not a hand size of 1 to 13 cards"),
            BenchError::NoMakeableDeal { workload, size } => write!(
                f,
                "{workload}: no deal of {size} cards that double dummy makes in {MAX_REDEALS} tries"
            ),
            BenchError::Position(e) => write!(f, "generated an invalid position: {e}"),
        }
    }
}

impl std::error::Error for BenchError {}

/// A claim to check, and what it claims.
#[derive(Debug, Clone)]
pub struct BenchClaim {
    /// The position, before any card is played.
    pub position: ClaimPosition,
    /// The deal, for reporting.
    pub hands: Hands,
    /// Tricks claimed.
    pub tricks: usize,
}

/// A splitmix64 stream: seeded from a claim's identity, the same everywhere.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Claim number `index` of `workload` with `size` cards per hand.
pub fn bench_claim(workload: &Workload, size: usize, index: u64) -> Result<BenchClaim, BenchError> {
    if size == 0 || size > TOTAL_TRICKS {
        return Err(BenchError::BadSize(size));
    }
    // The workload's position in the list is not part of the seed, so that
    // adding or reordering workloads leaves every existing claim unchanged.
    let name_hash = workload
        .name
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
        });
    let mut rng = Rng(name_hash ^ ((size as u64) << 56) ^ index);
    for _ in 0..MAX_REDEALS {
        if let Some(claim) = deal(workload, size, &mut rng)? {
            return Ok(claim);
        }
    }
    Err(BenchError::NoMakeableDeal {
        workload: workload.name,
        size,
    })
}

/// One random claim, or `None` if the workload wants a deal double dummy
/// makes and this is not one.
fn deal(w: &Workload, size: usize, rng: &mut Rng) -> Result<Option<BenchClaim>, BenchError> {
    let mut deck: Vec<usize> = (0..TOTAL_CARDS).collect();
    for i in (1..TOTAL_CARDS).rev() {
        let j = rng.below(i + 1);
        deck.swap(i, j);
    }
    let cards = &deck[..size * NUM_SEATS];
    let declarer = rng.below(NUM_SEATS);
    let leader = if w.all {
        declarer
    } else {
        (declarer + 1) % NUM_SEATS
    };
    let trump = rng.below(NUM_SUITS + 1);

    let mut hands = Hands::new();
    if w.strong {
        // Rank plus noise, highest first: the top half goes to declarer's side.
        let mut keyed: Vec<(usize, usize)> = cards
            .iter()
            .map(|&c| (rank_of(c) + rng.below(6), c))
            .collect();
        keyed.sort_by_key(|&(k, c)| (std::cmp::Reverse(k), c));
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
    let double_dummy = || {
        let ns = Solver::new(hands, trump, leader).solve() as usize;
        if claimer % 2 == NORTH % 2 {
            ns
        } else {
            size - ns
        }
    };
    let tricks = if w.all { size } else { double_dummy() };
    if w.makeable && double_dummy() < size {
        return Ok(None);
    }
    let position = ClaimPosition::from_play(&hands, trump, declarer, claimer, leader, &[])
        .map_err(BenchError::Position)?;
    Ok(Some(BenchClaim {
        position,
        hands,
        tricks,
    }))
}

/// The letter a report shows for a verdict: `V`alid, `X` invalid, `?` out of
/// budget.
pub fn verdict_letter(verdict: ClaimVerdict) -> char {
    match verdict {
        ClaimVerdict::Valid => 'V',
        ClaimVerdict::Invalid => 'X',
        ClaimVerdict::Undecided => '?',
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_do_not_depend_on_order() {
        let w = workload("decl-dd").expect("exists");
        let later = bench_claim(w, 8, 5).expect("claim");
        let _ = bench_claim(w, 8, 4).expect("claim");
        let again = bench_claim(w, 8, 5).expect("claim");
        assert_eq!(format!("{:?}", later.hands), format!("{:?}", again.hands));
        assert_eq!(later.tricks, again.tricks);
    }

    #[test]
    fn rest_dd_claims_every_trick_double_dummy_makes() {
        let w = workload("rest-dd").expect("exists");
        for i in 0..5 {
            let c = bench_claim(w, 7, i).expect("claim");
            assert_eq!(c.tricks, 7);
        }
    }

    #[test]
    fn presets_name_real_workloads() {
        for p in &PRESETS {
            for name in p.workloads {
                assert!(workload(name).is_some(), "{} names {name}", p.name);
            }
        }
    }

    /// The text between `start` and the next `end` after it.
    fn between<'a>(text: &'a str, start: &str, end: &str) -> &'a str {
        let from = text.find(start).expect("start marker") + start.len();
        let len = text[from..].find(end).expect("end marker");
        &text[from..from + len]
    }

    /// The web page checks the browser build against the native results in
    /// this fixture, so it has to describe this build. Rechecks the cheap
    /// claims among the first few of every row; a change to the claim search
    /// that alters any of them means the fixture needs regenerating.
    #[test]
    fn matches_the_web_fixture() {
        let fixture = include_str!("../web/src/lib/fixtures/claim-bench-v1.json");
        let budget: u64 = between(fixture, "\"budget\":", ",")
            .parse()
            .expect("budget");
        let mut checked = 0;
        for w in &WORKLOADS {
            for size in 7..=13 {
                let key = format!("\"{}/{size}\":", w.name);
                let row = between(fixture, &key, "}");
                let verdicts: Vec<char> = between(row, "\"verdicts\":\"", "\"").chars().collect();
                let nodes: Vec<u64> = between(row, "\"nodes\":[", "]")
                    .split(',')
                    .map(|n| n.parse().expect("node count"))
                    .collect();
                for i in 0..5 {
                    if nodes[i] > 20_000 {
                        continue;
                    }
                    let c = bench_claim(w, size, i as u64).expect("claim");
                    let r = c.position.check(c.tricks, budget).expect("check");
                    assert_eq!(
                        (verdict_letter(r.verdict), r.nodes),
                        (verdicts[i], nodes[i]),
                        "{} size {size} claim {i} differs from the web fixture; \
                         run scripts/claim-bench-fixture.sh",
                        w.name
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 50, "only {checked} cheap claims to compare");
    }

    #[test]
    fn bad_sizes_are_rejected() {
        let w = &WORKLOADS[0];
        assert_eq!(bench_claim(w, 0, 0).err(), Some(BenchError::BadSize(0)));
        assert_eq!(bench_claim(w, 14, 0).err(), Some(BenchError::BadSize(14)));
    }
}
