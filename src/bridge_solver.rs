//! Main solver implementation
//!
//! Uses alpha-beta search with MTD(f) driver

use super::cards::*;
use super::hands::Hands;
use super::search;
use super::types::*;

/// A card played to the current trick, with the seat that played it
#[derive(Clone, Copy, Debug)]
pub struct PlayedCard {
    /// The card played (0-51)
    pub card: usize,
    /// The seat that played the card
    pub seat: Seat,
}

impl PlayedCard {
    /// Create a new played card
    pub fn new(card: usize, seat: Seat) -> Self {
        PlayedCard { card, seat }
    }
}

/// Represents a partially played trick (1-3 cards already played)
#[derive(Clone, Debug, Default)]
pub struct PartialTrick {
    /// Cards played so far in this trick, in play order
    pub plays: Vec<PlayedCard>,
}

impl PartialTrick {
    /// Create a new partial trick with no cards played
    pub fn new() -> Self {
        PartialTrick { plays: Vec::new() }
    }

    /// Add a card to the partial trick
    pub fn add(&mut self, card: usize, seat: Seat) -> &mut Self {
        self.plays.push(PlayedCard::new(card, seat));
        self
    }

    /// Get the number of cards played
    pub fn len(&self) -> usize {
        self.plays.len()
    }

    /// Check if no cards have been played
    pub fn is_empty(&self) -> bool {
        self.plays.is_empty()
    }

    /// Get the lead suit (suit of the first card played)
    pub fn lead_suit(&self) -> Option<Suit> {
        self.plays.first().map(|p| suit_of(p.card))
    }

    /// Get the seat that led to this trick
    pub fn leader(&self) -> Option<Seat> {
        self.plays.first().map(|p| p.seat)
    }

    /// Get the next seat to play
    pub fn next_to_play(&self) -> Option<Seat> {
        self.plays.last().map(|p| next_seat(p.seat))
    }
}

/// Ordered cards container for move ordering
#[derive(Default)]
pub struct OrderedCards {
    cards: [u8; TOTAL_TRICKS],
    count: usize,
}

impl OrderedCards {
    #[inline]
    pub fn new() -> Self {
        OrderedCards {
            cards: [0; TOTAL_TRICKS],
            count: 0,
        }
    }

    #[inline]
    fn add(&mut self, card: usize) {
        self.cards[self.count] = card as u8;
        self.count += 1;
    }

    /// Add cards in natural order (high to low)
    #[inline]
    fn add_cards(&mut self, cards: Cards) {
        for card in cards.iter() {
            self.add(card);
        }
    }

    /// Add cards in reversed order (low to high)
    #[inline]
    fn add_reversed(&mut self, cards: Cards) {
        // Iterate in reverse by collecting to bottom first
        let mut remaining = cards;
        while !remaining.is_empty() {
            let card = remaining.bottom();
            self.add(card);
            remaining.remove(card);
        }
    }

    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.cards[..self.count].iter().map(|&c| c as usize)
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.count
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    #[inline]
    pub fn card(&self, i: usize) -> usize {
        self.cards[i] as usize
    }
}

/// Order lead cards (when starting a trick)
/// Priority: ruff_leads > good_leads > high_leads > normal_leads > void_leads > bad_leads > trump_leads
pub fn order_leads(
    playable: Cards,
    hands: &Hands,
    seat: Seat,
    trump: usize,
    all_cards: Cards,
) -> OrderedCards {
    let mut ordered = OrderedCards::new();
    let mut remaining = playable;

    let pd_hand = hands[partner(seat)];
    let lho_hand = hands[left_hand_opp(seat)];
    let rho_hand = hands[right_hand_opp(seat)];

    let mut good_leads = Cards::new();
    let mut high_leads = Cards::new();
    let mut normal_leads = Cards::new();
    let mut bad_leads = Cards::new();
    let mut trump_leads = Cards::new();
    let mut ruff_leads = Cards::new();
    let mut void_leads = Cards::new();

    let is_suit_contract = trump < NOTRUMP;

    for suit in 0..NUM_SUITS {
        let my_suit = playable.suit(suit);
        if my_suit.is_empty() {
            continue;
        }

        // Handle trump suit specially in suit contracts
        if is_suit_contract && suit == trump {
            trump_leads.add(my_suit.top());
            if my_suit.size() > 1 {
                trump_leads.add(my_suit.bottom());
            }
            continue;
        }

        // Skip suits where opponents can ruff
        if is_suit_contract {
            if lho_hand.suit(trump).size() > 0 && lho_hand.suit(suit).is_empty() {
                continue;
            }
            if rho_hand.suit(trump).size() > 0 && rho_hand.suit(suit).is_empty() {
                continue;
            }
        }

        let pd_suit = pd_hand.suit(suit);
        let lho_suit = lho_hand.suit(suit);
        let rho_suit = rho_hand.suit(suit);
        let all_suit = all_cards.suit(suit);

        // Get relative ranks (A, K, Q, J, T) in this suit
        let a = if !all_suit.is_empty() {
            all_suit.top()
        } else {
            continue;
        };
        let all_minus_a = all_suit.different(Cards::from_bits(1u64 << a));
        let k = if !all_minus_a.is_empty() {
            all_minus_a.top()
        } else {
            a
        };
        let all_minus_ak = all_minus_a.different(Cards::from_bits(1u64 << k));
        let q = if !all_minus_ak.is_empty() {
            all_minus_ak.top()
        } else {
            k
        };
        let all_minus_akq = all_minus_ak.different(Cards::from_bits(1u64 << q));
        let j = if !all_minus_akq.is_empty() {
            all_minus_akq.top()
        } else {
            q
        };
        let all_minus_akqj = all_minus_akq.different(Cards::from_bits(1u64 << j));
        let t = if !all_minus_akqj.is_empty() {
            all_minus_akqj.top()
        } else {
            j
        };

        // This suit's cards, mine as playable and partner's. Upstream 14ab4c7
        // switched the bad-lead and high-lead tests below to it from both
        // hands' every card.
        let our_suits = my_suit.union(pd_suit);

        // Check for good leads (finesse positions)
        // Partner has K and LHO has A, etc.
        //
        // Reached only with at least five cards of the suit out (my one or
        // more, two or more each for partner and LHO), so A down to T all
        // exist and the fallbacks above never apply here.
        if pd_suit.size() >= 2 && lho_suit.size() >= 2 {
            // Until upstream 0cf82a2 the reference read
            // `our_suits.Have(Cards().Add(q).Add(j))`, which C++ turned into
            // `Have(1)` through `Cards`' implicit `operator bool`: card index 1,
            // never in both partnerships' cards at once, so the q/j and j/t
            // tests never fired and the port copied that for lock-step. They
            // test what they say now.
            let card = |c: usize| Cards::from_bits(1u64 << c);
            if (pd_suit.have(k) && lho_suit.have(a))
                || (pd_suit.have(a)
                    && lho_suit.have(k)
                    && (pd_suit.have(q) || our_suits.include(card(q).union(card(j)))))
                || (pd_suit.have(k)
                    && lho_suit.have(q)
                    && (pd_suit.have(j) || our_suits.include(card(j).union(card(t)))))
            {
                good_leads.add(my_suit.top());
                if my_suit.size() > 1 {
                    good_leads.add(my_suit.bottom());
                }
                continue;
            }
        }

        // Check for bad leads (high card in front of RHO's higher card): a free
        // finesse for RHO. Upstream b83aa4b: in a suit contract only the top
        // card is a bad lead (the bottom joins the leftovers at the end), and at
        // no-trump the suit is no longer skipped but classified below.
        if my_suit.size() >= 2
            && rho_suit.size() >= 2
            && ((my_suit.have(a) && rho_suit.have(k))
                || (my_suit.have(k) && rho_suit.have(a) && !our_suits.have(q)))
            && is_suit_contract
        {
            bad_leads.add(my_suit.top());
            continue;
        }

        // Check for high leads (both sides have A/K/Q)
        let mut akq = Cards::new();
        akq.add(a);
        akq.add(k);
        akq.add(q);
        if !lho_suit.is_empty() && !rho_suit.is_empty() && our_suits.intersect(akq).size() >= 2 {
            high_leads.add(my_suit.top());
            if my_suit.size() > 1 {
                high_leads.add(my_suit.bottom());
            }
            continue;
        }

        // Check for ruff leads (partner can ruff)
        if is_suit_contract
            && pd_suit.is_empty()
            && !lho_suit.is_empty()
            && !rho_suit.is_empty()
            && pd_hand.suit(trump).size() > 0
            && pd_hand.suit(trump).size() <= playable.suit(trump).size()
            && my_suit.bottom() != a
        {
            ruff_leads.add(my_suit.bottom());
            continue;
        }

        // A suit some other player is void in comes after the normal leads
        // (upstream de5ee81).
        if pd_suit.is_empty() || lho_suit.is_empty() || rho_suit.is_empty() {
            void_leads.add(my_suit.top());
            void_leads.add(my_suit.bottom());
            continue;
        }

        // Normal leads (top and bottom)
        normal_leads.add(my_suit.top());
        if my_suit.size() > 1 {
            normal_leads.add(my_suit.bottom());
        }
    }

    // Add in priority order
    if is_suit_contract {
        ordered.add_cards(ruff_leads);
        remaining.remove_cards(ruff_leads);
    }
    ordered.add_cards(good_leads);
    remaining.remove_cards(good_leads);
    ordered.add_cards(high_leads);
    remaining.remove_cards(high_leads);
    ordered.add_cards(normal_leads);
    remaining.remove_cards(normal_leads);
    ordered.add_cards(void_leads);
    remaining.remove_cards(void_leads);
    if is_suit_contract {
        ordered.add_cards(bad_leads);
        remaining.remove_cards(bad_leads);
        ordered.add_cards(trump_leads);
        remaining.remove_cards(trump_leads);
    }
    // Add any remaining cards
    ordered.add_cards(remaining);

    ordered
}

/// Order follow cards (when following suit or discarding)
/// Matches the C++ OrderCards logic for better move ordering
#[allow(clippy::too_many_arguments)]
pub fn order_follows(
    playable: Cards,
    hands: &Hands,
    seat: Seat,
    trump: usize,
    lead_suit: Suit,
    winning_seat: Seat,
    winning_card: usize,
    card_in_trick: usize,
    wins_over: impl Fn(usize, usize) -> bool,
) -> OrderedCards {
    let mut ordered = OrderedCards::new();

    let pd_suit = hands[partner(seat)].suit(lead_suit);
    let lho_suit = hands[left_hand_opp(seat)].suit(lead_suit);

    let trick_ending = card_in_trick == 3;
    let second_seat = card_in_trick == 1;

    // Helper to check if card1 is higher rank than card2 (lower index = higher rank)
    let higher_rank = |c1: usize, c2: usize| c1 < c2;

    // Following suit?
    let my_suit = playable.suit(lead_suit);
    if !my_suit.is_empty() {
        // Can't beat current winner - play low first
        if !wins_over(my_suit.top(), winning_card) {
            ordered.add_reversed(playable);
            return ordered;
        }

        // Partner is winning - check if we should play low
        if winning_seat == partner(seat) {
            // Play low if:
            // - Trick is ending (partner wins)
            // - LHO has no cards in suit
            // - Partner's winning card beats LHO's best
            // - LHO's options above partner's card equals LHO's options above our best
            //   (meaning we can't improve the situation by playing high)
            if trick_ending
                || lho_suit.is_empty()
                || higher_rank(winning_card, lho_suit.top())
                || lho_suit.slice(0, winning_card) == lho_suit.slice(0, my_suit.top())
            {
                ordered.add_reversed(playable);
                return ordered;
            }
        }

        // Second seat analysis - should we duck for partner?
        if second_seat && !pd_suit.is_empty() && higher_rank(pd_suit.top(), winning_card) {
            let combined = pd_suit.union(my_suit);
            // If LHO has a higher card than our combined best, and their options
            // above partner's card equals options above our best, play low
            if !lho_suit.is_empty()
                && higher_rank(lho_suit.top(), combined.top())
                && lho_suit.slice(0, pd_suit.top()) == lho_suit.slice(0, my_suit.top())
            {
                ordered.add_reversed(playable);
                return ordered;
            }
            // If LHO can't beat partner, play low
            if lho_suit.is_empty() || higher_rank(pd_suit.top(), lho_suit.top()) {
                ordered.add_reversed(playable);
                return ordered;
            }
        }

        // Split cards into those that beat the winner and those that don't
        let higher_cards = my_suit.slice(0, winning_card);
        let lower_cards = my_suit.different(higher_cards);

        // Order higher cards based on whether we need to beat LHO
        if trick_ending || lho_suit.is_empty() || higher_rank(higher_cards.bottom(), lho_suit.top())
        {
            // We can safely play low among our winning cards
            ordered.add_reversed(higher_cards);
        } else {
            // Try high cards first (might need to beat LHO)
            ordered.add_cards(higher_cards);
        }
        // Add lower cards (low first)
        ordered.add_reversed(lower_cards);
        return ordered;
    }

    // Not following suit - ruff or discard
    let is_suit_contract = trump < NOTRUMP;
    let my_trumps = if is_suit_contract {
        playable.suit(trump)
    } else {
        Cards::new()
    };

    if !my_trumps.is_empty() {
        // Can ruff
        let lho_has_trumps = !hands[left_hand_opp(seat)].suit(trump).is_empty();

        // Check if partner is winning and can hold the trick
        let partner_winning = winning_seat == partner(seat);
        if partner_winning
            && (trick_ending || (!lho_suit.is_empty() && wins_over(winning_card, lho_suit.top())))
        {
            // Partner can win - don't ruff, discard instead
        } else if suit_of(winning_card) == trump {
            // Someone already trumped - try to overruff if possible
            if winning_seat != partner(seat) && wins_over(my_trumps.top(), winning_card) {
                // We can overruff - try higher trumps first
                let higher_trumps = my_trumps.slice(my_trumps.top(), winning_card);
                ordered.add_reversed(higher_trumps);
                // Then add the rest of playable cards
                let remaining = playable.different(higher_trumps);
                add_discards(&mut ordered, remaining, trump);
                return ordered;
            }
        } else if trick_ending || !lho_suit.is_empty() || !lho_has_trumps {
            // The lowest trump is guaranteed to win
            ordered.add(my_trumps.bottom());
            let remaining = playable.different(Cards::from_bits(1u64 << my_trumps.bottom()));
            add_discards(&mut ordered, remaining, trump);
            return ordered;
        } else {
            // LHO might overruff - try trumps high to low
            ordered.add_reversed(my_trumps);
            let remaining = playable.different(my_trumps);
            add_discards(&mut ordered, remaining, trump);
            return ordered;
        }
    }

    // Discard - try bottom card from each suit first
    add_discards(&mut ordered, playable, trump);
    ordered
}

/// Add discards matching C++ logic:
/// 1. For each non-trump suit, add the bottom (lowest) card
/// 2. Sort those discards by suit length (longer suits first)
/// 3. Add remaining cards
fn add_discards(ordered: &mut OrderedCards, mut playable: Cards, trump: usize) {
    // Collect bottom card from each non-trump suit, tracking suit lengths
    let mut discards: [(usize, usize); 4] = [(0, 0); 4]; // (card, suit_length)
    let mut num_discards = 0;

    for suit in 0..4 {
        if suit == trump {
            continue;
        }
        let suit_cards = playable.suit(suit);
        if !suit_cards.is_empty() {
            let bottom = suit_cards.bottom();
            // Count how many cards remain in this suit after removing bottom
            let remaining_in_suit = playable.suit(suit).size();
            discards[num_discards] = (bottom, remaining_in_suit);
            num_discards += 1;
            playable.remove(bottom);
        }
    }

    // Sort discards by suit length, longer suits first and ties in suit order:
    // upstream 559908e's three compare-and-swaps in place of a sort call.
    let mut compare_swap = |a: usize, b: usize| {
        if discards[a].1 < discards[b].1 {
            discards.swap(a, b);
        }
    };
    if num_discards >= 2 {
        compare_swap(0, 1);
        if num_discards >= 3 {
            compare_swap(1, 2);
            compare_swap(0, 1);
        }
    }

    // Add sorted discards
    for discard in discards.iter().take(num_discards) {
        ordered.add(discard.0);
    }

    // Add remaining cards
    ordered.add_cards(playable);
}

/// Double-dummy solver
pub struct Solver {
    hands: Hands,
    trump: usize,
    initial_leader: Seat,
    num_tricks: usize,
}

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

thread_local! {
    /// Nodes visited by the last solve *on this thread*.
    ///
    /// Per-thread rather than global so that concurrent solves — several
    /// threads each solving their own deal, which is how `dealer3` drives the
    /// library — report their own work instead of overwriting one another's.
    static LAST_SOLVE_NODES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

pub(crate) static XRAY_COUNT: AtomicUsize = AtomicUsize::new(0);
pub(crate) static XRAY_LIMIT: AtomicUsize = AtomicUsize::new(0);
/// Cache-digest window: emit every STEP iterations from START to END.
/// STEP 0 disables it; END 0 means no upper bound.
pub(crate) static XRAY_CACHE_START: AtomicUsize = AtomicUsize::new(0);
pub(crate) static XRAY_CACHE_END: AtomicUsize = AtomicUsize::new(0);
pub(crate) static XRAY_CACHE_STEP: AtomicUsize = AtomicUsize::new(0);
pub(crate) static NO_PRUNING: AtomicBool = AtomicBool::new(false);
pub(crate) static NO_TT: AtomicBool = AtomicBool::new(false);
pub(crate) static NO_RANK_SKIP: AtomicBool = AtomicBool::new(false);
pub(crate) static SHOW_PERF: AtomicBool = AtomicBool::new(false);

/// Get the node count from the last solve on this thread (for profiling)
pub fn get_node_count() -> u64 {
    LAST_SOLVE_NODES.with(|n| n.get())
}

/// Set xray tracing limit (0 = disabled)
pub fn set_xray_limit(limit: usize) {
    XRAY_LIMIT.store(limit, Ordering::Relaxed);
    XRAY_COUNT.store(0, Ordering::Relaxed);
}

/// Check if xray logging should occur (enabled and under limit)
pub(crate) fn xray_should_log() -> bool {
    let limit = XRAY_LIMIT.load(Ordering::Relaxed);
    limit > 0 && XRAY_COUNT.load(Ordering::Relaxed) <= limit
}

/// Emit a cache content digest every `step` iterations from `start` to `end`.
///
/// The trace records what the search *did*; this records what its caches
/// *hold*. Those diverge at different moments: two searches can take the same
/// decisions for a long time while their caches drift apart through different
/// eviction, resizing or pattern-tree shape, and the first visible symptom is
/// then a lookup that hits on one side and misses on the other -- far too late
/// to say why.
///
/// The window exists so the drift can be bisected. A digest is O(table size),
/// so emitting one per node is far too slow to run to completion; instead sweep
/// coarsely, find the interval where the two solvers stop agreeing, and narrow
/// into it. `step` of 0 disables, `end` of 0 means no upper bound, and the
/// first emission lands exactly on `start`.
pub fn set_xray_cache_window(start: usize, end: usize, step: usize) {
    XRAY_CACHE_START.store(start, Ordering::Relaxed);
    XRAY_CACHE_END.store(end, Ordering::Relaxed);
    XRAY_CACHE_STEP.store(step, Ordering::Relaxed);
}

/// Set no-pruning mode (disables fast/slow tricks pruning for debugging)
pub fn set_no_pruning(enabled: bool) {
    NO_PRUNING.store(enabled, Ordering::Relaxed);
}

/// Set no-TT mode (disables transposition table for debugging)
pub fn set_no_tt(enabled: bool) {
    NO_TT.store(enabled, Ordering::Relaxed);
}

/// Set no-rank-skip mode (disables min_relevant_ranks optimization for debugging)
pub fn set_no_rank_skip(enabled: bool) {
    NO_RANK_SKIP.store(enabled, Ordering::Relaxed);
}

/// Set show-perf mode (outputs [PERF] lines to stderr after each solve)
pub fn set_show_perf(enabled: bool) {
    SHOW_PERF.store(enabled, Ordering::Relaxed);
}

impl Solver {
    /// Create a new solver
    pub fn new(hands: Hands, trump: usize, initial_leader: Seat) -> Self {
        let num_tricks = hands.num_tricks();
        Solver {
            hands,
            trump,
            initial_leader,
            num_tricks,
        }
    }

    /// Create a solver for a mid-trick position
    ///
    /// Use this when solving from a position where a trick is partially played.
    /// The hands should contain the cards NOT YET played (excluding cards in partial_trick).
    ///
    /// # Arguments
    /// * `hands` - The remaining cards in each hand (excluding cards in partial_trick)
    /// * `trump` - Trump suit (0-3) or NOTRUMP (4)
    /// * `partial_trick` - Cards already played to the current trick
    ///
    /// # Returns
    /// The solver, or None if the partial trick is invalid
    pub fn new_mid_trick(hands: Hands, trump: usize, partial_trick: &PartialTrick) -> Option<Self> {
        if partial_trick.is_empty() || partial_trick.len() > 3 {
            return None;
        }

        // The leader is the first player in the partial trick
        let initial_leader = partial_trick.leader()?;

        // num_tricks is based on the largest hand size
        // Since we're mid-trick, hands have different sizes
        // Max hand size = hands that haven't played yet = total tricks remaining
        let max_hand_size = (0..NUM_SEATS).map(|s| hands[s].size()).max().unwrap_or(0);
        let num_tricks = max_hand_size;

        Some(Solver {
            hands,
            trump,
            initial_leader,
            num_tricks,
        })
    }

    /// Solve and return NS tricks
    pub fn solve(&self) -> u8 {
        let mut cutoff_cache = search::CutoffCache::new(16);
        let mut pattern_cache = crate::PatternCache::new(crate::PatternCache::DEFAULT_BITS);
        self.solve_with_caches(&mut cutoff_cache, &mut pattern_cache)
    }

    /// Solve with external caches (allows cache sharing across multiple solves)
    pub fn solve_with_caches(
        &self,
        cutoff_cache: &mut search::CutoffCache,
        pattern_cache: &mut super::pattern::PatternCache,
    ) -> u8 {
        self.solve_with_caches_and_partial(cutoff_cache, pattern_cache, None, None)
    }

    /// Solve with external caches, seeding the MTD(f) search from `guess`.
    ///
    /// A caller filling several cells of a double-dummy table can pass the
    /// previous cell's answer, which is usually close to this one's, so the
    /// search converges in fewer iterations. The seed cannot affect the
    /// result: MTD(f) converges to the same value from any starting point, so
    /// a poor guess costs iterations and nothing else.
    ///
    /// Pass `Solver::seed_from(previous_ns_tricks)` to build the guess.
    pub fn solve_with_caches_seeded(
        &self,
        cutoff_cache: &mut search::CutoffCache,
        pattern_cache: &mut super::pattern::PatternCache,
        guess: usize,
    ) -> u8 {
        self.solve_with_caches_and_partial(cutoff_cache, pattern_cache, None, Some(guess))
    }

    /// The MTD(f) seed to use for the next cell, given this cell's NS tricks.
    pub fn seed_from(ns_tricks: u8) -> usize {
        (ns_tricks as usize + 1).min(TOTAL_TRICKS)
    }

    /// Solve from a mid-trick position with external caches
    ///
    /// Use this to evaluate positions where some cards have already been played
    /// to the current trick.
    ///
    /// # Arguments
    /// * `cutoff_cache` - Cutoff cache for move ordering
    /// * `pattern_cache` - Pattern cache for transposition table
    /// * `partial_trick` - Cards already played to the current trick
    ///
    /// # Returns
    /// The number of tricks NS will take with optimal play from this position
    pub fn solve_mid_trick(
        &self,
        cutoff_cache: &mut search::CutoffCache,
        pattern_cache: &mut super::pattern::PatternCache,
        partial_trick: &PartialTrick,
    ) -> u8 {
        self.solve_with_caches_and_partial(cutoff_cache, pattern_cache, Some(partial_trick), None)
    }

    /// Internal solve implementation that handles both normal and mid-trick positions
    fn solve_with_caches_and_partial(
        &self,
        cutoff_cache: &mut search::CutoffCache,
        pattern_cache: &mut super::pattern::PatternCache,
        partial_trick: Option<&PartialTrick>,
        seed: Option<usize>,
    ) -> u8 {
        XRAY_COUNT.store(0, Ordering::Relaxed);

        // The clock is only wanted for the optional [PERF] line below, so read
        // it only when that is switched on. Two reasons this matters:
        //
        //  * On wasm32 `Instant::now()` has no implementation and traps, which
        //    would make every solve in a browser abort. Timing and eprintln are
        //    both meaningless there anyway, so the whole diagnostic is compiled
        //    out for that target.
        //  * Even natively this saved a clock read on every single solve, and
        //    solves are the hot path.
        #[cfg(not(target_arch = "wasm32"))]
        let start = SHOW_PERF
            .load(Ordering::Relaxed)
            .then(std::time::Instant::now);

        let num_tricks = self.num_tricks;
        let guess = seed.unwrap_or_else(|| self.guess_tricks());
        let (result, nodes) = self.mtdf_search_with_caches_and_partial(
            num_tricks,
            guess,
            cutoff_cache,
            pattern_cache,
            partial_trick,
        );
        LAST_SOLVE_NODES.with(|n| n.set(nodes));

        #[cfg(not(target_arch = "wasm32"))]
        if let Some(start) = start {
            let elapsed = start.elapsed();
            let iterations = nodes;
            let ns_per_iter = if iterations > 0 {
                elapsed.as_nanos() as f64 / iterations as f64
            } else {
                0.0
            };
            eprintln!(
                "[PERF] iterations={}, time={:.3}s, ns/iter={:.1}",
                iterations,
                elapsed.as_secs_f64(),
                ns_per_iter
            );
        }

        result
    }

    /// MTD(f) search driver that handles mid-trick positions
    fn mtdf_search_with_caches_and_partial(
        &self,
        num_tricks: usize,
        guess: usize,
        cutoff_cache: &mut search::CutoffCache,
        pattern_cache: &mut super::pattern::PatternCache,
        partial_trick: Option<&PartialTrick>,
    ) -> (u8, u64) {
        let mut hands = self.hands;

        // Each MTD(f) iteration builds a fresh `Search`, so the per-search node
        // counts are summed here to give the whole solve's total.
        let mut nodes = 0u64;
        let mut lower = 0i8;
        let mut upper = num_tricks as i8;
        let mut ns_tricks = guess as i8;

        while lower < upper {
            let beta = if ns_tricks == lower {
                ns_tricks + 1
            } else {
                ns_tricks
            };

            let mut searcher = search::Search::new_with_partial_trick(
                &mut hands,
                self.trump,
                self.initial_leader,
                cutoff_cache,
                pattern_cache,
                partial_trick,
            );
            searcher.set_nodes_base(nodes);
            ns_tricks = searcher.search(beta) as i8;
            nodes += searcher.nodes();

            if ns_tricks < beta {
                upper = ns_tricks;
            } else {
                lower = ns_tricks;
            }
        }

        (lower as u8, nodes)
    }

    /// Estimate starting tricks for MTD(f)
    /// Ported from C++ GuessTricks() for consistent behavior
    fn guess_tricks(&self) -> usize {
        let num_tricks = self.num_tricks;
        let ns_points = self.hands[NORTH].points() + self.hands[SOUTH].points();
        let ew_points = self.hands[EAST].points() + self.hands[WEST].points();

        if self.trump >= NOTRUMP {
            // NT contract
            if ns_points * 2 < ew_points {
                return 0;
            }
            if ns_points < ew_points {
                return num_tricks / 2 + 1;
            }
        } else {
            // Suit contract - compare points AND trump length
            let n_trumps = self.hands[NORTH].suit(self.trump).size();
            let s_trumps = self.hands[SOUTH].suit(self.trump).size();
            let e_trumps = self.hands[EAST].suit(self.trump).size();
            let w_trumps = self.hands[WEST].suit(self.trump).size();

            let ns_max_trumps = n_trumps.max(s_trumps);
            let ew_max_trumps = e_trumps.max(w_trumps);

            if ns_points < ew_points
                && (ns_max_trumps < ew_max_trumps
                    || (ns_max_trumps == ew_max_trumps
                        && n_trumps + s_trumps < e_trumps + w_trumps))
            {
                return 0;
            }
        }

        num_tricks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: All solver tests are marked #[ignore] because they run the DDS solver
    // which takes ~1 sec per test. Run with `cargo test -- --ignored` when needed.

    #[test]
    #[ignore] // Slow: runs DDS solver
    fn test_solver_1_trick() {
        // Single trick - NS has ace, EW has king
        // N: SA  E: SK  S: S2  W: S3
        let hands = Hands::from_pbn("N:A... K... 2... 3...").unwrap();

        // West leads - EW has the lead but NS has the ace
        let solver = Solver::new(hands, NOTRUMP, WEST);
        let ns_tricks = solver.solve();
        assert_eq!(ns_tricks, 1); // NS wins with the ace
    }

    #[test]
    #[ignore] // Slow: runs DDS solver
    fn test_solver_1_trick_ew_wins() {
        // Single trick - EW has ace
        // N: SK  E: SA  S: S2  W: S3
        let hands = Hands::from_pbn("N:K... A... 2... 3...").unwrap();

        // West leads
        let solver = Solver::new(hands, NOTRUMP, WEST);
        let ns_tricks = solver.solve();
        assert_eq!(ns_tricks, 0); // EW wins with the ace
    }

    #[test]
    #[ignore] // Slow: runs DDS solver
    fn test_solver_2_tricks() {
        // Two tricks - NS has both aces
        // N: SA,HA  E: SK,HK  S: S2,H2  W: S3,H3
        let hands = Hands::from_pbn("N:A.A.. K.K.. 2.2.. 3.3..").unwrap();

        // West leads
        let solver = Solver::new(hands, NOTRUMP, WEST);
        let ns_tricks = solver.solve();
        assert_eq!(ns_tricks, 2); // NS wins both tricks
    }

    #[test]
    #[ignore] // Slow: runs DDS solver
    fn test_solver_4_tricks() {
        // Four tricks - NS has all aces
        let hands = Hands::from_pbn("N:A.A.A.A K.K.K.K 2.2.2.2 3.3.3.3").unwrap();

        // West leads
        let solver = Solver::new(hands, NOTRUMP, WEST);
        let ns_tricks = solver.solve();
        assert_eq!(ns_tricks, 4); // NS wins all 4 tricks
    }

    #[test]
    #[ignore] // Slow: runs DDS solver
    fn test_solver_8_tricks() {
        // 8 tricks - NS has AK in each suit
        let hands = Hands::from_pbn("N:AK.AK.AK.AK QJ.QJ.QJ.QJ 32.32.32.32 T9.T9.T9.T9").unwrap();

        // West leads
        let start = std::time::Instant::now();
        let solver = Solver::new(hands, NOTRUMP, WEST);
        let ns_tricks = solver.solve();
        eprintln!("8-trick test took {:?}", start.elapsed());
        assert_eq!(ns_tricks, 8); // NS wins 8 tricks
    }

    #[test]
    #[ignore] // Slow: runs DDS solver
    fn test_solver_cold_13() {
        // NS has all top cards
        let hands = Hands::from_pbn(
            "N:AKQJ.AKQ.AKQ.AKQ T987.JT9.JT9.JT9 6543.876.876.876 2.5432.5432.5432",
        )
        .unwrap();

        eprintln!("Hands parsed, starting solve...");
        let solver = Solver::new(hands, NOTRUMP, WEST);
        let start = std::time::Instant::now();
        let ns_tricks = solver.solve();
        eprintln!("Solve took {:?}", start.elapsed());
        assert_eq!(ns_tricks, 13);
    }

    #[test]
    #[ignore] // Slow: runs DDS solver
    fn test_solver_cold_0() {
        // EW has all top cards
        let hands = Hands::from_pbn(
            "N:T987.JT9.JT9.JT9 AKQJ.AKQ.AKQ.AKQ 2.5432.5432.5432 6543.876.876.876",
        )
        .unwrap();

        let solver = Solver::new(hands, NOTRUMP, WEST);
        let ns_tricks = solver.solve();
        assert_eq!(ns_tricks, 0);
    }

    #[test]
    #[ignore] // Slow: runs DDS solver
    fn test_solver_9_tricks() {
        // From test case
        let hands = Hands::from_pbn(
            "N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72",
        )
        .unwrap();

        let start = std::time::Instant::now();
        let solver = Solver::new(hands, NOTRUMP, WEST);
        let ns_tricks = solver.solve();
        eprintln!("9-trick test took {:?}", start.elapsed());
        assert_eq!(ns_tricks, 9);
    }

    #[test]
    #[ignore] // Slow: runs DDS solver
    fn test_solver_13card_north_only() {
        // Same 13-card deal, but only test North leading
        let hands = Hands::from_pbn(
            "N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72",
        )
        .unwrap();

        let start = std::time::Instant::now();
        let solver = Solver::new(hands, NOTRUMP, NORTH);
        let ns_tricks = solver.solve();
        let nodes = get_node_count();
        eprintln!(
            "13-card North lead test: {} tricks, {:?}, {} nodes",
            ns_tricks,
            start.elapsed(),
            nodes
        );
        // Note: Expected value needs verification with C++ solver
    }

    // Mid-trick solving tests

    #[test]
    #[ignore] // Slow: runs DDS solver
    fn test_mid_trick_1_card_played() {
        // 2-trick position: W:S3,H3 N:SA,HA E:SK,HK S:S2,H2
        // West leads S3, then we solve from North's perspective
        // Remaining hands after S3 played:
        // W: H3  N: SA,HA  E: SK,HK  S: S2,H2
        // PBN format: N:spades.hearts.diamonds.clubs then E, S, W (clockwise)
        let hands = Hands::from_pbn("N:A.A.. K.K.. 2.2.. .3..").unwrap();

        // Create partial trick with West's S3 already played
        let mut partial = PartialTrick::new();
        partial.add(card_of(SPADE, THREE), WEST);

        let solver = Solver::new_mid_trick(hands, NOTRUMP, &partial).unwrap();
        let mut cutoff_cache = search::CutoffCache::new(16);
        let mut pattern_cache = crate::PatternCache::new(crate::PatternCache::DEFAULT_BITS);

        let ns_tricks = solver.solve_mid_trick(&mut cutoff_cache, &mut pattern_cache, &partial);

        // North will play SA to win, then lead HA to win = 2 tricks for NS
        assert_eq!(ns_tricks, 2);
    }

    #[test]
    #[ignore] // Slow: runs DDS solver
    fn test_mid_trick_2_cards_played() {
        // 2-trick position: W:S3,H3 N:SA,HA E:SK,HK S:S2,H2
        // West leads S3, North plays SA
        // Remaining hands after S3 and SA played:
        // W: H3  N: HA  E: SK,HK  S: S2,H2
        let hands = Hands::from_pbn("N:.A.. K.K.. 2.2.. .3..").unwrap();

        let mut partial = PartialTrick::new();
        partial.add(card_of(SPADE, THREE), WEST);
        partial.add(card_of(SPADE, ACE), NORTH);

        let solver = Solver::new_mid_trick(hands, NOTRUMP, &partial).unwrap();
        let mut cutoff_cache = search::CutoffCache::new(16);
        let mut pattern_cache = crate::PatternCache::new(crate::PatternCache::DEFAULT_BITS);

        let ns_tricks = solver.solve_mid_trick(&mut cutoff_cache, &mut pattern_cache, &partial);

        // SA is winning. East plays SK, South plays S2.
        // NS wins this trick (SA > SK). North leads HA to win = 2 tricks for NS
        assert_eq!(ns_tricks, 2);
    }

    #[test]
    #[ignore] // Slow: runs DDS solver
    fn test_mid_trick_3_cards_played() {
        // 2-trick position: W:S3,H3 N:SA,HA E:SK,HK S:S2,H2
        // West leads S3, North plays SA, East plays SK
        // Remaining hands after S3, SA, SK played:
        // W: H3  N: HA  E: HK  S: S2,H2
        let hands = Hands::from_pbn("N:.A.. .K.. 2.2.. .3..").unwrap();

        let mut partial = PartialTrick::new();
        partial.add(card_of(SPADE, THREE), WEST);
        partial.add(card_of(SPADE, ACE), NORTH);
        partial.add(card_of(SPADE, KING), EAST);

        let solver = Solver::new_mid_trick(hands, NOTRUMP, &partial).unwrap();
        let mut cutoff_cache = search::CutoffCache::new(16);
        let mut pattern_cache = crate::PatternCache::new(crate::PatternCache::DEFAULT_BITS);

        let ns_tricks = solver.solve_mid_trick(&mut cutoff_cache, &mut pattern_cache, &partial);

        // SA is winning over SK. South plays S2 to complete trick.
        // NS wins (SA). North leads HA, EW play. NS wins HA = 2 tricks
        assert_eq!(ns_tricks, 2);
    }

    #[test]
    #[ignore] // Slow: runs DDS solver
    fn test_mid_trick_trump_overruff() {
        // Trump contract (spades trump): W leads DA, N can ruff
        // W: DA,H3  N: SA,HA  E: DK,HK  S: S2,H2
        // After W leads DA:
        // W: H3  N: SA,HA  E: DK,HK  S: S2,H2
        let hands = Hands::from_pbn("N:A.A.. ..K.K 2.2.. .3..").unwrap();

        let mut partial = PartialTrick::new();
        partial.add(card_of(DIAMOND, ACE), WEST);

        let solver = Solver::new_mid_trick(hands, SPADE, &partial).unwrap();
        let mut cutoff_cache = search::CutoffCache::new(16);
        let mut pattern_cache = crate::PatternCache::new(crate::PatternCache::DEFAULT_BITS);

        let ns_tricks = solver.solve_mid_trick(&mut cutoff_cache, &mut pattern_cache, &partial);

        // North ruffs with SA (trumps DA), East plays DK, South plays S2
        // NS wins trick (SA trumped). Then HA wins = 2 tricks
        assert_eq!(ns_tricks, 2);
    }

    #[test]
    fn test_partial_trick_builder() {
        let mut partial = PartialTrick::new();
        assert!(partial.is_empty());
        assert_eq!(partial.len(), 0);

        partial.add(card_of(SPADE, THREE), WEST);
        assert!(!partial.is_empty());
        assert_eq!(partial.len(), 1);
        assert_eq!(partial.lead_suit(), Some(SPADE));
        assert_eq!(partial.leader(), Some(WEST));
        assert_eq!(partial.next_to_play(), Some(NORTH));

        partial.add(card_of(SPADE, ACE), NORTH);
        assert_eq!(partial.len(), 2);
        assert_eq!(partial.next_to_play(), Some(EAST));
    }

    #[test]
    fn test_new_mid_trick_validation() {
        let hands = Hands::from_pbn("N:A... K... 2... 3...").unwrap();

        // Empty partial trick should fail
        let empty = PartialTrick::new();
        assert!(Solver::new_mid_trick(hands, NOTRUMP, &empty).is_none());

        // 4 cards (complete trick) should fail
        let mut full = PartialTrick::new();
        full.add(card_of(SPADE, THREE), WEST);
        full.add(card_of(SPADE, ACE), NORTH);
        full.add(card_of(SPADE, KING), EAST);
        full.add(card_of(SPADE, TWO), SOUTH);
        assert!(Solver::new_mid_trick(hands, NOTRUMP, &full).is_none());

        // 1-3 cards should work
        let mut one = PartialTrick::new();
        one.add(card_of(SPADE, THREE), WEST);
        assert!(Solver::new_mid_trick(hands, NOTRUMP, &one).is_some());
    }
}
