//! Fixed double-dummy workloads, for benchmarking the solver.
//!
//! The counterpart of [`crate::claim_bench`] for the analysis engine. The
//! deals are the C++ reference's own 200-deal corpus, the one this port is
//! kept in lock-step with (`fixtures/divergence/lockstep-200.pbn`; the copy
//! here is what gets compiled in, and a test keeps the two identical). Each
//! benchmark item is one deal's full 20-cell double-dummy table, the work the
//! analysis page does first for every hand.
//!
//! A table's tricks and node count depend only on the deal: the solver's
//! caches are reset between deals, so any deal can be solved alone, in any
//! order, on any thread, and comes out the same. That lets the native tool
//! spread deals across threads and the web page run them one at a time, and
//! still compare node for node.

use bridge_types::{Deal, Direction, Strain};

use crate::par::solve_dd_table_with_nodes;

/// The corpus, one PBN deal per line.
const DEALS: &str = include_str!("dd_bench_deals.txt");

/// A named slice of the corpus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    /// Short name, as given on a command line.
    pub name: &'static str,
    /// Deals solved, the first this many of the corpus.
    pub deals: usize,
}

/// The presets the web page and `bridge-solver --bench` share.
///
/// `quick` is a few seconds in a laptop's browser; `full` is the whole corpus,
/// under half a minute single-threaded natively and a minute or two in a
/// browser.
pub const PRESETS: [Preset; 2] = [
    Preset {
        name: "quick",
        deals: 20,
    },
    Preset {
        name: "full",
        deals: 200,
    },
];

/// The preset called `name`.
pub fn preset(name: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.name == name)
}

/// How many deals the corpus holds.
pub fn deal_count() -> usize {
    DEALS.lines().filter(|l| !l.trim().is_empty()).count()
}

/// The `index`th deal of the corpus as a PBN deal string, if there is one.
pub fn deal_pbn(index: usize) -> Option<&'static str> {
    DEALS
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .nth(index)
}

/// One solved deal: its table and the work it took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchTable {
    /// `tricks[seat][strain]`, seats `N, E, S, W`, strains `C, D, H, S, NT`.
    pub tricks: [[u8; 5]; 4],
    /// Search nodes visited, summed over the twenty cells.
    pub nodes: u64,
}

impl BenchTable {
    /// The twenty trick counts as one string of hex digits, row by row: the
    /// compact form the web fixture and the native tool's JSON both use.
    pub fn tricks_hex(&self) -> String {
        self.tricks
            .iter()
            .flatten()
            .map(|&t| char::from_digit(u32::from(t), 16).unwrap_or('?'))
            .collect()
    }
}

/// Solve the `index`th deal's full table, or `None` past the end of the
/// corpus.
pub fn bench_table(index: usize) -> Option<BenchTable> {
    let deal = Deal::from_pbn(deal_pbn(index)?)?;
    let (table, nodes) = solve_dd_table_with_nodes(&deal);
    let seats = [
        Direction::North,
        Direction::East,
        Direction::South,
        Direction::West,
    ];
    let strains = [
        Strain::Clubs,
        Strain::Diamonds,
        Strain::Hearts,
        Strain::Spades,
        Strain::NoTrump,
    ];
    Some(BenchTable {
        tricks: seats.map(|seat| strains.map(|strain| table.tricks(seat, strain))),
        nodes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_corpus_is_the_lockstep_one() {
        assert_eq!(
            DEALS,
            include_str!("../fixtures/divergence/lockstep-200.pbn"),
            "src/dd_bench_deals.txt must stay a copy of the lock-step corpus"
        );
        assert_eq!(deal_count(), 200);
        for p in &PRESETS {
            assert!(p.deals <= deal_count(), "{} runs past the corpus", p.name);
        }
    }

    /// The web page checks the browser build against the native results in
    /// this fixture. Rechecks the cheap deals among the quick preset's; a
    /// change to the search that alters any of them means the fixture needs
    /// regenerating with scripts/dd-bench-fixture.sh.
    #[test]
    fn matches_the_web_fixture() {
        let fixture = include_str!("../web/src/lib/fixtures/dd-bench-v1.json");
        let list = |key: &str| -> Vec<&str> {
            let from = fixture.find(key).expect("key") + key.len();
            let len = fixture[from..].find(']').expect("end of list");
            fixture[from..from + len]
                .split(',')
                .map(|s| s.trim_matches('"'))
                .collect()
        };
        let nodes = list("\"nodes\":[");
        let tricks = list("\"tricks\":[");
        assert_eq!(nodes.len(), deal_count());
        let mut checked = 0;
        for i in 0..PRESETS[0].deals {
            let want: u64 = nodes[i].parse().expect("node count");
            if want > 800_000 {
                continue;
            }
            let got = bench_table(i).expect("deal");
            assert_eq!(
                (got.tricks_hex().as_str(), got.nodes),
                (tricks[i], want),
                "deal {i} differs from the web fixture; run scripts/dd-bench-fixture.sh"
            );
            checked += 1;
        }
        assert!(checked >= 5, "only {checked} cheap deals to compare");
    }

    #[test]
    fn past_the_end_is_none() {
        assert!(bench_table(deal_count()).is_none());
    }
}
