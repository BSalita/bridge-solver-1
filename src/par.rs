//! Par-score calculation from a double-dummy trick table.
//!
//! Given the max DD tricks for each seat in each strain plus vulnerability,
//! computes the contract-neutral par (the score optimal competitive bidding
//! yields, both sides free to bid). No extra solving — it is a bounded search
//! over the 35 possible contracts using duplicate scoring.
//!
//! Validated to 100% agreement with Bridge Composer's `OptimumScore` across 104
//! tournament boards (see `tests`). The `OptimumScore` convention is matched:
//! the score is labeled by the par contract's declaring side and signed to that
//! side (a sacrifice reads negative, e.g. "NS -100").
//!
//! `ParContract` is not the same tag written differently, and agreeing on the
//! score does not imply agreeing on the contract. Bridge Composer lists *every*
//! contract tied at par, separated by `"; "`, and names a single **seat** when
//! only one partner can take the tricks double-dummy — `N 3N=` where North
//! makes nine at notrump and South only eight. Both rules were read off
//! `fixtures/bridge-composer`, where our scores matched on all eight boards and
//! four of the contracts did not.

use crate::{direction_to_seat, get_node_count, CutoffCache, Hands, PatternCache, Solver};
use crate::{CLUB, DIAMOND, HEART, NOTRUMP, SPADE};
use bridge_types::{Contract, DdTable, Deal, Direction, Doubled, Strain, STRAINS};
use std::cell::Cell;

const DIRECTIONS: [Direction; 4] = [
    Direction::South,
    Direction::North,
    Direction::West,
    Direction::East,
];

fn dir_index(d: Direction) -> usize {
    match d {
        Direction::North => 0,
        Direction::East => 1,
        Direction::South => 2,
        Direction::West => 3,
    }
}

fn strain_index(s: Strain) -> usize {
    match s {
        Strain::Clubs => 0,
        Strain::Diamonds => 1,
        Strain::Hearts => 2,
        Strain::Spades => 3,
        Strain::NoTrump => 4,
    }
}

/// Best DD tricks a side can take in a strain, over its two seats.
///
/// [`DdTable::best_for_side`] takes one seat and consults its partner, which is
/// the same thing said from the other end.
fn side_max(dd: &DdTable, side: Side, strain: Strain) -> u8 {
    dd.best_for_side(side.seats().0, strain)
}

/// Solve the full 20-entry DD table for a complete deal.
pub fn solve_dd_table(deal: &Deal) -> DdTable {
    with_shared(|s| s.solve(deal))
}

/// Solve the full table, and report how many nodes it took.
///
/// The same function as [`solve_dd_table`], with the per-cell counts summed.
/// It lives here rather than in the benchmark harness so that the count cannot
/// drift from the search it describes: comparing node counts against the C++
/// reference is only meaningful if the count covers exactly the work the
/// timing covers, cache reuse and MTD(f) seeding included.
pub fn solve_dd_table_with_nodes(deal: &Deal) -> (DdTable, u64) {
    with_shared(|s| s.solve_with_nodes(deal))
}

/// Solve the full table, reporting each cell's nodes as it is finished.
///
/// For localising a divergence against the C++ reference: a whole-table count
/// says the trees differ, a per-cell one says *which* search to trace.
pub fn solve_dd_table_cells(deal: &Deal) -> (DdTable, Vec<(Strain, Direction, u64)>) {
    with_shared(|s| s.solve_cells(deal))
}

/// Solve one strain of a deal on this thread's shared [`TableSolver`].
///
/// The parallel counterpart to [`solve_dd_table`]: five of these fill the same
/// table as one of those, and a caller with more deals than it has patience
/// can spread the (deal, strain) pairs over threads rather than the deals. On
/// a small batch that is the difference between keeping every thread busy and
/// waiting on the last deal -- 27 boards are 27 work items but 135 pairs, and
/// deal cost spans roughly tenfold. See `bench/comparison/RESULTS.md`, case 2.
///
/// Returns tricks indexed N, E, S, W; the column belongs at
/// `DdTricks::tricks[dir][i]` for the `i` at which [`STRAINS`] holds `strain`.
pub fn solve_dd_strain(deal: &Deal, strain: Strain) -> [u8; 4] {
    with_shared(|s| s.solve_strain(deal, strain))
}

/// The size both caches start at, in bits. The reference starts its
/// `cutoff_cache` at 16 and its `common_bounds_cache` at 15; both grow on
/// demand, and the starting size is not something the search can see.
const CACHE_BITS: usize = 16;

/// Reusable storage for solving tables.
///
/// The two caches a table solve needs are the solver's whole memory footprint,
/// and building them is not free: `CutoffCache::new(16)` alone is a megabyte,
/// and both then double their way up to whatever the deal wants. Solving each
/// strain with a fresh pair means five of those a deal -- a thousand over the
/// 200-deal lock-step corpus -- and throws away the grown capacity every time.
///
/// The C++ reference does not. Its `common_bounds_cache` and `cutoff_cache`
/// are process globals, and `Solve` only calls `Reset()` on them per trump, so
/// it allocates once for the life of the process. Timing it the way it is
/// actually run rather than one process per deal is worth 4.5% to it over 500
/// random deals; part of that is exactly this. See
/// `bench/results/release-profile.md`.
///
/// So: one pair, reset between strains. Holding a `TableSolver` across deals
/// is the point -- the free functions in this module do it through a
/// thread-local -- but the storage it keeps is the peak of the hardest deal it
/// has seen, which for a freakish distribution is large. A long-lived thread
/// that wants that back calls [`crate::drain_pool`].
pub struct TableSolver {
    cutoff: CutoffCache,
    pattern: PatternCache,
}

impl Default for TableSolver {
    fn default() -> Self {
        Self::new()
    }
}

impl TableSolver {
    /// Storage for one thread's solves, at the size the reference starts at.
    pub fn new() -> Self {
        TableSolver {
            cutoff: CutoffCache::new(CACHE_BITS),
            pattern: PatternCache::new(PatternCache::DEFAULT_BITS),
        }
    }

    /// Solve the full 20-entry DD table for a complete deal.
    pub fn solve(&mut self, deal: &Deal) -> DdTable {
        self.solve_inner(deal, |_, _, _| {})
    }

    /// Solve the full table, and report how many nodes it took.
    pub fn solve_with_nodes(&mut self, deal: &Deal) -> (DdTable, u64) {
        let mut nodes = 0;
        let tricks = self.solve_inner(deal, |_, _, n| nodes += n);
        (tricks, nodes)
    }

    /// Solve the full table, reporting each cell's nodes as it is finished.
    pub fn solve_cells(&mut self, deal: &Deal) -> (DdTable, Vec<(Strain, Direction, u64)>) {
        let mut cells = Vec::with_capacity(20);
        let tricks = self.solve_inner(deal, |s, d, n| cells.push((s, d, n)));
        (tricks, cells)
    }

    /// Solve one strain of a deal: the four declarers' tricks in that strain,
    /// indexed N, E, S, W -- the first axis of [`DdTricks::tricks`].
    ///
    /// This is the smallest self-contained piece of a table, and so the unit a
    /// parallel caller should hand to a thread. A strain owns its cache state
    /// -- both caches are reset here, as the reference resets per trump -- and
    /// the MTD(f) seed chain runs from the first of its four declarers to the
    /// last, so those four cells have to stay together. Nothing crosses a
    /// strain boundary, so the five strains of a deal may be solved
    /// concurrently, each on its own `TableSolver`, and
    /// [`Self::solve`] is exactly the five of them in [`STRAINS`] order.
    ///
    /// Re-deriving [`Hands`] per strain rather than once per deal is the whole
    /// cost of the split: a bitmask fill against a search measured in
    /// milliseconds.
    pub fn solve_strain(&mut self, deal: &Deal, strain: Strain) -> [u8; 4] {
        self.solve_strain_hands(Hands::from_deal(deal), strain)
    }

    /// [`Self::solve_strain`] for a caller that already holds [`Hands`].
    ///
    /// The same search and the same column; what differs is what it will read.
    /// [`Hands::from_pbn`] accepts deal strings that
    /// [`Deal::from_pbn`](bridge_types::Deal::from_pbn) rejects — one with no
    /// leading `N:` seat, one writing a void as `-` inside a suit rather than
    /// as the whole suit — so a caller that parsed with the first and then
    /// went through a `Deal` to reach this solver would silently stop
    /// accepting files it reads today. This is the way in that does not narrow
    /// them.
    ///
    /// It also skips re-deriving [`Hands`], which the `Deal` form pays on
    /// every call — five times a table when a caller splits by strain.
    pub fn solve_strain_hands(&mut self, hands: Hands, strain: Strain) -> [u8; 4] {
        let total = hands.num_tricks() as u8;
        self.solve_strain_inner(hands, total, strain, |_, _, _| {})
    }

    /// The one table-solving loop. `on_cell` sees each cell's node count;
    /// passing a closure that ignores it compiles the reporting away entirely,
    /// so [`Self::solve`] pays nothing for the instrumentation.
    fn solve_inner(
        &mut self,
        deal: &Deal,
        mut on_cell: impl FnMut(Strain, Direction, u64),
    ) -> DdTable {
        let hands = Hands::from_deal(deal);
        let total = hands.num_tricks() as u8;
        let mut table = DdTable::new();
        for strain in STRAINS {
            let column = self.solve_strain_inner(hands, total, strain, &mut on_cell);
            // The column is in `dir_index` order; name each seat rather than
            // relying on the two layouts happening to agree.
            for (row, cell) in column.iter().enumerate() {
                let declarer = match row {
                    0 => Direction::North,
                    1 => Direction::East,
                    2 => Direction::South,
                    _ => Direction::West,
                };
                table.set(declarer, strain, *cell);
            }
        }
        table
    }

    /// One strain's four cells, solved in `DIRECTIONS` order into a column
    /// indexed by `dir_index`. The single implementation behind both
    /// [`Self::solve`] and [`Self::solve_strain`], so that splitting a table
    /// across threads cannot quietly become a different search from solving it
    /// in one.
    fn solve_strain_inner(
        &mut self,
        hands: Hands,
        total: u8,
        strain: Strain,
        mut on_cell: impl FnMut(Strain, Direction, u64),
    ) -> [u8; 4] {
        let trump = strain_trump(strain);
        // Per strain, as the reference resets per trump. The entries go
        // and the capacity stays, which is the whole point; see
        // `CutoffCache::reset` for why the size cannot reach the answers.
        self.cutoff.reset();
        self.pattern.reset();
        // The four declarers in a strain give similar counts, so each cell
        // seeds the next one's MTD(f) search. The seed cannot change an
        // answer, only how many iterations reaching it takes.
        let mut seed: Option<usize> = None;
        let mut column = [0u8; 4];
        for dir in DIRECTIONS {
            let seat = direction_to_seat(dir);
            let leader = (seat + 1) % 4;
            let solver = Solver::new(hands, trump, leader);
            let ns = match seed {
                Some(g) => solver.solve_with_caches_seeded(&mut self.cutoff, &mut self.pattern, g),
                None => solver.solve_with_caches(&mut self.cutoff, &mut self.pattern),
            };
            on_cell(strain, dir, get_node_count());
            seed = Some(Solver::seed_from(ns));
            // Upstream 14ab4c7, 189f86b and 3052870: a deal with four or more
            // voids clears the cut-off cache after every cell, not only
            // between strains. (The pattern cache was cleared too, for a time,
            // to cap its size on freak deals; 3052870 stopped that.)
            if hands.num_voids() >= 4 {
                self.cutoff.reset();
            }
            let declarer_tricks = if matches!(dir, Direction::North | Direction::South) {
                ns
            } else {
                total - ns
            };
            column[dir_index(dir)] = declarer_tricks;
        }
        column
    }
}

thread_local! {
    /// The [`TableSolver`] the free functions in this module share, so that a
    /// caller solving deal after deal allocates once without having to thread
    /// a context through. Per-thread because a `TableSolver` is exclusive
    /// storage, and `None` while a solve is using it.
    static SHARED: Cell<Option<Box<TableSolver>>> = const { Cell::new(None) };
}

/// Run `f` against this thread's shared [`TableSolver`].
///
/// Taken out of the cell for the duration rather than borrowed, so that a
/// re-entrant call gets storage of its own instead of a panic, and an unwind
/// out of `f` leaves the cell empty rather than poisoned -- the next call
/// simply builds a fresh one.
fn with_shared<R>(f: impl FnOnce(&mut TableSolver) -> R) -> R {
    let mut solver = SHARED
        .with(|cell| cell.take())
        .unwrap_or_else(|| Box::new(TableSolver::new()));
    let out = f(&mut solver);
    SHARED.with(|cell| cell.set(Some(solver)));
    out
}

/// Drop this thread's shared [`TableSolver`], returning its pattern-tree blocks
/// to the pool.
///
/// Called by [`crate::drain_pool`], which cannot do what it promises without
/// it: the blocks a retained cache holds are live, so draining the pool around
/// them would free everything except the peak that actually matters.
pub(crate) fn release_shared() {
    SHARED.with(|cell| cell.set(None));
}

fn strain_trump(strain: Strain) -> usize {
    match strain {
        Strain::Clubs => CLUB,
        Strain::Diamonds => DIAMOND,
        Strain::Hearts => HEART,
        Strain::Spades => SPADE,
        Strain::NoTrump => NOTRUMP,
    }
}

/// A declaring partnership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    NS,
    EW,
}

impl Side {
    fn seats(self) -> (Direction, Direction) {
        match self {
            Side::NS => (Direction::North, Direction::South),
            Side::EW => (Direction::East, Direction::West),
        }
    }
    fn label(self) -> &'static str {
        match self {
            Side::NS => "NS",
            Side::EW => "EW",
        }
    }
}

/// Who a par contract names: one seat, or a whole side.
///
/// Bridge Composer names a single seat when only one partner can take the
/// tricks the par score is computed from, and the side when either can — board
/// 5 of `fixtures/bridge-composer` is `N 3N=` because North makes nine at
/// notrump and South only eight. We wrote the side unconditionally until that
/// fixture said otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParDeclarer {
    /// Only this seat takes the tricks; its partner takes fewer.
    Seat(Direction),
    /// Both partners take them, so either may declare.
    Both(Side),
}

impl ParDeclarer {
    /// The seat or side as `ParContract` writes it: `"N"` or `"NS"`.
    pub fn label(self) -> String {
        match self {
            ParDeclarer::Seat(seat) => seat.to_char().to_string(),
            ParDeclarer::Both(side) => side.label().to_string(),
        }
    }

    /// The declaring side, whichever form this takes.
    pub fn side(self) -> Side {
        match self {
            ParDeclarer::Seat(Direction::North | Direction::South) => Side::NS,
            ParDeclarer::Seat(_) => Side::EW,
            ParDeclarer::Both(side) => side,
        }
    }
}

/// The par contract.
#[derive(Debug, Clone, Copy)]
pub struct ParContract {
    pub side: Side,
    pub level: u8,
    pub strain: Strain,
    /// DD tricks the side takes (result relative to the contract may be negative
    /// for a sacrifice).
    pub tricks: u8,
    /// The seat or side the contract is named for; see [`ParDeclarer`].
    pub declarer: ParDeclarer,
}

impl ParContract {
    /// Result relative to the contract (+overtricks / -undertricks).
    pub fn relative(&self) -> i32 {
        self.tricks as i32 - (self.level as i32 + 6)
    }
    /// A sacrifice is a par contract that does not make (doubled by opponents).
    pub fn is_sacrifice(&self) -> bool {
        self.relative() < 0
    }
    /// e.g. "NS 6S=", "EW 4SX-1", "N 3N=".
    pub fn describe(&self) -> String {
        let rel = self.relative();
        let x = if self.is_sacrifice() { "X" } else { "" };
        let result = match rel.cmp(&0) {
            std::cmp::Ordering::Equal => "=".to_string(),
            std::cmp::Ordering::Greater => format!("+{rel}"),
            std::cmp::Ordering::Less => rel.to_string(),
        };
        format!(
            "{} {}{}{}{}",
            self.declarer.label(),
            self.level,
            self.strain.to_char(),
            x,
            result
        )
    }
}

/// Result of a par calculation.
#[derive(Debug, Clone)]
pub struct ParResult {
    /// Par score from North-South's perspective (positive = NS benefits).
    pub score_ns: i32,
    /// Every contract tied at the par score, cheapest first — one per strain,
    /// at the lowest level in that strain that reaches par. Empty for a
    /// passed-out deal (par zero).
    ///
    /// A single contract is the common case; two are not rare, and Bridge
    /// Composer writes all of them. This was an `Option<ParContract>`, which
    /// could not represent a tie at all: board 1 of `fixtures/bridge-composer`
    /// is `EW 2SX-1; EW 3CX-1`, two sacrifices that cost the same 100.
    pub contracts: Vec<ParContract>,
}

impl ParResult {
    /// Bridge-Composer-style `OptimumScore`: labeled by the par contract's
    /// declaring side, signed to that side (e.g. "NS 980", "EW -500", "0").
    ///
    /// Every tied contract belongs to the same side — they all score the same
    /// number, and it is signed to whoever declares — so the first one names
    /// the side for all of them.
    pub fn optimum_score(&self) -> String {
        match self.contracts.first() {
            None => "0".to_string(),
            Some(c) => {
                let to_side = if c.side == Side::NS {
                    self.score_ns
                } else {
                    -self.score_ns
                };
                format!("{} {}", c.side.label(), to_side)
            }
        }
    }

    /// Bridge-Composer-style `ParContract`: every tied contract, cheapest
    /// first, separated by `"; "` (e.g. `"NS 4H=; NS 4S="`). `None` for a
    /// passed-out deal, which carries no contract to name.
    pub fn par_contract(&self) -> Option<String> {
        if self.contracts.is_empty() {
            return None;
        }
        Some(
            self.contracts
                .iter()
                .map(ParContract::describe)
                .collect::<Vec<_>>()
                .join("; "),
        )
    }
}

/// Bidding rank of a contract; higher outranks lower.
fn rank(level: u8, strain: Strain) -> i32 {
    (level as i32 - 1) * 5 + strain_index(strain) as i32
}

/// Score to `side` if it declares `level`-`strain`: making contracts undoubled
/// (positive); non-making contracts are sacrifices, doubled by opponents
/// (negative).
fn score_to_side(level: u8, strain: Strain, tricks: u8, vul: bool) -> i32 {
    let rel = tricks as i32 - (level as i32 + 6);
    let doubled = if rel >= 0 {
        Doubled::None
    } else {
        Doubled::Doubled
    };
    Contract::new(level, strain, doubled, 'N').score(rel, vul)
}

/// The contract `side` reaches by bidding `level`-`strain`, named for the seat
/// or seats that can actually take the tricks.
///
/// The trick count is the side's best, so when the partners differ only one of
/// them can declare it: Bridge Composer writes `N 3N=` rather than `NS 3N=`
/// where North makes nine at notrump and South eight.
fn contract_at(dd: &DdTable, side: Side, level: u8, strain: Strain) -> ParContract {
    let (first, second) = side.seats();
    let (a, b) = (dd.tricks(first, strain), dd.tricks(second, strain));
    let declarer = match a.cmp(&b) {
        std::cmp::Ordering::Equal => ParDeclarer::Both(side),
        std::cmp::Ordering::Greater => ParDeclarer::Seat(first),
        std::cmp::Ordering::Less => ParDeclarer::Seat(second),
    };
    ParContract {
        side,
        level,
        strain,
        tricks: a.max(b),
        declarer,
    }
}

/// Every contract tied at the par score, cheapest first.
///
/// One per strain — the lowest level in that strain reaching par, since a
/// higher one in the same strain is the same contract bid dearer — from
/// `min_rank`, the rank the outbidding in [`par`] came to rest at. Contracts
/// below that rank are not par contracts even when they score the same: board 4
/// of `fixtures/bridge-composer` is `EW 1D+3`, and `1C+3` scores the same 130
/// but is never reached, which is what Bridge Composer writes too.
fn tied_contracts(
    dd: &DdTable,
    side: Side,
    min_rank: i32,
    score_ns: i32,
    vul: bool,
) -> Vec<ParContract> {
    let mut tied: Vec<ParContract> = Vec::new();
    for strain in STRAINS {
        let tricks = side_max(dd, side, strain);
        for level in 1..=7u8 {
            if rank(level, strain) < min_rank {
                continue;
            }
            let s = score_to_side(level, strain, tricks, vul);
            let s_ns = if side == Side::NS { s } else { -s };
            if s_ns == score_ns {
                tied.push(contract_at(dd, side, level, strain));
                break;
            }
        }
    }
    tied.sort_by_key(|c| rank(c.level, c.strain));
    tied
}

/// Compute par from a DD table and each side's vulnerability.
pub fn par(dd: &DdTable, vul_ns: bool, vul_ew: bool) -> ParResult {
    let vul_of = |side: Side| match side {
        Side::NS => vul_ns,
        Side::EW => vul_ew,
    };

    let mut cur_rank = 0i32;
    let mut cur_ns = 0i32;
    let mut contract: Option<ParContract> = None;

    // Each accepted bid strictly outranks the last, so this terminates; the
    // bound is a backstop.
    for _ in 0..64 {
        // Cheapest bid (lowest rank) above the current contract that improves
        // the bidding side's position — minimum competitive outbid.
        let mut best: Option<(i32, i32, ParContract)> = None;
        for side in [Side::NS, Side::EW] {
            for level in 1..=7u8 {
                for strain in STRAINS {
                    let r = rank(level, strain);
                    if r <= cur_rank {
                        continue;
                    }
                    let tricks = side_max(dd, side, strain);
                    let s = score_to_side(level, strain, tricks, vul_of(side));
                    let s_ns = if side == Side::NS { s } else { -s };
                    let improves = if side == Side::NS {
                        s_ns > cur_ns
                    } else {
                        s_ns < cur_ns
                    };
                    if improves && best.is_none_or(|(br, _, _)| r < br) {
                        best = Some((r, s_ns, contract_at(dd, side, level, strain)));
                    }
                }
            }
        }
        match best {
            None => break,
            Some((r, s_ns, c)) => {
                cur_rank = r;
                cur_ns = s_ns;
                contract = Some(c);
            }
        }
    }

    // The auction has come to rest. Par is `cur_ns` at `cur_rank`, and the
    // contract found is the cheapest that reaches it — but not necessarily the
    // only one, so collect the rest of the tie.
    let contracts = match contract {
        None => Vec::new(),
        Some(c) => tied_contracts(dd, c.side, cur_rank, cur_ns, vul_of(c.side)),
    };

    ParResult {
        score_ns: cur_ns,
        contracts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bridge_types::Deal;

    /// (vul_ns, vul_ew) for a PBN Vulnerable value.
    fn vul(v: &str) -> (bool, bool) {
        match v.to_uppercase().as_str() {
            "NS" | "N-S" => (true, false),
            "EW" | "E-W" => (false, true),
            "ALL" | "BOTH" => (true, true),
            _ => (false, false),
        }
    }

    /// Boards Bridge Composer has scored, with what it wrote:
    /// `(deal, vulnerable, its OptimumScore, its ParContract)`. Oracle:
    /// LBC-2026 hand records.
    fn oracle() -> [(&'static str, &'static str, &'static str, &'static str); 14] {
        [
            (
                "N:AQT94.T53.KQ8.T9 63.J98.J53.K8654 K72.6.AT9762.AJ7 J85.AKQ742.4.Q32",
                "None",
                "NS 980",
                "NS 6S=",
            ),
            (
                "E:7.AKJ6543.3.AK93 642.8.KQT9.T8752 AKQJ5.Q97.52.QJ6 T983.T2.AJ8764.4",
                "NS",
                "EW 980",
                "EW 6H=; EW 6S=",
            ),
            (
                "S:AJ83.7.T9653.A62 Q4.QJT984.4.T853 T762.K3.KQ2.KQJ4 K95.A652.AJ87.97",
                "EW",
                "NS -100",
                "NS 4SX-1",
            ),
            (
                "W:87.9.T9642.AK942 AQJ53.QJ873.QJ5. 94.A652.K83.QJ75 KT62.KT4.A7.T863",
                "All",
                "NS 650",
                "NS 4S+1",
            ),
            (
                "N:932.962.A.AJ8643 AJ87.QJ43.Q9.KT2 KQ5.8.JT6543.Q97 T64.AKT75.K872.5",
                "NS",
                "EW 140",
                "EW 3H=",
            ),
            (
                "E:QT7.Q8.QJT84.K83 986543.5.K5.AT94 K.AKJ9743.7.Q765 AJ2.T62.A9632.J2",
                "EW",
                "NS 140",
                "NS 3S=",
            ),
            (
                "S:AT.A5.AJ842.J984 Q7654.QJ9874.95. 2.K32.QT763.KT75 KJ983.T6.K.AQ632",
                "All",
                "EW -500",
                "EW 5SX-2",
            ),
            (
                "W:QJ76.KJ9.AT92.Q8 A843.A2.QJ75.T52 K5.T873.K863.AKJ T92.Q654.4.97643",
                "None",
                "EW 430",
                "EW 3N+1",
            ),
            (
                "N:K643.J65.T5.Q654 AJ72.83.AJ6.AK93 QT98.QT9.Q987.T2 5.AK742.K432.J87",
                "EW",
                "EW 660",
                "E 3N+2",
            ),
            (
                "E:Q85.AJ62.AJ984.K 93.9.Q7653.J8752 AKT742.8.K2.QT43 J6.KQT7543.T.A96",
                "All",
                "EW 1430",
                "EW 6S=",
            ),
            (
                "S:J8.KT93.QJ95.T96 K63.AJ86.A43.A74 AT7542.72.762.K3 Q9.Q54.KT8.QJ852",
                "None",
                "EW 400",
                "EW 3N=",
            ),
            (
                "W:QJ832.Q76.QT63.9 T74.82.AK982.QJ2 A96.KJT43.4.AKT5 K5.A95.J75.87643",
                "NS",
                "EW 420",
                "EW 4H=",
            ),
            (
                "W:AQ86.86.AQT943.6 T54.QJ2.J82.KQ42 3.AKT9.K5.JT8753 KJ972.7543.76.A9",
                "EW",
                "EW 1370",
                "EW 6D=",
            ),
            (
                "E:KQJT.KQ6.94.KJ54 A64.7.AKJ6532.T6 752.JT84.Q7.A932 983.A9532.T8.Q87",
                "None",
                "EW -300",
                "EW 4CX-2; EW 4HX-2",
            ),
        ]
    }

    /// Par computed from our own DD solve must reproduce Bridge Composer's
    /// `OptimumScore` on real tournament boards.
    #[test]
    fn par_matches_bridge_composer_optimum_score() {
        for (deal_str, v, expected, their_contract) in oracle() {
            let deal = Deal::from_pbn(deal_str).expect("deal parses");
            let (vn, ve) = vul(v);
            let result = par(&solve_dd_table(&deal), vn, ve);
            assert_eq!(
                result.optimum_score(),
                expected,
                "deal {deal_str} (their par {their_contract}); computed contract {:?}",
                result.par_contract(),
            );
        }
    }

    /// The contracts themselves, against the same oracle. Every one of these
    /// strings was read off a Bridge Composer file; three of them are ties it
    /// lists in full, and one names a single seat because only that partner can
    /// take the tricks. We wrote one contract, always labelled by side, until
    /// `fixtures/bridge-composer` showed both to be wrong.
    #[test]
    fn par_contract_matches_bridge_composer() {
        for (deal_str, v, _expected, their_contract) in oracle() {
            let deal = Deal::from_pbn(deal_str).expect("deal parses");
            let (vn, ve) = vul(v);
            let result = par(&solve_dd_table(&deal), vn, ve);
            assert_eq!(
                result.par_contract().as_deref(),
                Some(their_contract),
                "deal {deal_str}"
            );
        }
    }

    #[test]
    fn passed_out_is_par_zero() {
        // Nobody can take 7 tricks anywhere.
        let dd = DdTable::from_fn(|_, _| 6);
        let r = par(&dd, false, false);
        assert_eq!(r.optimum_score(), "0");
        assert!(r.contracts.is_empty());
        assert_eq!(r.par_contract(), None);
    }
}
