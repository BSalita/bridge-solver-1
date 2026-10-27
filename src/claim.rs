//! Claim checking: can the claiming side guarantee N more tricks without
//! seeing the cards the claimer cannot see?
//!
//! The design, its measurements, what was tried and dropped, and credits
//! are in `docs/claim-checker.md`. Read that before changing the search.
//!
//! A double-dummy result answers a different question. It knows where every
//! card is, so it happily "proves" a claim that only works because a finesse
//! happens to be onside. A claim has to hold however the unseen cards lie.
//!
//! # Model
//!
//! The claimer sees two hands: their own and dummy's. The other two hands'
//! remaining cards form one *pool* whose split between them is unknown. When
//! a pooled seat is to play, it may play *any* pool card, provided some split
//! of what is left is still consistent with:
//!
//! * each pooled seat's remaining hand size, and
//! * every suit that seat has shown out of, in this search or in the play so
//!   far.
//!
//! Playing off-suit marks the seat void in the led suit. For a claim by
//! declarer this is the model the BBO claim checker is described as using.
//!
//! Choosing the layout lazily like this gives the same answer as fixing it in
//! advance. The claimer's decisions depend only on cards already seen, so a
//! strategy that beats every lazily chosen defence also beats every fixed
//! layout, and the reverse holds because each lazy line of play ends in some
//! fixed layout. A claim is therefore [`ClaimVerdict::Valid`] exactly when
//! the claimer has a line that takes N tricks against every layout consistent
//! with what has been seen.
//!
//! ## Claims by a defender
//!
//! A defender sees their own hand and dummy, so the pool is declarer's hand
//! and the defender's *partner's*. The claimer cannot see partner's cards and
//! cannot steer partner's play, so the search gives partner's choices to the
//! adversary, along with declarer's and dummy's. Partner's tricks still count
//! for the claiming side. A defender's claim is valid only if it works
//! whatever partner holds and however partner plays: one that needs partner
//! to hold a particular card, or to find a particular play, is not a claim.
//!
//! The search is an AND/OR search with a transposition table at trick
//! boundaries and a node budget. When the budget runs out the answer is
//! [`ClaimVerdict::Undecided`], never a false `Invalid`.
//!
//! Before searching, a claim is tested against a few dozen layouts by double
//! dummy: if the claiming side falls short on any one of them, no line can
//! guarantee the claim. Most invalid claims end there. Valid claims are sped
//! up by a sure-trick bound that cashes winners from both of declarer's
//! hands, crossing between them and drawing trumps first.
//!
//! This module calls the double-dummy search in `search.rs` but does not
//! change it.

use std::collections::HashMap;
use std::fmt;
use std::hash::{BuildHasherDefault, Hasher};

use super::bridge_solver::PartialTrick;
use super::cards::*;
use super::hands::Hands;
use super::pattern::PatternCache;
use super::play::{check_play, get_playable_cards, trick_winner, wins_over, IllegalPlay};
use super::search::{CutoffCache, Search as DdSearch};
use super::types::*;

/// A node budget that settles typical end-of-hand claims in well under a
/// second in a browser.
pub const DEFAULT_CLAIM_BUDGET: u64 = 2_000_000;

/// Why a position could not be set up for claim checking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimError {
    /// The four hands of the starting position are not all the same size.
    UnequalHands,
    /// A card in the play record is not in the hand of the seat to play.
    NotHeld {
        /// Index of the offending card in the play record.
        index: usize,
        /// The card.
        card: usize,
        /// The seat that was due to play.
        seat: Seat,
    },
    /// A card in the play record fails to follow suit when it could have.
    Revoke {
        /// Index of the offending card in the play record.
        index: usize,
        /// The card.
        card: usize,
        /// The seat that played it.
        seat: Seat,
    },
    /// The claim is for more tricks than remain.
    TooManyTricks {
        /// Tricks claimed.
        claimed: usize,
        /// Tricks remaining, including the one in progress.
        remaining: usize,
    },
}

impl fmt::Display for ClaimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClaimError::UnequalHands => write!(f, "the four hands are not the same size"),
            ClaimError::NotHeld { index, card, seat } => write!(
                f,
                "play {}: {} is not in {}'s hand",
                index + 1,
                name_of(*card),
                seat_name(*seat)
            ),
            ClaimError::Revoke { index, card, seat } => write!(
                f,
                "play {}: {} by {} fails to follow suit",
                index + 1,
                name_of(*card),
                seat_name(*seat)
            ),
            ClaimError::TooManyTricks { claimed, remaining } => write!(
                f,
                "claimed {} tricks but only {} remain",
                claimed, remaining
            ),
        }
    }
}

impl std::error::Error for ClaimError {}

/// The outcome of checking a claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimVerdict {
    /// The claiming side makes the claimed tricks against every consistent
    /// layout.
    Valid,
    /// Some consistent layout and play holds the claiming side below the
    /// claim.
    Invalid,
    /// The node budget ran out before the claim was settled either way.
    Undecided,
}

/// A verdict and the work it took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaimResult {
    /// Whether the claim holds.
    pub verdict: ClaimVerdict,
    /// Search nodes visited, the double-dummy layout tests' included.
    pub nodes: u64,
}

/// The most tricks the claiming side can guarantee, as far as the budget
/// allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaimLimit {
    /// The largest claim proven valid.
    pub guaranteed: usize,
    /// True if the budget ran out while testing `guaranteed + 1`, so a larger
    /// claim might still be valid. False means `guaranteed + 1` is invalid
    /// (or every remaining trick is guaranteed).
    pub undecided: bool,
    /// Search nodes visited, the double-dummy layout tests' included.
    pub nodes: u64,
}

/// Who sees what, and who chooses the cards, for one claim.
#[derive(Clone, Copy, Debug)]
struct Roles {
    /// Seat parity of the claiming side.
    side: usize,
    /// The two seats the claimer can see: their own and dummy.
    visible: [Seat; 2],
    /// The two seats whose cards are pooled.
    hidden: [Seat; 2],
    /// One bit per seat whose cards the claimer chooses.
    controlled: u8,
}

impl Roles {
    fn new(declarer: Seat, claimer: Seat) -> Self {
        let dummy = partner(declarer);
        if claimer & 1 == declarer & 1 {
            Roles {
                side: declarer & 1,
                visible: [declarer, dummy],
                hidden: [left_hand_opp(declarer), right_hand_opp(declarer)],
                controlled: 1 << declarer | 1 << dummy,
            }
        } else {
            Roles {
                side: claimer & 1,
                visible: [claimer, dummy],
                hidden: [partner(claimer), declarer],
                controlled: 1 << claimer,
            }
        }
    }

    /// Whether `seat`'s tricks count for the claim.
    fn on_side(&self, seat: Seat) -> bool {
        seat & 1 == self.side
    }

    /// Whether the claimer chooses `seat`'s cards.
    fn controls(&self, seat: Seat) -> bool {
        self.controlled & (1 << seat) != 0
    }

    fn is_visible(&self, seat: Seat) -> bool {
        self.visible.contains(&seat)
    }

    /// The pooled seat that is not `seat`.
    fn other_hidden(&self, seat: Seat) -> Seat {
        if self.hidden[0] == seat {
            self.hidden[1]
        } else {
            self.hidden[0]
        }
    }
}

/// A position as the claimer sees it: two hands known, two hands pooled.
#[derive(Clone, Copy, Debug)]
struct State {
    /// Remaining cards of the two visible seats. The pooled seats' entries
    /// are empty.
    hands: Hands,
    /// The pooled seats' remaining cards, split between them in some unknown
    /// way.
    pool: Cards,
    /// Per seat, a bit per suit the seat is known to be void in.
    voids: [u8; NUM_SEATS],
    /// Seat that led to the current trick.
    leader: Seat,
    /// Cards played to the current trick, in play order.
    trick: [usize; NUM_SEATS],
    /// How many cards have been played to the current trick.
    played: usize,
    /// Tricks left, counting the one in progress.
    tricks_left: usize,
}

impl State {
    /// Cards `seat` still holds: one fewer once it has played to the trick
    /// in progress.
    fn holds(&self, seat: Seat) -> usize {
        let has_played = (seat + NUM_SEATS - self.leader) % NUM_SEATS < self.played;
        self.tricks_left - usize::from(has_played)
    }
}

/// A position ready for claim checking.
#[derive(Clone, Debug)]
pub struct ClaimPosition {
    trump: Suit,
    roles: Roles,
    /// Completed tricks already won by the claiming side.
    won: usize,
    state: State,
}

impl ClaimPosition {
    /// Set up a position from a deal and the play so far.
    ///
    /// * `deal` - the hands at the start of play; a full deal or an ending, as
    ///   long as all four hands are the same size
    /// * `trump` - trump suit, or `NOTRUMP`
    /// * `declarer` - the declaring seat; its partner is dummy
    /// * `claimer` - the seat claiming: declarer, or either defender. Dummy is
    ///   treated as declarer, since both see the same two hands.
    /// * `leader` - the seat that leads the first card of `plays`
    /// * `plays` - cards played so far, in order; may end mid-trick
    ///
    /// Every show-out in `plays` is recorded, so later plays from the pool
    /// must be consistent with it.
    pub fn from_play(
        deal: &Hands,
        trump: Suit,
        declarer: Seat,
        claimer: Seat,
        leader: Seat,
        plays: &[usize],
    ) -> Result<Self, ClaimError> {
        let size = deal[WEST].size();
        if (0..NUM_SEATS).any(|s| deal[s].size() != size) {
            return Err(ClaimError::UnequalHands);
        }

        let roles = Roles::new(declarer, claimer);
        let mut hands = *deal;
        let mut voids = [0u8; NUM_SEATS];
        let mut trick = [0usize; NUM_SEATS];
        let mut played = 0;
        let mut leader = leader;
        let mut won = 0;

        for (index, &card) in plays.iter().enumerate() {
            let seat = (leader + played) % NUM_SEATS;
            let led = (played > 0).then(|| suit_of(trick[0]));
            check_play(hands[seat], card, led).map_err(|e| match e {
                IllegalPlay::NotHeld => ClaimError::NotHeld { index, card, seat },
                IllegalPlay::Revoke => ClaimError::Revoke { index, card, seat },
            })?;
            if let Some(led) = led {
                if suit_of(card) != led {
                    voids[seat] |= 1 << led;
                }
            }
            hands[seat].remove(card);
            trick[played] = card;
            played += 1;
            if played == NUM_SEATS {
                leader = trick_winner(trump, leader, &trick);
                if roles.on_side(leader) {
                    won += 1;
                }
                played = 0;
            }
        }

        // The leader of a trick in progress has already played, so the
        // longest hand is the one that has not.
        let tricks_left = (0..NUM_SEATS).map(|s| hands[s].size()).max().unwrap_or(0);

        let mut known = Hands::new();
        for &seat in &roles.visible {
            known[seat] = hands[seat];
        }
        let pool = hands[roles.hidden[0]].union(hands[roles.hidden[1]]);

        Ok(ClaimPosition {
            trump,
            roles,
            won,
            state: State {
                hands: known,
                pool,
                voids,
                leader,
                trick,
                played,
                tricks_left,
            },
        })
    }

    /// Tricks left to play, counting the one in progress.
    pub fn tricks_left(&self) -> usize {
        self.state.tricks_left
    }

    /// Completed tricks the claiming side has already won.
    pub fn tricks_won(&self) -> usize {
        self.won
    }

    /// Check whether the claiming side can guarantee `tricks` of the
    /// remaining tricks, counting the trick in progress.
    pub fn check(&self, tricks: usize, budget: u64) -> Result<ClaimResult, ClaimError> {
        if tricks > self.state.tricks_left {
            return Err(ClaimError::TooManyTricks {
                claimed: tricks,
                remaining: self.state.tricks_left,
            });
        }
        let mut search = Search::new(self.trump, self.roles, budget);
        let verdict = match search.claim(&self.state, tricks) {
            Ok(true) => ClaimVerdict::Valid,
            Ok(false) => ClaimVerdict::Invalid,
            Err(OutOfBudget) => ClaimVerdict::Undecided,
        };
        Ok(ClaimResult {
            verdict,
            nodes: search.nodes,
        })
    }

    /// The largest claim the claiming side could make, within `budget` nodes
    /// in total.
    pub fn max_claim(&self, budget: u64) -> ClaimLimit {
        // One search for every target, so positions proven for a smaller
        // claim are reused when testing the next one.
        let mut search = Search::new(self.trump, self.roles, budget);
        let mut guaranteed = 0;
        let mut undecided = false;
        for tricks in 1..=self.state.tricks_left {
            match search.claim(&self.state, tricks) {
                Ok(true) => guaranteed = tricks,
                Ok(false) => break,
                Err(OutOfBudget) => {
                    undecided = true;
                    break;
                }
            }
        }
        ClaimLimit {
            guaranteed,
            undecided,
            nodes: search.nodes,
        }
    }
}

/// All cards of the suits set in `voids`.
fn suits_mask(voids: u8) -> u64 {
    (0..NUM_SUITS)
        .filter(|s| voids & (1 << s) != 0)
        .fold(0, |m, s| m | mask_of(s))
}

/// Whether `pool` can be dealt as `na` cards to a seat void in `void_a` and
/// `nb` cards to a seat void in `void_b`.
///
/// With two hands this is Hall's condition: the cards only `b` can hold must
/// fit in `b`, likewise for `a`, and no card may be barred from both.
fn can_split(pool: Cards, void_a: u8, na: usize, void_b: u8, nb: usize) -> bool {
    let (p, va, vb) = (pool.value(), suits_mask(void_a), suits_mask(void_b));
    p & va & vb == 0
        && (p & va & !vb).count_ones() as usize <= nb
        && (p & vb & !va).count_ones() as usize <= na
}

/// The cards of `suit` in `holding` that are higher than every card of it in
/// `others`.
fn top_cards(holding: Cards, others: Cards, suit: Suit) -> Cards {
    let mine = holding.suit(suit);
    let theirs = others.suit(suit);
    if theirs.is_empty() {
        mine
    } else {
        mine.slice(suit * NUM_RANKS, theirs.top())
    }
}

/// How many of `winners`, led one after another, win without the leader's
/// partner having to overtake: all of them if partner can always play lower
/// or discard, else one per lower card partner holds.
fn cashable(winners: Cards, partner_hand: Cards, suit: Suit) -> usize {
    let theirs = partner_hand.suit(suit);
    let below = theirs
        .slice(winners.bottom() + 1, (suit + 1) * NUM_RANKS)
        .size();
    if below == theirs.size() {
        winners.size()
    } else {
        winners.size().min(below)
    }
}

/// The cards in `own` from `suit` that are worth trying: one per run of cards
/// with nothing from `live` between them. Highest first.
fn push_runs(own: Cards, live: Cards, suit: Suit, moves: &mut Moves) {
    let mut prev_own = false;
    for card in live.suit(suit).union(own.suit(suit)).iter() {
        let is_own = own.have(card);
        if is_own && !prev_own {
            moves.push(card);
        }
        prev_own = is_own;
    }
}

/// Like [`push_runs`], but for a seat whose cards are pooled with a seat
/// still to play to this trick: the run's top card, its bottom card and one
/// from the middle.
///
/// The run is shared between the two pooled seats, so the card played now
/// decides what the other seat can do with the rest of it this trick: the
/// top card leaves it only lower cards, the bottom only higher ones, and a
/// middle card both. Every middle card offers the same, so one stands for
/// all of them.
fn push_run_ends(own: Cards, live: Cards, suit: Suit, moves: &mut Moves) {
    let mut run = Cards::new();
    let flush = |run: &mut Cards, moves: &mut Moves| {
        if !run.is_empty() {
            let mut rest = *run;
            moves.push(rest.top());
            rest.remove(rest.top());
            if rest.size() > 1 {
                moves.push(rest.top());
            }
            if !rest.is_empty() {
                moves.push(rest.bottom());
            }
            *run = Cards::new();
        }
    };
    for card in live.suit(suit).union(own.suit(suit)).iter() {
        if own.have(card) {
            run.add(card);
        } else {
            flush(&mut run, moves);
        }
    }
    flush(&mut run, moves);
}

/// A fixed-capacity move list. A seat can have at most one candidate per
/// pooled card, and the pool holds at most 26.
struct Moves {
    cards: [usize; 2 * TOTAL_TRICKS],
    len: usize,
}

impl Moves {
    fn new() -> Self {
        Moves {
            cards: [0; 2 * TOTAL_TRICKS],
            len: 0,
        }
    }

    fn push(&mut self, card: usize) {
        if self.len < self.cards.len() {
            self.cards[self.len] = card;
            self.len += 1;
        }
    }

    fn as_slice(&self) -> &[usize] {
        &self.cards[..self.len]
    }
}

struct OutOfBudget;

/// Claims with fewer tricks left than this skip the double-dummy test: the
/// claim search settles them faster on its own.
const LAYOUT_MIN_TRICKS: usize = 5;
/// Random layouts tried, after the eight that each give one pooled seat every
/// pooled card of a suit.
const RANDOM_LAYOUTS: usize = 64;

/// Double-dummy refutation.
///
/// A claim has to hold against every layout of the pool, so a single layout
/// on which double dummy holds the claiming side below its target refutes
/// it. That is never true the other way round: a claim can fail because the
/// claimer has to guess, with every individual layout makeable. So this only
/// ever answers "refuted" or "don't know".
///
/// It is tried once, before the claim search, and not at the search's inner
/// positions: measured on random endings, tests there cost more than they
/// saved, since a claim that no layout refutes at the start is one that
/// fails by a guess, and stays that way.
struct Layouts {
    cutoff: CutoffCache,
    pattern: PatternCache,
    rng: u64,
}

impl Layouts {
    fn new() -> Self {
        Layouts {
            cutoff: CutoffCache::new(12),
            pattern: PatternCache::new(12),
            rng: 0x9e37_79b9_7f4a_7c15,
        }
    }

    fn random(&mut self) -> u64 {
        // xorshift64: deterministic, so verdicts and node counts repeat.
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    /// The remaining hands of a full deal consistent with `s`, giving the
    /// first pooled seat as many of the cards in `prefer` as the hand sizes
    /// and voids allow. `None` only if no split exists at all.
    // `x & x.wrapping_neg()` is the classic isolate-lowest-set-bit idiom; see
    // `pattern::unpack_bits` for why the stable-but-unstable method is not
    // used. `isolate_lowest_one` broke dealer3's wasm build, whose toolchain
    // predates it.
    #[allow(unknown_lints, clippy::manual_isolate_lowest_one)]
    fn deal(roles: &Roles, s: &State, prefer: u64) -> Option<Hands> {
        let [a, b] = roles.hidden;
        let n = s.holds(a);
        let pool = s.pool.value();
        let only_a = pool & suits_mask(s.voids[b]);
        let only_b = pool & suits_mask(s.voids[a]);
        if only_a & only_b != 0 {
            return None;
        }
        let free = pool & !only_a & !only_b;
        let mut to_a = only_a | (free & prefer);
        while (to_a.count_ones() as usize) < n {
            let spare = free & !to_a;
            if spare == 0 {
                return None;
            }
            to_a |= spare & spare.wrapping_neg();
        }
        while (to_a.count_ones() as usize) > n {
            let movable = free & to_a;
            if movable == 0 {
                return None;
            }
            to_a &= !(movable & movable.wrapping_neg());
        }
        let mut hands = s.hands;
        hands[a] = Cards::from_bits(to_a);
        hands[b] = Cards::from_bits(pool & !to_a);
        Some(hands)
    }

    /// Whether double dummy holds the claiming side below `need` on the
    /// layout `prefer` describes, and the nodes that took.
    fn refutes(
        &mut self,
        trump: Suit,
        roles: &Roles,
        s: &State,
        need: usize,
        prefer: u64,
    ) -> (bool, u64) {
        let Some(mut hands) = Self::deal(roles, s, prefer) else {
            return (false, 0);
        };
        let mut trick = PartialTrick::new();
        for (i, &card) in s.trick[..s.played].iter().enumerate() {
            trick.add(card, (s.leader + i) % NUM_SEATS);
        }
        // The DD search answers "can North-South take `beta`?".
        let ns_side = roles.on_side(NORTH);
        let beta = if ns_side {
            need
        } else {
            s.tricks_left + 1 - need
        };
        let mut dd = DdSearch::new_with_partial_trick(
            &mut hands,
            trump,
            s.leader,
            &mut self.cutoff,
            &mut self.pattern,
            Some(&trick),
        );
        let ns_makes = dd.search(beta as i8) as usize >= beta;
        (ns_makes != ns_side, dd.nodes())
    }
}

/// A multiply-rotate hasher for the search's small integer keys, in the
/// manner of rustc's `FxHasher`. The default SipHash guards against
/// adversarial keys, which card sets are not, and costs several times more.
#[derive(Default)]
struct FastHasher(u64);

impl Hasher for FastHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(u64::from(b));
        }
    }

    fn write_u8(&mut self, n: u8) {
        self.write_u64(u64::from(n));
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<FastHasher>>;

/// Transposition key at a trick boundary: the two visible hands, the pool,
/// both pooled seats' voids and the leader.
type Key = (u64, u64, u64, u8, u8, u8);

struct Search {
    trump: Suit,
    roles: Roles,
    /// Per position, `(lo, hi)`: claims of `lo` tricks or fewer are proven,
    /// claims of `hi` or more are refuted.
    memo: FastMap<Key, (u8, u8)>,
    /// [`Self::cash`] results, by both hands and the adversary's cards.
    cash_memo: FastMap<(u64, u64, u64), u8>,
    nodes: u64,
    budget: u64,
    /// Off only in tests: try every card and skip the sure-trick bounds, as
    /// a reference for the shortcuts.
    shortcuts: bool,
    layouts: Layouts,
    /// [`LAYOUT_MIN_TRICKS`], lowered in tests so small endings exercise it.
    layout_min_tricks: usize,
}

impl Search {
    fn new(trump: Suit, roles: Roles, budget: u64) -> Self {
        Search {
            trump,
            roles,
            memo: FastMap::default(),
            cash_memo: FastMap::default(),
            nodes: 0,
            budget,
            shortcuts: true,
            layouts: Layouts::new(),
            layout_min_tricks: LAYOUT_MIN_TRICKS,
        }
    }

    /// Whether the claiming side can take `need` of the tricks left in `s`,
    /// as [`Self::wins`], trying first to refute the claim with a single
    /// layout.
    fn claim(&mut self, s: &State, need: usize) -> Result<bool, OutOfBudget> {
        if self.shortcuts
            && need > 0
            && need <= s.tricks_left
            && s.tricks_left >= self.layout_min_tricks
            && self.layout_refutes(s, need)?
        {
            return Ok(false);
        }
        self.wins(s, need)
    }

    /// Whether some layout consistent with `s` refutes a claim of `need`
    /// tricks by double dummy: every suit given wholly to one pooled seat or
    /// the other, then random layouts.
    fn layout_refutes(&mut self, s: &State, need: usize) -> Result<bool, OutOfBudget> {
        let bad_breaks = (0..NUM_SUITS).flat_map(|suit| [mask_of(suit), !mask_of(suit)]);
        let layouts: Vec<u64> = bad_breaks
            .chain((0..RANDOM_LAYOUTS).map(|_| self.layouts.random()))
            .collect();
        for layout in layouts {
            let (refuted, nodes) = self
                .layouts
                .refutes(self.trump, &self.roles, s, need, layout);
            self.nodes += nodes;
            if self.nodes > self.budget {
                return Err(OutOfBudget);
            }
            if refuted {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn key(&self, s: &State) -> Key {
        let r = &self.roles;
        (
            s.hands[r.visible[0]].value(),
            s.hands[r.visible[1]].value(),
            s.pool.value(),
            s.voids[r.hidden[0]],
            s.voids[r.hidden[1]],
            s.leader as u8,
        )
    }

    /// Whether the claiming side can take `need` of the tricks left in `s`.
    fn wins(&mut self, s: &State, need: usize) -> Result<bool, OutOfBudget> {
        if need == 0 {
            return Ok(true);
        }
        if need > s.tricks_left {
            return Ok(false);
        }
        if s.played == 0 && self.shortcuts {
            if self.roles.controls(s.leader) {
                if self.claimer_sure_tricks(s) >= need {
                    return Ok(true);
                }
            } else if !self.roles.on_side(s.leader) {
                if need + self.adversary_sure_tricks(s) > s.tricks_left {
                    return Ok(false);
                }
                if self.claimer_sure_after_lead(s) >= need {
                    return Ok(true);
                }
            }
        }

        let key = (s.played == 0).then(|| self.key(s));
        if let Some(k) = key {
            if let Some(&(lo, hi)) = self.memo.get(&k) {
                if need <= lo as usize {
                    return Ok(true);
                }
                if need >= hi as usize {
                    return Ok(false);
                }
            }
        }

        self.nodes += 1;
        if self.nodes > self.budget {
            return Err(OutOfBudget);
        }

        let seat = (s.leader + s.played) % NUM_SEATS;
        let ours = self.roles.controls(seat);
        let mut moves = if self.roles.is_visible(seat) {
            self.visible_moves(s, seat)
        } else {
            self.pooled_moves(s, seat)
        };
        self.order(s, seat, &mut moves);

        // The claimer needs one move that works; the adversary needs one
        // that beats the claim.
        let mut result = !ours;
        for &card in moves.as_slice() {
            let (next, next_need) = self.play(s, seat, card, need);
            if self.wins(&next, next_need)? == ours {
                result = ours;
                break;
            }
        }

        if let Some(k) = key {
            let entry = self.memo.entry(k).or_insert((0, s.tricks_left as u8 + 1));
            if result {
                entry.0 = entry.0.max(need as u8);
            } else {
                entry.1 = entry.1.min(need as u8);
            }
        }
        Ok(result)
    }

    /// Every card the claimer cannot count on: the pool, which may include
    /// the claimer's own partner's cards, and the visible opponents' hand.
    fn against(&self, s: &State) -> Cards {
        self.roles
            .visible
            .iter()
            .filter(|&&v| !self.roles.on_side(v))
            .fold(s.pool, |c, &v| c.union(s.hands[v]))
    }

    /// The claiming side's visible cards.
    fn ours_seen(&self, s: &State) -> Cards {
        self.roles
            .visible
            .iter()
            .filter(|&&v| self.roles.on_side(v))
            .fold(Cards::new(), |c, &v| c.union(s.hands[v]))
    }

    /// Whether the claimer's partner is pooled, as in a defender's claim.
    fn partner_pooled(&self) -> bool {
        self.roles.hidden.iter().any(|&h| self.roles.on_side(h))
    }

    /// Tricks the claimer, on lead, can cash without giving up the lead,
    /// however the pool is split.
    ///
    /// With partner visible, as in a claim by declarer, that is
    /// [`Self::cash`] over both hands. Otherwise only the leading hand cashes:
    /// a card counts if it beats every card the claimer cannot count on, and
    /// while any of those may be a trump only trumps count, since anything
    /// else could be ruffed. A pooled partner's cards are among those beaten,
    /// so it cannot overtake.
    fn claimer_sure_tricks(&mut self, s: &State) -> usize {
        let lead = s.hands[s.leader];
        let mate = partner(s.leader);
        let against = self.against(s);
        if self.roles.is_visible(mate) {
            return self.cash(lead, s.hands[mate], against);
        }
        let trump_threat = self.trump < NUM_SUITS && !against.suit(self.trump).is_empty();
        let mut sure = 0;
        for suit in 0..NUM_SUITS {
            if trump_threat && suit != self.trump {
                continue;
            }
            let winners = top_cards(lead, against, suit);
            if winners.is_empty() {
                continue;
            }
            sure += if self.roles.is_visible(mate) {
                cashable(winners, s.hands[mate], suit)
            } else {
                winners.size()
            };
        }
        sure
    }

    /// Tricks the claiming side can cash from `lead` and `mate`, both
    /// visible and controlled, with `lead` on lead, without ever giving up
    /// the lead, against adversary cards no better than `against`.
    ///
    /// `against` is a pessimistic stand-in for what the adversary still
    /// holds. Each round of a suit removes only its lowest card of the suit,
    /// as if one hand held them all and kept the high ones; a real holding
    /// is then never longer and never higher, suit by suit, so whatever wins
    /// against `against` wins against it too. That is what lets small cards
    /// become winners once the suit is exhausted (the 2 of AKQ2), an idea
    /// taken from Bethe's DTAC, which counts rounds per suit the same way.
    ///
    /// While `against` may hold a trump only trumps are led, since anything
    /// else could be ruffed. Once it holds none, side suits are safe, and
    /// partner may ruff a card of a suit partner is void in, which wins a
    /// trick with a loser or crosses to partner's hand.
    ///
    /// Within that the search is exact over the plans it tries: leading the
    /// top or bottom winner of a suit, or its lowest card for partner to
    /// ruff, with partner following low, overtaking as cheaply as possible,
    /// or discarding (or ruffing with) the lowest card of a suit. Every plan
    /// it finds works whatever the adversary does, so trying fewer plans
    /// than exist only weakens the bound, never its soundness.
    fn cash(&mut self, lead: Cards, mate: Cards, against: Cards) -> usize {
        if lead.is_empty() {
            return 0;
        }
        let key = (lead.value(), mate.value(), against.value());
        if let Some(&tricks) = self.cash_memo.get(&key) {
            return tricks as usize;
        }
        let trump = self.trump;
        let has_trumps = trump < NUM_SUITS;
        let trumps_out = has_trumps && !against.suit(trump).is_empty();
        let mut best = 0;
        for suit in 0..NUM_SUITS {
            let ours = lead.suit(suit);
            let is_trump = suit == trump;
            if ours.is_empty() || (trumps_out && !is_trump) {
                continue;
            }
            let theirs = against.suit(suit);
            let winners = if theirs.is_empty() {
                ours
            } else {
                ours.slice(suit * NUM_RANKS, theirs.top())
            };
            let follows = mate.suit(suit);
            let ruffable = has_trumps
                && !trumps_out
                && !is_trump
                && follows.is_empty()
                && !mate.suit(trump).is_empty();

            let mut leads = Moves::new();
            if !winners.is_empty() {
                leads.push(winners.top());
                if winners.size() > 1 {
                    leads.push(winners.bottom());
                }
            }
            if ruffable && !winners.have(ours.bottom()) {
                leads.push(ours.bottom());
            }
            let mut next_against = against;
            if !theirs.is_empty() {
                next_against.remove(theirs.bottom());
            }

            for &card in leads.as_slice() {
                let mut replies = Moves::new();
                if follows.is_empty() {
                    for d in 0..NUM_SUITS {
                        if !mate.suit(d).is_empty() {
                            replies.push(mate.suit(d).bottom());
                        }
                    }
                } else {
                    replies.push(follows.bottom());
                    let over = follows.slice(suit * NUM_RANKS, card);
                    if !over.is_empty() {
                        replies.push(over.bottom());
                    }
                }
                for &reply in replies.as_slice() {
                    let ruffs = has_trumps && !is_trump && suit_of(reply) == trump;
                    // A loser is led only for partner to ruff it.
                    if !winners.have(card) && !ruffs {
                        continue;
                    }
                    let overtakes = suit_of(reply) == suit && higher_rank(reply, card);
                    let (mut l, mut m) = (lead, mate);
                    l.remove(card);
                    m.remove(reply);
                    let rest = if overtakes || ruffs {
                        self.cash(m, l, next_against)
                    } else {
                        self.cash(l, m, next_against)
                    };
                    best = best.max(1 + rest);
                    if best == lead.size() {
                        self.cash_memo.insert(key, best as u8);
                        return best;
                    }
                }
            }
        }
        self.cash_memo.insert(key, best as u8);
        best
    }

    /// Tricks a declarer claim is sure of when a pooled defender is on lead:
    /// whatever suit is led, declarer's side wins the trick and then cashes
    /// [`Self::cash`] from the hand that won it. Zero for a defender's claim,
    /// or when any suit the leader may hold could beat it.
    ///
    /// A suit is safe when either of declarer's hands can win the trick for
    /// certain: with a card above every pooled card of the suit, provided
    /// it cannot be ruffed (the suit is trumps, or no pooled trump is left);
    /// or by ruffing a suit it is void in, provided it cannot be overruffed
    /// (no pooled trump is left, or the ruff is above all of them). The
    /// other hand plays its lowest card of the suit, or discards its lowest
    /// card of some suit, whichever leaves the most to cash. As in `cash`,
    /// the trick is assumed to remove only the pool's lowest card of the led
    /// suit. This is DTAC's "opponents on lead" AND node.
    fn claimer_sure_after_lead(&mut self, s: &State) -> usize {
        if self.partner_pooled() || self.roles.is_visible(s.leader) {
            return 0;
        }
        let trump = self.trump;
        let has_trumps = trump < NUM_SUITS;
        let pool = s.pool;
        let pool_trumps = if has_trumps {
            pool.suit(trump)
        } else {
            Cards::new()
        };
        let (second, fourth) = ((s.leader + 1) % NUM_SEATS, (s.leader + 3) % NUM_SEATS);
        let mut sure = usize::MAX;
        for suit in 0..NUM_SUITS {
            let led = pool.suit(suit);
            if led.is_empty() || s.voids[s.leader] & (1 << suit) != 0 {
                continue;
            }
            let is_trump = suit == trump;
            let mut after = pool;
            after.remove(led.bottom());
            let mut best = 0;
            for (winner, other) in [(second, fourth), (fourth, second)] {
                let hand = s.hands[winner];
                let mine = hand.suit(suit);
                // The card that wins the trick, if this hand has one.
                let card = if !mine.is_empty() {
                    let top = mine.top();
                    let unruffable = is_trump || pool_trumps.is_empty();
                    (higher_rank(top, led.top()) && unruffable).then_some(top)
                } else if has_trumps && !is_trump && !hand.suit(trump).is_empty() {
                    let ruff = if pool_trumps.is_empty() {
                        hand.suit(trump).bottom()
                    } else {
                        hand.suit(trump).top()
                    };
                    (pool_trumps.is_empty() || higher_rank(ruff, pool_trumps.top())).then_some(ruff)
                } else {
                    None
                };
                let Some(card) = card else { continue };
                let mut won = hand;
                won.remove(card);
                let rest = s.hands[other];
                let follows = rest.suit(suit);
                let discards: Vec<usize> = if follows.is_empty() {
                    (0..NUM_SUITS)
                        .filter(|&d| !rest.suit(d).is_empty())
                        .map(|d| rest.suit(d).bottom())
                        .collect()
                } else {
                    vec![follows.bottom()]
                };
                for discard in discards {
                    let mut left = rest;
                    left.remove(discard);
                    best = best.max(1 + self.cash(won, left, after));
                }
            }
            sure = sure.min(best);
            if sure == 0 {
                return 0;
            }
        }
        if sure == usize::MAX {
            0
        } else {
            sure
        }
    }

    /// Tricks the adversary, on lead, can be dealt and cash at once.
    ///
    /// The adversary chooses the layout, so a pooled leader may hold every
    /// pooled card that beats all the claiming side's visible cards in its
    /// suit, as long as that leaves a consistent split. Any other pooled
    /// cards of the suit are lower, so a pooled partner of the claimer cannot
    /// beat them either. Outside trumps they count only when the claiming
    /// side cannot hold a trump. Returns 0 when the cards cannot all be given
    /// to the leader, which is always a safe answer.
    fn adversary_sure_tricks(&self, s: &State) -> usize {
        let lead = s.leader;
        let mate = partner(lead);
        let ours = self.ours_seen(s);
        let partner_pooled = self.partner_pooled();
        let trump_threat = self.trump < NUM_SUITS
            && (!ours.suit(self.trump).is_empty()
                || (partner_pooled && !s.pool.suit(self.trump).is_empty()));

        if self.roles.is_visible(lead) {
            // Dummy, against a defender's claim. Its winners must also beat
            // the pool, where the claimer's partner may be.
            let beaten = if partner_pooled {
                ours.union(s.pool)
            } else {
                ours
            };
            return (0..NUM_SUITS)
                .filter(|&suit| !trump_threat || suit == self.trump)
                .map(|suit| top_cards(s.hands[lead], beaten, suit).size())
                .sum();
        }

        let mut held = Cards::new();
        let mut sure = 0;
        for suit in 0..NUM_SUITS {
            if s.voids[lead] & (1 << suit) != 0 || (trump_threat && suit != self.trump) {
                continue;
            }
            let winners = top_cards(s.pool, ours, suit);
            if winners.is_empty() {
                continue;
            }
            held.add_cards(winners);
            sure += if self.roles.is_visible(mate) {
                cashable(winners, s.hands[mate], suit)
            } else {
                winners.size()
            };
        }
        let k = held.size();
        if k == 0 || k > s.tricks_left {
            return 0;
        }
        let other = self.roles.other_hidden(lead);
        let rest = s.pool.different(held);
        if can_split(
            rest,
            s.voids[lead],
            s.tricks_left - k,
            s.voids[other],
            s.tricks_left,
        ) {
            sure
        } else {
            0
        }
    }

    /// Put the moves most likely to settle the node first. Ordering never
    /// changes a verdict, only how soon it is found.
    ///
    /// On lead, the claimer cashes winners first and the adversary leads
    /// high. Later in the trick, a seat plays low when the trick is already
    /// going the way its chooser wants; otherwise it wins as cheaply as it
    /// can, if winning helps its chooser, or plays low.
    fn order(&self, s: &State, seat: Seat, moves: &mut Moves) {
        let pro = self.roles.controls(seat);
        let against = self.against(s);
        let score = |card: usize| -> i32 {
            let rank = rank_of(card) as i32;
            if s.played == 0 {
                if !pro {
                    return rank;
                }
                let ruffable = self.trump < NUM_SUITS
                    && suit_of(card) != self.trump
                    && !against.suit(self.trump).is_empty();
                let top = against.suit(suit_of(card));
                let winner = !ruffable && (top.is_empty() || higher_rank(card, top.top()));
                return if winner { 100 + rank } else { -rank };
            }
            let mut best = 0;
            for i in 1..s.played {
                if wins_over(s.trick[i], s.trick[best], self.trump) {
                    best = i;
                }
            }
            let best_seat = (s.leader + best) % NUM_SEATS;
            let going_well = self.roles.on_side(best_seat) == pro;
            let winning_helps = self.roles.on_side(seat) == pro;
            if !going_well && winning_helps && wins_over(card, s.trick[best], self.trump) {
                100 - rank
            } else {
                -rank
            }
        };
        let slice = &mut moves.cards[..moves.len];
        slice.sort_by_key(|&card| std::cmp::Reverse(score(card)));
    }

    /// Every card that is still out, plus those played to this trick. Two
    /// cards of one holder are interchangeable only with none of these
    /// between them.
    fn live(s: &State) -> Cards {
        let mut live = s.hands.all_cards().union(s.pool);
        for &card in &s.trick[..s.played] {
            live.add(card);
        }
        live
    }

    fn led_suit(s: &State) -> Option<Suit> {
        (s.played > 0).then(|| suit_of(s.trick[0]))
    }

    /// Legal cards of a visible seat, one per run of equivalent cards.
    fn visible_moves(&self, s: &State, seat: Seat) -> Moves {
        let legal = get_playable_cards(&s.hands, seat, Self::led_suit(s));
        let live = Self::live(s);
        let mut moves = Moves::new();
        for suit in 0..NUM_SUITS {
            if self.shortcuts {
                push_runs(legal, live, suit, &mut moves);
            } else {
                legal.suit(suit).iter().for_each(|c| moves.push(c));
            }
        }
        moves
    }

    /// Cards a pooled seat could play: any suit it has not shown out of, as
    /// long as the rest of the pool can still be split between the two
    /// pooled seats afterwards.
    fn pooled_moves(&self, s: &State, seat: Seat) -> Moves {
        let other = self.roles.other_hidden(seat);
        let led = Self::led_suit(s);
        let has_played = |d: Seat| s.holds(d) < s.tricks_left;
        let (n_seat, n_other) = (s.holds(seat) - 1, s.holds(other));

        let live = Self::live(s);
        let mut moves = Moves::new();
        for suit in 0..NUM_SUITS {
            let cards = s.pool.suit(suit);
            if cards.is_empty() || s.voids[seat] & (1 << suit) != 0 {
                continue;
            }
            // Which card of the suit is played does not affect whether the
            // rest can be split, so test the suit once.
            let mut voids = s.voids[seat];
            if let Some(led) = led {
                if led != suit {
                    voids |= 1 << led;
                }
            }
            let rest = s.pool.different(Cards::from_bits(1 << cards.top()));
            if !can_split(rest, voids, n_seat, s.voids[other], n_other) {
                continue;
            }
            if !self.shortcuts {
                cards.iter().for_each(|c| moves.push(c));
            } else if has_played(other) {
                push_runs(s.pool, live, suit, &mut moves);
            } else {
                push_run_ends(s.pool, live, suit, &mut moves);
            }
        }
        moves
    }

    /// The position after `seat` plays `card`, and the tricks the claiming
    /// side still needs from it.
    fn play(&self, s: &State, seat: Seat, card: usize, need: usize) -> (State, usize) {
        let mut n = *s;
        if self.roles.is_visible(seat) {
            n.hands[seat].remove(card);
        } else {
            n.pool.remove(card);
            if let Some(led) = Self::led_suit(s) {
                if suit_of(card) != led {
                    n.voids[seat] |= 1 << led;
                }
            }
        }
        n.trick[n.played] = card;
        n.played += 1;

        let mut need = need;
        if n.played == NUM_SEATS {
            let winner = trick_winner(self.trump, n.leader, &n.trick);
            if self.roles.on_side(winner) {
                need -= 1;
            }
            n.leader = winner;
            n.played = 0;
            n.tricks_left -= 1;
        }
        (n, need)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Solver;

    fn ending(pbn: &str) -> Hands {
        Hands::from_pbn(pbn).expect("test deal parses")
    }

    fn card(s: &str) -> usize {
        let mut chars = s.chars();
        let suit = chars.next().and_then(char_to_suit).expect("suit");
        let rank = chars.next().and_then(char_to_rank).expect("rank");
        card_of(suit, rank)
    }

    fn verdict(pos: &ClaimPosition, tricks: usize) -> ClaimVerdict {
        pos.check(tricks, DEFAULT_CLAIM_BUDGET)
            .expect("claim within range")
            .verdict
    }

    #[test]
    fn top_tricks_are_a_valid_claim() {
        // North: SAK, South: HAK. Two top tricks whatever the defenders hold.
        let deal = ending("N:AK... 43... .AK.. ..32.");
        let pos =
            ClaimPosition::from_play(&deal, NOTRUMP, SOUTH, SOUTH, NORTH, &[]).expect("valid");
        assert_eq!(verdict(&pos, 2), ClaimVerdict::Valid);
    }

    #[test]
    fn finesse_is_not_a_claim() {
        // South leads towards SAQ; West holds the king, so double dummy
        // makes both tricks, but a claim cannot rely on the finesse.
        let deal = ending("N:AQ... ..32. 32... K4...");
        let dd = Solver::new(deal, NOTRUMP, SOUTH).solve();
        assert_eq!(dd, 2);

        let pos =
            ClaimPosition::from_play(&deal, NOTRUMP, SOUTH, SOUTH, SOUTH, &[]).expect("valid");
        assert_eq!(verdict(&pos, 2), ClaimVerdict::Invalid);
        assert_eq!(verdict(&pos, 1), ClaimVerdict::Valid);
        let limit = pos.max_claim(DEFAULT_CLAIM_BUDGET);
        assert_eq!(limit.guaranteed, 1);
        assert!(!limit.undecided);
    }

    #[test]
    fn a_known_void_makes_the_finesse_a_claim() {
        // The same ending, but East has shown out of spades, so the king is
        // known to be with West.
        let deal = ending("N:AQ... ..32. 32... K4...");
        let mut pos =
            ClaimPosition::from_play(&deal, NOTRUMP, SOUTH, SOUTH, SOUTH, &[]).expect("valid");
        pos.state.voids[EAST] |= 1 << SPADE;
        assert_eq!(verdict(&pos, 2), ClaimVerdict::Valid);
    }

    #[test]
    fn a_bad_split_defeats_the_claim() {
        // North cashes SAK and hopes the 3 is good. With the actual 2-1 split
        // it is, but West could equally hold all three missing spades.
        let deal = ending("N:AK3... T...43 .AKQ.. QJ...2");
        assert_eq!(Solver::new(deal, NOTRUMP, NORTH).solve(), 3);
        let pos =
            ClaimPosition::from_play(&deal, NOTRUMP, SOUTH, SOUTH, NORTH, &[]).expect("valid");
        assert_eq!(verdict(&pos, 3), ClaimVerdict::Invalid);
        assert_eq!(verdict(&pos, 2), ClaimVerdict::Valid);
    }

    #[test]
    fn show_outs_in_the_play_are_recorded() {
        // North leads SA and East discards a club: East is void in spades.
        let deal = ending("N:AK3..2. ..43.32 2.AKQ.. QJT7...");
        let plays = [card("SA"), card("C2"), card("S2"), card("S7")];
        let pos =
            ClaimPosition::from_play(&deal, NOTRUMP, SOUTH, SOUTH, NORTH, &plays).expect("valid");
        assert_eq!(pos.state.voids[EAST], 1 << SPADE);
        assert_eq!(pos.state.voids[WEST], 0);
        assert_eq!(pos.state.leader, NORTH);
        assert_eq!(pos.tricks_left(), 3);
        assert_eq!(pos.tricks_won(), 1);
    }

    #[test]
    fn revokes_are_rejected() {
        // East holds the spade ten, so a club discard is a revoke.
        let deal = ending("N:AK3..2. T..4.32 2.AKQ.. QJ7.J..");
        let plays = [card("SA"), card("C2")];
        let err = ClaimPosition::from_play(&deal, NOTRUMP, SOUTH, SOUTH, NORTH, &plays)
            .expect_err("revoke");
        assert!(matches!(err, ClaimError::Revoke { index: 1, .. }));
    }

    #[test]
    fn mid_trick_claims_count_the_current_trick() {
        // West has led a low heart to the trick; North's ace wins it.
        let deal = ending("N:.A2.. K.4.. ..A3. .Q5..");
        let plays = [card("H5")];
        let pos =
            ClaimPosition::from_play(&deal, NOTRUMP, SOUTH, SOUTH, WEST, &plays).expect("valid");
        assert_eq!(pos.tricks_left(), 2);
        assert_eq!(verdict(&pos, 1), ClaimVerdict::Valid);
    }

    #[test]
    fn budget_exhaustion_is_undecided() {
        let deal = ending("N:AQ... ..32. 32... K4...");
        let pos =
            ClaimPosition::from_play(&deal, NOTRUMP, SOUTH, SOUTH, SOUTH, &[]).expect("valid");
        let result = pos.check(2, 1).expect("in range");
        assert_eq!(result.verdict, ClaimVerdict::Undecided);
    }

    #[test]
    fn a_defender_with_the_last_trumps_can_claim() {
        // Spades are trumps; West holds the only two left.
        let deal = ending("N:.AK.. ..32. ...AK AK...");
        let pos = ClaimPosition::from_play(&deal, SPADE, SOUTH, WEST, WEST, &[]).expect("valid");
        assert_eq!(verdict(&pos, 2), ClaimVerdict::Valid);
    }

    #[test]
    fn a_defender_cannot_count_on_partner() {
        // West cashes SA and leads the 2. East's king would win the second
        // trick, but West cannot see it: with the king elsewhere, or with
        // East ducking, declarer's jack wins.
        let deal = ending("N:..32. K...3 QJ... A2...");
        let ns = Solver::new(deal, NOTRUMP, WEST).solve();
        assert_eq!(ns, 0, "double dummy, the defence takes both");
        let pos = ClaimPosition::from_play(&deal, NOTRUMP, SOUTH, WEST, WEST, &[]).expect("valid");
        assert_eq!(verdict(&pos, 2), ClaimVerdict::Invalid);
        assert_eq!(verdict(&pos, 1), ClaimVerdict::Valid);
    }

    #[test]
    fn a_defender_sees_dummy() {
        // Dummy's king is singleton, so West's ace drops it and the queen is
        // good. Had the king been in declarer's hand, it could have been
        // guarded.
        let deal = ending("N:K.2.. ..32. .43.. AQ...");
        let pos = ClaimPosition::from_play(&deal, NOTRUMP, SOUTH, WEST, WEST, &[]).expect("valid");
        assert_eq!(verdict(&pos, 2), ClaimVerdict::Valid);
    }

    #[test]
    fn tricks_won_count_for_the_claiming_side() {
        // North's SA wins the first trick: one for declarer, none for West.
        let deal = ending("N:AK3..2. ..43.32 2.AKQ.. QJT7...");
        let plays = [card("SA"), card("C2"), card("S2"), card("S7")];
        let pos =
            ClaimPosition::from_play(&deal, NOTRUMP, SOUTH, WEST, NORTH, &plays).expect("valid");
        assert_eq!(pos.tricks_won(), 0);
    }

    #[test]
    fn shortcuts_never_change_a_verdict() {
        // Collapsing equivalent cards, the sure-trick bounds and the
        // double-dummy layout test exist only for speed. Compare every claim
        // against the same search trying every card with none of them.
        let mut rng = Lcg(0xc1a1);
        for _ in 0..400 {
            let size = 2 + rng.next(3);
            let deal = random_ending(&mut rng, size);
            let trump = rng.next(NUM_SUITS + 1);
            let declarer = rng.next(NUM_SEATS);
            let claimer = rng.next(NUM_SEATS);
            let leader = rng.next(NUM_SEATS);
            // Up to three cards of the first trick already played, so claims
            // made mid-trick are covered too.
            let mut hands = deal;
            let mut plays = Vec::new();
            for i in 0..rng.next(NUM_SEATS) {
                let seat = (leader + i) % NUM_SEATS;
                let led = plays.first().map(|&c| suit_of(c));
                let legal: Vec<usize> = get_playable_cards(&hands, seat, led).iter().collect();
                let card = legal[rng.next(legal.len())];
                hands[seat].remove(card);
                plays.push(card);
            }
            let pos = ClaimPosition::from_play(&deal, trump, declarer, claimer, leader, &plays)
                .expect("valid");
            for tricks in 1..=size {
                let fast = verdict(&pos, tricks);
                let mut layouts = Search::new(trump, pos.roles, u64::MAX);
                layouts.layout_min_tricks = 1;
                let with_layouts = match layouts.claim(&pos.state, tricks) {
                    Ok(true) => ClaimVerdict::Valid,
                    Ok(false) => ClaimVerdict::Invalid,
                    Err(OutOfBudget) => ClaimVerdict::Undecided,
                };
                assert_eq!(
                    fast, with_layouts,
                    "layout test, {tricks} of {deal:?} after {plays:?}"
                );
                let mut reference = Search::new(trump, pos.roles, u64::MAX);
                reference.shortcuts = false;
                let slow = match reference.wins(&pos.state, tricks) {
                    Ok(true) => ClaimVerdict::Valid,
                    Ok(false) => ClaimVerdict::Invalid,
                    Err(OutOfBudget) => ClaimVerdict::Undecided,
                };
                assert_eq!(
                    fast,
                    slow,
                    "{} claims {} of {:?} in {} (declarer {}, {} leads, then {:?})",
                    seat_name(claimer),
                    tricks,
                    deal,
                    suit_name(trump),
                    seat_name(declarer),
                    seat_name(leader),
                    plays
                );
            }
        }
    }

    /// A small deterministic generator, so failures reproduce.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self, n: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) % n as u64) as usize
        }
    }

    fn random_ending(rng: &mut Lcg, size: usize) -> Hands {
        let mut deck: Vec<usize> = (0..TOTAL_CARDS).collect();
        for i in (1..deck.len()).rev() {
            deck.swap(i, rng.next(i + 1));
        }
        let mut hands = Hands::new();
        for (i, &c) in deck.iter().take(size * NUM_SEATS).enumerate() {
            hands[i % NUM_SEATS].add(c);
        }
        hands
    }

    /// Like [`random_ending`], but dealt from only the suits in `suits`,
    /// so voids, ruffs and overruffs are the rule rather than the exception.
    fn short_suited_ending(rng: &mut Lcg, size: usize, suits: &[Suit]) -> Hands {
        let mut deck: Vec<usize> = suits
            .iter()
            .flat_map(|&suit| (0..NUM_RANKS).map(move |r| suit * NUM_RANKS + r))
            .collect();
        for i in (1..deck.len()).rev() {
            deck.swap(i, rng.next(i + 1));
        }
        let mut hands = Hands::new();
        for (i, &c) in deck.iter().take(size * NUM_SEATS).enumerate() {
            hands[i % NUM_SEATS].add(c);
        }
        hands
    }

    #[test]
    fn bounds_hold_where_ruffs_decide() {
        // The cashing and opponents-on-lead bounds reason about who can
        // ruff and who can overruff. Random endings from four suits rarely
        // test that, so these are dealt from two or three suits, one of them
        // trumps, with a defender on lead against declarer's claim.
        let mut rng = Lcg(0x7255);
        for _ in 0..300 {
            let size = 3 + rng.next(3);
            let suits: Vec<Suit> = if rng.next(2) == 0 {
                vec![SPADE, HEART]
            } else {
                vec![SPADE, HEART, DIAMOND]
            };
            let deal = short_suited_ending(&mut rng, size, &suits);
            let trump = suits[rng.next(suits.len())];
            let declarer = rng.next(NUM_SEATS);
            let leader = (declarer + 1 + 2 * rng.next(2)) % NUM_SEATS;
            let pos = ClaimPosition::from_play(&deal, trump, declarer, declarer, leader, &[])
                .expect("valid");
            for tricks in 1..=size {
                let fast = verdict(&pos, tricks);
                let mut reference = Search::new(trump, pos.roles, u64::MAX);
                reference.shortcuts = false;
                let slow = match reference.wins(&pos.state, tricks) {
                    Ok(true) => ClaimVerdict::Valid,
                    Ok(false) => ClaimVerdict::Invalid,
                    Err(OutOfBudget) => ClaimVerdict::Undecided,
                };
                assert_eq!(
                    fast,
                    slow,
                    "{} claims {tricks} of {deal:?} in {} ({} leads)",
                    seat_name(declarer),
                    suit_name(trump),
                    seat_name(leader)
                );
            }
        }
    }

    #[test]
    fn a_claim_never_exceeds_double_dummy() {
        // The claiming side cannot guarantee more than it makes against the
        // actual layout, with every card seen and partner cooperating. Nor
        // more than it makes with the two pooled hands swapped, since that
        // layout is just as consistent with what the claimer sees.
        let mut rng = Lcg(0x5eed);
        let mut strictly_less = 0;
        for _ in 0..300 {
            let size = 2 + rng.next(4);
            let mut deal = random_ending(&mut rng, size);
            let trump = rng.next(NUM_SUITS + 1);
            let declarer = rng.next(NUM_SEATS);
            let claimer = rng.next(NUM_SEATS);
            let leader = rng.next(NUM_SEATS);

            let pos = ClaimPosition::from_play(&deal, trump, declarer, claimer, leader, &[])
                .expect("valid");
            let limit = pos.max_claim(DEFAULT_CLAIM_BUDGET);
            assert!(!limit.undecided);

            let claimers_dd = |d: Hands| {
                let ns = Solver::new(d, trump, leader).solve() as usize;
                if is_ns(claimer) {
                    ns
                } else {
                    size - ns
                }
            };
            let actual = claimers_dd(deal);
            let [a, b] = Roles::new(declarer, claimer).hidden;
            let (ha, hb) = (deal[a], deal[b]);
            deal[a] = hb;
            deal[b] = ha;
            let swapped = claimers_dd(deal);

            assert!(
                limit.guaranteed <= actual.min(swapped),
                "claim {} > dd {}/{}",
                limit.guaranteed,
                actual,
                swapped
            );
            if limit.guaranteed < actual.min(swapped) {
                strictly_less += 1;
            }
        }
        // The pooled model has to bite somewhere, or it is not modelling
        // hidden cards at all.
        assert!(strictly_less > 0);
    }
}
