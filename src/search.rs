//! Refactored search implementation matching C++ structure
//!
//! This module implements a 3-layer search structure matching the C++ reference:
//! - SearchWithCache: Trick boundary handling, TT lookups, XRAY logging
//! - SearchAtTrickStart: Fast/slow tricks pruning
//! - EvaluatePlayableCards: Main card evaluation loop with IsEquivalent

use super::cards::*;
use super::hands::Hands;
use super::pattern::{
    compute_pattern_hands, Bounds, PatHands, Pattern, PatternCache, RelativeHands, Shape,
};
use super::play::*;
use super::types::*;
use std::sync::atomic::Ordering;

// Re-export atomic counters from bridge_solver module
use super::bridge_solver::{
    xray_should_log, NO_PRUNING, NO_RANK_SKIP, NO_TT, XRAY_CACHE_END, XRAY_CACHE_START,
    XRAY_CACHE_STEP, XRAY_COUNT, XRAY_LIMIT,
};

/// Search result - NS tricks and rank winners (cards whose rank affected the outcome)
#[derive(Clone, Copy, Default)]
pub struct SearchResult {
    pub ns_tricks: u8,
    pub rank_winners: Cards,
}

/// Per-trick state, matching C++ `struct Trick`
#[derive(Clone, Copy, Default)]
pub struct Trick {
    /// All remaining cards at the start of this trick
    pub all_cards: Cards,
    /// The suit that was led
    pub lead_suit: usize,
    /// Shape of hands at this trick
    pub shape: Shape,
    /// Relative hands (cards normalized within each suit)
    pub relative_hands: RelativeHands,
}

/// Per-play state, matching C++ `class Play` member variables
#[derive(Clone, Copy, Default)]
pub struct PlayState {
    /// NS tricks won so far
    pub ns_tricks_won: u8,
    /// Current seat to play
    pub seat_to_play: Seat,
    /// Card played at this depth
    pub card_played: usize,
    /// Depth of the winning play in current trick
    pub winning_play: usize,
    /// The runs of equivalent cards (as rank positions) that this play's
    /// result relied on within its trick: Bob Salita's `equivalence_runs`.
    /// A trick start's runs decide how far its stored pattern extends.
    pub equivalence_runs: Cards,
}

/// The cut-off cache's tag. Upstream `448a1f5` narrowed it from 64 bits to
/// 32: a cut-off card is only a move-ordering hint, so a tag collision costs a
/// slightly worse card order, never a wrong result, and the entry shrinks from
/// 12 bytes to 8. [`hash_cutoff_index`] keeps the product's top 32 bits, the
/// ones that mix well, and the bucket index is the tag's own top bits.
type CutoffHash = u32;

/// Cutoff cache entry
#[derive(Clone, Copy, Default)]
struct CutoffEntry {
    hash: CutoffHash,
    card: [u8; 4],
}

impl CutoffEntry {
    /// A slot holding nothing: zero hash, and no card for any seat.
    const EMPTY: CutoffEntry = CutoffEntry {
        hash: 0,
        card: [255; 4],
    };
}

/// Cutoff cache with linear probing (matching C++ Cache behavior)
pub struct CutoffCache {
    entries: Box<[CutoffEntry]>,
    bits: usize,
    /// The size this cache was built at, and the floor [`Self::reset`] shrinks
    /// back to. A table that grew for one hard strain must not charge every
    /// easy strain after it for the space.
    base_bits: usize,
    load_count: usize,
}

impl CutoffCache {
    /// Live entries, and a digest of their contents. See
    /// `pattern::PatternCache::digest` for the rules the value obeys.
    pub fn digest(&self) -> (usize, u64) {
        let mut count = 0;
        let mut digest = 0u64;
        for entry in self.entries.iter() {
            if entry.hash == 0 {
                continue;
            }
            count += 1;
            let mut h = super::pattern::mix_for_digest(entry.hash as u64);
            for (i, card) in entry.card.iter().enumerate() {
                // "No card" is 255 here and TOTAL_CARDS in the C++ reference,
                // so canonicalise it or the digests could never agree.
                let c = if (*card as usize) < TOTAL_CARDS {
                    *card as u64
                } else {
                    64
                };
                h = super::pattern::mix_for_digest(
                    h ^ super::pattern::mix_for_digest((c << 8) | i as u64),
                );
            }
            digest ^= h;
        }
        (count, digest)
    }

    pub fn new(bits: usize) -> Self {
        let size = 1 << bits;
        CutoffCache {
            entries: vec![CutoffEntry::EMPTY; size].into_boxed_slice(),
            bits,
            base_bits: bits,
            load_count: 0,
        }
    }

    /// Empty the cache, ready for the next search, without giving up the
    /// allocation.
    ///
    /// The C++ reference's `cutoff_cache` is a process global that `Solve`
    /// only calls `Reset()` on, per trump: the entries go, the capacity
    /// stays. That is what lets `par::TableSolver` hold one cache for the life
    /// of a run instead of building five a deal.
    ///
    /// Keeping the capacity cannot change what the search finds. Entries are
    /// never evicted -- a full table doubles rather than replacing anything --
    /// and probing stops at the first empty slot, which insertion never skips
    /// over. So a lookup hits exactly when the key was stored, whatever the
    /// table's size, and the size only decides how often `resize` runs.
    ///
    /// **Capacity is kept only as far as the last search justified it.**
    /// Keeping it unconditionally, as the reference does, is a bad trade here
    /// and was measured to be one: over the 200-deal lock-step corpus 99% of
    /// strains never grow past the base size, but a handful that do leave the
    /// table at four times it, and every later strain then pays to clear four
    /// megabytes it will use one of. Sizing to the load just seen keeps the
    /// in-place clear for the overwhelmingly common case and re-grows for a
    /// run of genuinely hard deals, at one allocation per change of size.
    pub fn reset(&mut self) {
        let bits = self.bits_for(self.load_count);
        if bits == self.bits {
            self.entries.fill(CutoffEntry::EMPTY);
        } else {
            self.entries = vec![CutoffEntry::EMPTY; 1 << bits].into_boxed_slice();
            self.bits = bits;
        }
        self.load_count = 0;
    }

    /// The smallest table, never below the size this cache was built at, that
    /// holds `load` entries without tripping the resize in [`Self::store`].
    /// The threshold is written the way `store` writes it so the two cannot
    /// drift apart.
    fn bits_for(&self, load: usize) -> usize {
        let mut bits = self.base_bits;
        while load >= (1usize << bits) * 3 / 4 {
            bits += 1;
        }
        bits
    }

    #[inline]
    fn index(&self, hash: CutoffHash) -> usize {
        // C++ uses: hash >> (BitSize(hash) - bits), BitSize being the tag's width.
        (hash >> (CutoffHash::BITS as usize - self.bits)) as usize
    }

    #[inline]
    /// The card stored for `slot`, the seat's place in the trick (0 to 3).
    pub fn lookup(&self, hash: CutoffHash, slot: usize) -> Option<usize> {
        let base_index = self.index(hash);
        // The mask is derived from the length rather than stored beside it so
        // that LLVM can see `idx <= len - 1` and drop the bounds check on the
        // probe; from a separate field it cannot know the two agree.
        let mask = self.entries.len() - 1;
        // Linear probing to the first empty slot (upstream 581cefe dropped the
        // tracked probe distance). It always comes: entries are never removed
        // and the table is resized at three quarters full.
        for d in 0.. {
            let entry = &self.entries[(base_index + d) & mask];
            if entry.hash == hash {
                if entry.card[slot] != 255 {
                    return Some(entry.card[slot] as usize);
                } else {
                    return None;
                }
            }
            if entry.hash == 0 {
                break; // Empty slot, entry not found
            }
        }
        None
    }

    #[inline]
    pub fn store(&mut self, hash: CutoffHash, slot: usize, card: usize) {
        // Resize if needed (at 75% load)
        let size = self.entries.len();
        if self.load_count >= size * 3 / 4 {
            self.resize();
        }

        let base_index = self.index(hash);
        // See `lookup` for why the mask comes from the length.
        let mask = self.entries.len() - 1;
        // Linear probing to find or create entry
        for d in 0.. {
            let idx = (base_index + d) & mask;
            let entry = &mut self.entries[idx];
            if entry.hash == hash {
                // Found existing entry for this hash
                entry.card[slot] = card as u8;
                return;
            }
            if entry.hash == 0 {
                // Empty slot, create new entry
                self.load_count += 1;
                entry.hash = hash;
                entry.card = [255, 255, 255, 255];
                entry.card[slot] = card as u8;
                return;
            }
        }
    }

    fn resize(&mut self) {
        let old_entries = std::mem::take(&mut self.entries);
        let new_bits = self.bits + 1;
        let new_size = 1 << new_bits;
        self.entries = vec![CutoffEntry::EMPTY; new_size].into_boxed_slice();
        self.bits = new_bits;
        self.load_count = 0;

        // Re-insert all existing entries
        for entry in old_entries.iter() {
            if entry.hash != 0 {
                // Store each seat's card if it was set
                for slot in 0..4 {
                    if entry.card[slot] != 255 {
                        self.store(entry.hash, slot, entry.card[slot] as usize);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Whether the second-seat rule is applied: off only in the test that
    /// checks it against the search without it. Per thread, so that test
    /// cannot reach any other.
    pub(crate) static SECOND_SEAT_RULE: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

/// Whether [`Search::second_seat_sure_tricks`] is tried: always, outside tests.
#[inline(always)]
fn second_seat_rule_enabled() -> bool {
    #[cfg(test)]
    {
        SECOND_SEAT_RULE.with(|on| on.get())
    }
    #[cfg(not(test))]
    {
        true
    }
}

/// Every suit, as a fast-trick suit mask.
const ALL_SUITS: u8 = 0b1111;

/// C++ `SureTrickCutoff`: the North-South total if `sure_tricks` for the side
/// they belong to (`for_ns`) decide the search against `beta`, else `None`.
fn sure_trick_cutoff(
    ns_tricks_won: u8,
    remaining: usize,
    beta: i8,
    sure_tricks: usize,
    for_ns: bool,
) -> Option<u8> {
    let won = ns_tricks_won as isize;
    let (beta, tricks, remaining) = (beta as isize, sure_tricks as isize, remaining as isize);
    if for_ns {
        (won + tricks >= beta).then_some((won + tricks) as u8)
    } else {
        (won + remaining - tricks < beta).then_some((won + remaining - tricks) as u8)
    }
}

/// How far Bob Salita's trump-rank bound reaches: it is tried when the side
/// on lead is at most this many tricks short of a cut.
const MAX_RANK_TRICK_GAP: isize = 2;

/// How far Bob Salita's draw-trumps bound reaches, likewise.
const MAX_DRAW_TRUMP_GAP: isize = 3;

/// A suit's `n` highest cards: its `n` lowest set bits. Bob Salita's (and
/// upstream 31802f9's) `KeepTop`.
// `bits & bits.wrapping_neg()`: see `pattern::RelativeHands::convert_suit`.
#[allow(unknown_lints, clippy::manual_isolate_lowest_one)]
fn keep_top(suit: Cards, n: usize) -> Cards {
    if suit.size() <= n {
        return suit;
    }
    let mut bits = suit.value();
    let mut kept = 0u64;
    for _ in 0..n {
        let low = bits & bits.wrapping_neg();
        kept |= low;
        bits ^= low;
    }
    Cards::from_bits(kept)
}

/// Bob Salita's `TrumpKills`: the most trumps of `trumps` that higher trumps
/// of `opp_trumps` can beat, each opposing trump used once.
fn trump_kills(mut trumps: Cards, mut opp_trumps: Cards) -> usize {
    let mut kills = 0;
    while !trumps.is_empty() {
        let low = trumps.bottom();
        let mut higher = opp_trumps.slice(0, low);
        if higher.is_empty() {
            break;
        }
        kills += 1;
        trumps.remove(low);
        higher.remove(higher.bottom());
        opp_trumps = higher;
    }
    kills
}

/// The North-South total a cut by `sure_tricks` gives: C++ `SureTrickCutoff`'s
/// value when the side whose tricks they are (`for_ns`) is known to reach the
/// cut.
fn sure_trick_bound(ns_tricks_won: u8, remaining: usize, sure_tricks: usize, for_ns: bool) -> u8 {
    if for_ns {
        (ns_tricks_won as usize + sure_tricks) as u8
    } else {
        (ns_tricks_won as usize + remaining - sure_tricks) as u8
    }
}

/// A card's number in upstream's layout since 186d3e2: sixteen bits a suit
/// rather than this port's thirteen, the rank order within a suit unchanged.
/// Only the cut-off index needs it, so that its keys -- and with them any
/// tag collisions -- stay exactly upstream's.
#[inline]
fn upstream_card_number(card: usize) -> u64 {
    (card + 3 * suit_of(card)) as u64
}

/// The cut-off cache's tag for a packed index: upstream 8a01aa8's
/// `Cache::Hash`, one multiply keeping the product's top bits.
#[inline]
fn hash_cutoff_index(index: u64) -> CutoffHash {
    (index.wrapping_mul(0x9b8b4567327b23c7) >> (u64::BITS - CutoffHash::BITS)) as CutoffHash
}

/// The cut-off index, packed into one value as upstream 8a01aa8 does, and its
/// tag. Returns (tag, index, 0); the third slot is kept for the trace output.
///
/// Layout: 52 bits of cards, then six bits for the trick's winning card (or
/// winning seat at no-trump when not following suit). The seat's place in the
/// trick is not in the key since upstream 990a665: it picks the card slot
/// inside the entry instead.
#[inline]
#[allow(clippy::too_many_arguments)]
fn build_cutoff_index_debug(
    hands: &Hands,
    seat_to_play: Seat,
    card_in_trick: usize,
    lead_suit: usize,
    winning_card: usize,
    winning_seat: Seat,
    trump: usize,
    all_cards: Cards,
    beta: i8,
    ns_tricks_won: u8,
) -> (CutoffHash, u64, u64) {
    // North-South's tricks still needed, keyed in suit contracts at trick
    // start and when discarding or ruffing (upstream 7f2bb9b): the best lead
    // or discard can change between MTD passes. At no-trump entries stay
    // shared across targets; splitting them there loses on freak deals.
    // Computed as the reference does, in wrapping 64-bit arithmetic.
    let needed = if trump == NOTRUMP {
        0
    } else {
        (beta as i64 - ns_tricks_won as i64) as u64
    };
    let mut index = 0u64;
    if card_in_trick == 0 {
        // Trick starting: the player's hand, and partner's shape parity -- one
        // bit per suit, set when partner holds an odd number (upstream b6e1c1f).
        let pd_hand = hands[partner(seat_to_play)];
        let parity: u64 = (0..NUM_SUITS)
            .map(|suit| ((pd_hand.suit(suit).size() % 2) as u64) << suit)
            .sum();
        index += hands[seat_to_play].value();
        index += parity << TOTAL_CARDS;
        index = index.wrapping_add(needed << (TOTAL_CARDS + 4));
    } else if !hands[seat_to_play].suit(lead_suit).is_empty() {
        // Following suit: every card of the lead suit, the winning card, and
        // the seat that led (upstream 3116473, in place of 0216224's own
        // cards in the suit). Each play passes to the next seat, so the leader
        // is `card_in_trick` seats back.
        let leader = (seat_to_play + NUM_SEATS - card_in_trick) % NUM_SEATS;
        index += all_cards.suit(lead_suit).value();
        index += upstream_card_number(winning_card) << TOTAL_CARDS;
        index += (leader as u64) << (TOTAL_CARDS + 6);
    } else {
        // Not following suit: the player's hand, and the winner -- the seat at
        // no-trump, the card in a suit contract.
        let winner = if trump == NOTRUMP {
            winning_seat as u64
        } else {
            upstream_card_number(winning_card)
        };
        index += hands[seat_to_play].value();
        index += winner << TOTAL_CARDS;
        index = index.wrapping_add(needed << (TOTAL_CARDS + 6));
    }
    (hash_cutoff_index(index), index, 0)
}

/// Format a card as a string
fn card_name(card: usize) -> String {
    const SUITS: [char; 4] = ['S', 'H', 'D', 'C'];
    const RANKS: [char; 13] = [
        'A', 'K', 'Q', 'J', 'T', '9', '8', '7', '6', '5', '4', '3', '2',
    ];
    let suit = suit_of(card);
    let rank = rank_of(card);
    format!("{}{}", SUITS[suit], RANKS[12 - rank])
}

/// Ordered cards container for move ordering
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
    pub fn add(&mut self, card: usize) {
        self.cards[self.count] = card as u8;
        self.count += 1;
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.count
    }

    #[inline]
    pub fn card(&self, i: usize) -> usize {
        self.cards[i] as usize
    }

    /// Keep only the first `n` cards.
    #[inline]
    pub fn truncate(&mut self, n: usize) {
        self.count = self.count.min(n);
    }
}

/// The main search engine, matching C++ `class Play`
pub struct Search<'a> {
    // Fixed info (shared across all depths)
    hands: &'a mut Hands,
    trump: usize,
    num_tricks: usize,

    // Per-depth state arrays (like C++ plays[])
    plays: [PlayState; TOTAL_CARDS],

    // Per-trick state (like C++ tricks[])
    tricks: [Trick; TOTAL_TRICKS],

    // Caches (matching C++ common_bounds_cache and cutoff_cache)
    cutoff_cache: &'a mut CutoffCache,
    pattern_cache: &'a mut PatternCache,

    // Starting depth for mid-trick positions (0 for normal positions)
    start_depth: usize,

    // Nodes already visited by earlier MTD(f) iterations of the same solve.
    // The xray `iter=` field must count the whole solve, because that is what
    // the C++ reference's `g_iteration_count` does and the two traces are
    // meant to be diffed line for line.
    nodes_base: u64,

    // Nodes visited by this search. Deliberately a plain field rather than the
    // process-wide atomic it used to be: incremented once per node, a shared
    // counter is a single cache line read-modify-written by every thread in the
    // program, and it capped concurrent solves at 1.6x on eight cores where
    // this scales to 6.3x. `Solver` sums it across MTD(f) iterations.
    nodes: u64,
}

impl<'a> Search<'a> {
    #[allow(dead_code)]
    pub fn new(
        hands: &'a mut Hands,
        trump: usize,
        initial_leader: Seat,
        cutoff_cache: &'a mut CutoffCache,
        pattern_cache: &'a mut PatternCache,
    ) -> Self {
        Self::new_with_partial_trick(
            hands,
            trump,
            initial_leader,
            cutoff_cache,
            pattern_cache,
            None,
        )
    }

    /// Create a new search, optionally starting from a mid-trick position
    ///
    /// If `partial_trick` is provided, the search starts with those cards already played
    /// to the first trick. The hands should NOT contain the cards in the partial trick.
    pub fn new_with_partial_trick(
        hands: &'a mut Hands,
        trump: usize,
        initial_leader: Seat,
        cutoff_cache: &'a mut CutoffCache,
        pattern_cache: &'a mut PatternCache,
        partial_trick: Option<&super::bridge_solver::PartialTrick>,
    ) -> Self {
        // Compute num_tricks from the largest hand size
        // For mid-trick positions, hands have different sizes (some have played, some haven't)
        // The max hand size represents hands that haven't played yet in the current trick,
        // which equals the total number of tricks remaining
        let max_hand_size = (0..NUM_SEATS).map(|s| hands[s].size()).max().unwrap_or(0);
        let num_tricks = max_hand_size;

        let mut plays = [PlayState::default(); TOTAL_CARDS];
        let mut tricks = [Trick::default(); TOTAL_TRICKS];

        // Initialize the starting search depth based on partial trick
        let start_depth = if let Some(pt) = partial_trick {
            if pt.is_empty() {
                plays[0].seat_to_play = initial_leader;
                0
            } else {
                // Set up the partial trick state
                let lead_suit = suit_of(pt.plays[0].card);
                tricks[0].lead_suit = lead_suit;

                // Compute all_cards including the partial trick cards
                let mut all_cards = hands.all_cards();
                for played in &pt.plays {
                    all_cards.add(played.card);
                }
                tricks[0].all_cards = all_cards;

                // Compute shape from the full hands (including partial trick cards)
                // We need to temporarily add the partial trick cards back to compute the shape
                let mut full_hands = *hands;
                for played in &pt.plays {
                    full_hands[played.seat].add(played.card);
                }
                tricks[0].shape = Shape::from_hands(&full_hands);
                tricks[0].relative_hands.compute(&full_hands, all_cards);

                // Initialize plays for each card in the partial trick
                let mut winning_play = 0;
                let mut winning_card = pt.plays[0].card;

                for (i, played) in pt.plays.iter().enumerate() {
                    plays[i].seat_to_play = played.seat;
                    plays[i].card_played = played.card;
                    plays[i].ns_tricks_won = 0; // No tricks won yet

                    // Determine if this card wins over the current winner
                    if i == 0 {
                        plays[i].winning_play = 0;
                    } else {
                        let card_wins = {
                            let c1 = played.card;
                            let c2 = winning_card;
                            let s1 = suit_of(c1);
                            let s2 = suit_of(c2);

                            if s1 == s2 {
                                higher_rank(c1, c2)
                            } else if trump < NOTRUMP {
                                s1 == trump
                            } else {
                                false
                            }
                        };

                        if card_wins {
                            winning_play = i;
                            winning_card = played.card;
                        }
                        plays[i].winning_play = winning_play;
                    }
                }

                // The next player is after the last played card
                let next_depth = pt.len();
                plays[next_depth].seat_to_play = next_seat(pt.plays.last().unwrap().seat);
                plays[next_depth].ns_tricks_won = 0;
                plays[next_depth].winning_play = winning_play;

                next_depth
            }
        } else {
            plays[0].seat_to_play = initial_leader;
            0
        };

        Search {
            hands,
            trump,
            num_tricks,
            plays,
            tricks,
            cutoff_cache,
            pattern_cache,
            start_depth,
            nodes_base: 0,
            nodes: 0,
        }
    }

    /// Seed the xray iteration counter from earlier MTD(f) iterations, so the
    /// trace numbers the whole solve rather than restarting each iteration.
    pub fn set_nodes_base(&mut self, base: u64) {
        self.nodes_base = base;
    }

    /// Nodes visited so far by this search.
    pub fn nodes(&self) -> u64 {
        self.nodes
    }

    /// Format the play sequence up to (but not including) the given depth
    fn format_play_sequence(&self, depth: usize) -> String {
        if depth == 0 {
            return String::new();
        }
        let mut play_str = String::new();
        for d in 0..depth {
            if d > 0 && d % 4 == 0 {
                play_str.push_str(" | ");
            } else if d > 0 {
                play_str.push(' ');
            }
            play_str.push_str(&card_name(self.plays[d].card_played));
        }
        play_str
    }

    /// Main entry point - search with given beta (null-window)
    pub fn search(&mut self, beta: i8) -> u8 {
        #[cfg(feature = "debug_search")]
        eprintln!(
            "Search::search beta={} num_tricks={} start_depth={} hand_sizes=[{},{},{},{}]",
            beta,
            self.num_tricks,
            self.start_depth,
            self.hands[0].size(),
            self.hands[1].size(),
            self.hands[2].size(),
            self.hands[3].size()
        );
        let result = self.search_with_cache(self.start_depth, beta);
        #[cfg(feature = "debug_search")]
        eprintln!(
            "Search::search DONE beta={} result={} hand_sizes=[{},{},{},{}]",
            beta,
            result.ns_tricks,
            self.hands[0].size(),
            self.hands[1].size(),
            self.hands[2].size(),
            self.hands[3].size()
        );
        result.ns_tricks
    }

    /// SearchWithCache - called at every depth, handles trick boundaries
    /// Matches C++ Play::SearchWithCache
    fn search_with_cache(&mut self, depth: usize, beta: i8) -> SearchResult {
        let card_in_trick = depth & 3;

        // XRAY counter - increment on EVERY recursive call (not just trick boundaries)
        let limit = XRAY_LIMIT.load(Ordering::Relaxed);
        if limit > 0 {
            let count = XRAY_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
            if count == limit + 1 {
                eprintln!("XRAY_LIMIT_REACHED: {} iterations", limit);
            }
            let step = XRAY_CACHE_STEP.load(Ordering::Relaxed);
            let start = XRAY_CACHE_START.load(Ordering::Relaxed);
            let end = XRAY_CACHE_END.load(Ordering::Relaxed);
            if step > 0
                && count <= limit
                && count >= start
                && (end == 0 || count <= end)
                && (count - start).is_multiple_of(step)
            {
                let (cutoff_n, cutoff_h) = self.cutoff_cache.digest();
                let (pattern_n, pattern_h) = self.pattern_cache.digest();
                eprintln!(
                    "CACHE: iter={count} cutoff={cutoff_n}/{cutoff_h:016x} pattern={pattern_n}/{pattern_h:016x}"
                );
                // A window of exactly one iteration means "show me this one",
                // so break the pattern cache out entry by entry.
                if start == end && start > 0 {
                    self.pattern_cache.dump(count);
                }
            }
        }

        // Mid-trick: get state from previous play
        if card_in_trick != 0 {
            self.enter_follow(depth);
            return self.evaluate_playable_cards(depth, beta);
        }

        self.search_trick_start(depth, beta)
    }

    /// The trick-start half of [`Self::search_with_cache`]: bounds, the
    /// pattern cache, sure tricks, then the search, and storing the result.
    ///
    /// Out of line (upstream 31802f9) so the three follower plays of every
    /// trick, which return from `search_with_cache` before reaching it, do not
    /// pay this large frame's prologue.
    #[inline(never)]
    fn search_trick_start(&mut self, depth: usize, beta: i8) -> SearchResult {
        let trick_idx = depth / 4;
        // Trick start: compute ns_tricks_won and seat_to_play from previous trick
        self.enter_trick_start(depth);

        let ns_tricks_won = self.plays[depth].ns_tricks_won;
        let seat_to_play = self.plays[depth].seat_to_play;

        // XRAY detailed logging (at trick boundaries only, for readability)
        if xray_should_log() {
            let count = XRAY_COUNT.load(Ordering::Relaxed);
            let seat_name = match seat_to_play {
                WEST => "West",
                NORTH => "North",
                EAST => "East",
                SOUTH => "South",
                _ => "?",
            };
            eprintln!(
                "XRAY {}: depth={} seat={} beta={} ns_tricks_won={}",
                count, depth, seat_name, beta, ns_tricks_won
            );
            // Log hands at trick start
            eprintln!(
                "HANDS: W={:x} N={:x} E={:x} S={:x}",
                self.hands[WEST].value(),
                self.hands[NORTH].value(),
                self.hands[EAST].value(),
                self.hands[SOUTH].value()
            );
            // Log cards played so far
            if depth > 0 {
                let mut play_str = String::new();
                for d in 0..depth {
                    if d > 0 && d % 4 == 0 {
                        play_str.push_str(" | ");
                    } else if d > 0 {
                        play_str.push(' ');
                    }
                    play_str.push_str(&card_name(self.plays[d].card_played));
                }
                eprintln!("PLAY: {}", play_str);
            }
        }

        // Quick bounds check
        if ns_tricks_won as i8 >= beta {
            return SearchResult {
                ns_tricks: ns_tricks_won,
                rank_winners: Cards::new(),
            };
        }
        let remaining = self.num_tricks - trick_idx;
        if (ns_tricks_won as usize + remaining) < beta as usize {
            return SearchResult {
                ns_tricks: ns_tricks_won + remaining as u8,
                rank_winners: Cards::new(),
            };
        }

        // Last trick optimization
        if remaining == 1 {
            let result = self.collect_last_trick(depth);
            #[cfg(feature = "debug_search")]
            eprintln!(
                "  collect_last_trick depth={} ns_tricks={} -> result={}",
                depth, ns_tricks_won, result.ns_tricks
            );
            return result;
        }

        let all_cards = self.setup_trick_start(depth);

        // Pattern cache lookup (matching C++ common_bounds_cache)
        let shape_value = self.tricks[trick_idx].shape.value();
        // Hashed once for both the lookup below and the store further down.
        let pattern_hash = PatternCache::hash_for(shape_value);
        let mut pattern_cutoff = false;
        let rel_beta = beta - ns_tricks_won as i8;
        if !NO_TT.load(Ordering::Relaxed) {
            if let Some(entry) = self.pattern_cache.lookup(pattern_hash) {
                // Use relative beta for cutoff check (bounds are stored relative to ns_tricks_won)
                let (low, high) =
                    PatHands::from_hands(&self.tricks[trick_idx].relative_hands.hands).planes();
                if let Some((matched_hands, bounds)) =
                    Pattern::lookup_in(&entry.patterns[seat_to_play], low, high, rel_beta)
                        .map(|matched| (matched.hands, matched.bounds))
                {
                    // Compute rank_winners from matched pattern (matching C++ GetRankWinners)
                    let rank_winners = Pattern::rank_winners_of(matched_hands.cards(), all_cards);

                    let adj_lower = bounds.lower + ns_tricks_won as i8;
                    let adj_upper = bounds.upper + ns_tricks_won as i8;
                    if adj_lower >= beta {
                        if xray_should_log() {
                            eprintln!(
                                "PATTERN_HIT: depth={} seat={} beta={} ns_tricks_won={} bounds=[{},{}] adj_lower={} LOWER_CUT shape={:x} hands=[{:x},{:x},{:x},{:x}]",
                                depth, seat_to_play, beta, ns_tricks_won, bounds.lower, bounds.upper, adj_lower,
                                shape_value,
                                matched_hands.hands()[WEST].value(),
                                matched_hands.hands()[NORTH].value(),
                                matched_hands.hands()[EAST].value(),
                                matched_hands.hands()[SOUTH].value()
                            );
                        }
                        return SearchResult {
                            ns_tricks: adj_lower as u8,
                            rank_winners,
                        };
                    }
                    if adj_upper < beta {
                        if xray_should_log() {
                            eprintln!(
                                "PATTERN_HIT: depth={} seat={} beta={} ns_tricks_won={} bounds=[{},{}] adj_upper={} UPPER_CUT",
                                depth, seat_to_play, beta, ns_tricks_won, bounds.lower, bounds.upper, adj_upper
                            );
                        }
                        return SearchResult {
                            ns_tricks: adj_upper as u8,
                            rank_winners,
                        };
                    }
                    pattern_cutoff = true;
                }
            }
        }

        // Search at trick start (with pruning). A sure-trick cut skips no card
        // as equivalent, so starts from no runs.
        self.plays[depth].equivalence_runs = Cards::new();
        let result = self.search_at_trick_start(depth, beta);

        // Pattern cache store (matching C++ common_bounds_cache)
        if !NO_TT.load(Ordering::Relaxed) && !pattern_cutoff {
            let relative_tricks = result.ns_tricks as i8 - ns_tricks_won as i8;
            let bounds = if (result.ns_tricks as i8) < beta {
                Bounds::new(0, relative_tricks)
            } else {
                Bounds::new(relative_tricks, remaining as i8)
            };

            // Compute pattern hands from relative hands and rank winners
            // This filters to only rank-relevant cards, matching C++ ComputePatternHands
            let (pattern_hands, extended_rank_winners) = compute_pattern_hands(
                &self.tricks[trick_idx].relative_hands.hands,
                self.hands,
                all_cards,
                result.rank_winners,
                self.plays[depth].equivalence_runs,
            );

            let new_pattern = Pattern::new(pattern_hands, bounds);
            if xray_should_log() {
                eprintln!(
                    "PATTERN_STORE: depth={} seat={} beta={} ns_tricks_won={} result={} bounds=[{},{}] shape={:x} hands=[{:x},{:x},{:x},{:x}] rank_winners={:x}",
                    depth, seat_to_play, beta, ns_tricks_won, result.ns_tricks, bounds.lower, bounds.upper,
                    shape_value,
                    pattern_hands[WEST].value(), pattern_hands[NORTH].value(),
                    pattern_hands[EAST].value(), pattern_hands[SOUTH].value(),
                    result.rank_winners.value()
                );
            }
            let entry = self.pattern_cache.get_or_create(pattern_hash);
            Pattern::update_in(&mut entry.patterns[seat_to_play], new_pattern);

            // Return with extended_rank_winners instead of raw rank_winners
            return SearchResult {
                ns_tricks: result.ns_tricks,
                rank_winners: extended_rank_winners,
            };
        }

        result
    }

    /// A follower's state at `depth`: the trick count carried over, and the
    /// next seat to play.
    fn enter_follow(&mut self, depth: usize) {
        self.plays[depth].ns_tricks_won = self.plays[depth - 1].ns_tricks_won;
        self.plays[depth].seat_to_play = next_seat(self.plays[depth - 1].seat_to_play);
    }

    /// A trick start's state at `depth`: the last trick's winner leads, and
    /// North-South's count includes that trick. Nothing to do at depth 0.
    fn enter_trick_start(&mut self, depth: usize) {
        if depth == 0 {
            return;
        }
        let prev = self.plays[depth - 1];
        let winner = self.plays[prev.winning_play].seat_to_play;
        self.plays[depth].ns_tricks_won = prev.ns_tricks_won + u8::from(is_ns(winner));
        self.plays[depth].seat_to_play = winner;
    }

    /// The trick starting at `depth`: its cards, and its shape and relative
    /// hands, derived from the last trick's and the cards played to it.
    /// Returns the cards.
    fn setup_trick_start(&mut self, depth: usize) -> Cards {
        let trick_idx = depth / 4;
        let all_cards = self.hands.all_cards();
        self.tricks[trick_idx].all_cards = all_cards;
        if depth == 0 {
            self.tricks[trick_idx].shape = Shape::from_hands(self.hands);
            self.tricks[trick_idx]
                .relative_hands
                .compute(self.hands, all_cards);
        } else {
            let prev_trick_idx = trick_idx - 1;
            let prev_all_cards = self.tricks[prev_trick_idx].all_cards;
            let mut shape = self.tricks[prev_trick_idx].shape;
            let prev_start = prev_trick_idx * 4;
            shape.play_cards(
                self.plays[prev_start].seat_to_play,
                self.plays[prev_start].card_played,
                self.plays[prev_start + 1].card_played,
                self.plays[prev_start + 2].card_played,
                self.plays[prev_start + 3].card_played,
            );
            self.tricks[trick_idx].shape = shape;
            self.tricks[trick_idx].relative_hands = self.tricks[prev_trick_idx].relative_hands;
            self.tricks[trick_idx]
                .relative_hands
                .update(self.hands, prev_all_cards, all_cards);
        }
        all_cards
    }

    /// Play `card` at `depth`: take it from the hand, and set the trick's lead
    /// suit or the play that is winning it. Undo with
    /// `self.hands[seat].add(card)`; the rest is overwritten by the next play
    /// at this depth.
    fn apply_play(&mut self, depth: usize, card: usize) {
        let trick_idx = depth / 4;
        let seat_to_play = self.plays[depth].seat_to_play;
        self.plays[depth].card_played = card;
        self.hands[seat_to_play].remove(card);
        if depth & 3 == 0 {
            self.tricks[trick_idx].lead_suit = suit_of(card);
            self.plays[depth].winning_play = depth;
        } else {
            let current = self.plays[depth - 1].winning_play;
            let current_card = self.plays[current].card_played;
            let lead_suit = self.tricks[trick_idx].lead_suit;
            self.plays[depth].winning_play = if self.wins_over(card, current_card, lead_suit) {
                depth
            } else {
                current
            };
        }
    }

    /// Append `playable` to `ordered` in search order, as C++ `OrderCards`.
    /// Near the root (twelve or more tricks to go) a lead's order is then
    /// refined by [`Self::promote_probed_leads`] (upstream 3f6be83).
    fn order_cards(&mut self, ordered: &mut OrderedCards, depth: usize, playable: Cards, beta: i8) {
        let start = ordered.len();
        Self::order_cards_static(
            ordered,
            depth,
            playable,
            &self.plays,
            &self.tricks,
            self.hands,
            self.trump,
        );
        if depth & 3 == 0 && playable.size() > 1 && self.num_tricks - depth / 4 >= 12 {
            self.promote_probed_leads(ordered, start, depth, beta);
        }
    }

    /// Upstream's `PromoteProbedLeads`: play each lead from `start` on and the
    /// rest of its trick greedily ([`Self::probe_follow`]), and move to the
    /// front, in their order, the leads whose next trick start is a known cut
    /// for the leader. A wrong first lead is costly near the root, and one
    /// that transposes into a solved position is usually the cheapest proof.
    fn promote_probed_leads(
        &mut self,
        ordered: &mut OrderedCards,
        start: usize,
        depth: usize,
        beta: i8,
    ) {
        let maximizing = is_ns(self.plays[depth].seat_to_play);
        let seat = self.plays[depth].seat_to_play;
        let mut hits = OrderedCards::new();
        let mut misses = OrderedCards::new();
        for i in start..ordered.len() {
            let card = ordered.card(i);
            self.apply_play(depth, card);
            let value = self.probe_follow(depth + 1, beta);
            self.hands[seat].add(card);
            let hit = value.is_some_and(|v| if maximizing { v >= beta } else { v < beta });
            if hit {
                hits.add(card);
            } else {
                misses.add(card);
            }
        }
        if hits.len() == 0 {
            return;
        }
        ordered.truncate(start);
        for i in 0..hits.len() {
            ordered.add(hits.card(i));
        }
        for i in 0..misses.len() {
            ordered.add(misses.card(i));
        }
    }

    /// Upstream's `ProbeFollow`: finish the trick from `depth`, each follower
    /// playing its cut-off card if it holds one and its first-ordered card
    /// otherwise, then look the next trick start up in the pattern cache.
    /// North-South's tricks if that settles it against `beta`, else `None`.
    ///
    /// Reads the caches and changes neither; the play state it leaves at the
    /// depths it passes through is overwritten when the search gets there.
    fn probe_follow(&mut self, depth: usize, beta: i8) -> Option<i8> {
        let trick_idx = depth / 4;
        let card_in_trick = depth & 3;
        if card_in_trick != 0 {
            self.enter_follow(depth);
            let seat = self.plays[depth].seat_to_play;
            let lead_suit = self.tricks[trick_idx].lead_suit;
            let playable = get_playable_cards(self.hands, seat, Some(lead_suit));
            let winning_play = self.plays[depth - 1].winning_play;
            let (cutoff_hash, _, _) = build_cutoff_index_debug(
                self.hands,
                seat,
                card_in_trick,
                lead_suit,
                self.plays[winning_play].card_played,
                self.plays[winning_play].seat_to_play,
                self.trump,
                self.tricks[trick_idx].all_cards,
                beta,
                self.plays[depth].ns_tricks_won,
            );
            let cached = if NO_TT.load(Ordering::Relaxed) {
                None
            } else {
                self.cutoff_cache.lookup(cutoff_hash, card_in_trick)
            };
            let card = match cached {
                Some(card) if playable.have(card) => card,
                _ => {
                    let mut ordered = OrderedCards::new();
                    self.order_cards(&mut ordered, depth, playable, beta);
                    ordered.card(0)
                }
            };
            self.apply_play(depth, card);
            let value = self.probe_follow(depth + 1, beta);
            self.hands[seat].add(card);
            return value;
        }
        self.enter_trick_start(depth);
        let ns_tricks_won = self.plays[depth].ns_tricks_won as i8;
        let remaining = (self.num_tricks - trick_idx) as i8;
        if ns_tricks_won >= beta {
            return Some(ns_tricks_won);
        }
        if ns_tricks_won + remaining < beta {
            return Some(ns_tricks_won + remaining);
        }
        self.setup_trick_start(depth);
        if NO_TT.load(Ordering::Relaxed) {
            return None;
        }
        let seat = self.plays[depth].seat_to_play;
        let hash = PatternCache::hash_for(self.tricks[trick_idx].shape.value());
        let entry = self.pattern_cache.lookup(hash)?;
        let (low, high) =
            PatHands::from_hands(&self.tricks[trick_idx].relative_hands.hands).planes();
        let pattern = Pattern::lookup_in(&entry.patterns[seat], low, high, beta - ns_tricks_won)?;
        let lower = pattern.bounds.lower + ns_tricks_won;
        Some(if lower >= beta {
            lower
        } else {
            pattern.bounds.upper + ns_tricks_won
        })
    }

    /// SearchAtTrickStart - fast/slow tricks pruning
    /// Matches C++ Play::SearchAtTrickStart
    fn search_at_trick_start(&mut self, depth: usize, beta: i8) -> SearchResult {
        let trick_idx = depth / 4;
        let ns_tricks_won = self.plays[depth].ns_tricks_won;
        let seat_to_play = self.plays[depth].seat_to_play;
        let remaining = self.num_tricks - trick_idx;

        if !NO_PRUNING.load(Ordering::Relaxed) {
            // Fast tricks pruning
            let (fast, fast_rank_winners, side_tricks, side_rank_winners) = self.fast_tricks(depth);
            let from_fast_tricks = fast > 0;

            // Debug logging for fast tricks
            if xray_should_log() {
                eprintln!(
                    "FAST_TRICKS: depth={} seat={} fast={} trump={}",
                    depth, seat_to_play, fast, self.trump
                );
            }

            // A zero fast-trick count is upgraded from the side to play's own
            // perspective before the fast cuts, matching the C++ reference.
            //
            // Omitting this is not merely a lost prune. The cut it enables
            // returns a different `rank_winners` set, and the bounds cache
            // keys its entries on exactly those cards — so without it the
            // search stores over-general patterns that later match positions
            // the bound does not hold for, and the cached bound is then wrong.
            // That is the mechanism behind the wrong tables in #14.
            let (fast, fast_rank_winners) = if fast == 0 && self.trump != NOTRUMP {
                self.slow_trump_tricks_own(depth)
            } else {
                (fast, fast_rank_winners)
            };
            // Upstream f547a97: with trumps out, the trump-length count stands
            // in when it is at least as large. It needs no rank winners: trump
            // lengths are in the shape, which keys the pattern cache.
            let (fast, fast_rank_winners) = match self.trump_length_tricks(
                seat_to_play,
                partner(seat_to_play),
                left_hand_opp(seat_to_play),
                right_hand_opp(seat_to_play),
            ) {
                Some(length) if length >= fast => (length, Cards::new()),
                _ => (fast, fast_rank_winners),
            };

            // Tricks each side still needs to decide this search.
            let all_cards = self.hands.all_cards();
            let has_trumps = self.trump < NOTRUMP && !all_cards.suit(self.trump).is_empty();
            let lead_need = if is_ns(seat_to_play) {
                beta as isize - ns_tricks_won as isize
            } else {
                remaining as isize - (beta as isize - ns_tricks_won as isize) + 1
            };
            let other_need = remaining as isize - lead_need + 1;

            let (mut fast, mut fast_rank_winners) = (fast, fast_rank_winners);
            let ns_to_play = is_ns(seat_to_play);
            let mut lead_cut = sure_trick_cutoff(ns_tricks_won, remaining, beta, fast, ns_to_play);
            // Bob Salita's b12bb3f: the cut may need fewer tricks than there
            // are. Keep only the rank winners of the tricks it needs, for a
            // pattern that matches more positions.
            if lead_cut.is_some()
                && from_fast_tricks
                && !fast_rank_winners.is_empty()
                && self.minimize_fast_tricks(
                    depth,
                    lead_need,
                    has_trumps,
                    &mut fast,
                    &mut fast_rank_winners,
                )
            {
                lead_cut = sure_trick_cutoff(ns_tricks_won, remaining, beta, fast, ns_to_play);
            }
            if let Some(ns_tricks) = lead_cut {
                return SearchResult {
                    ns_tricks,
                    rank_winners: fast_rank_winners,
                };
            }

            // Bob Salita's b12bb3f: the side-suit fast tricks, then the trump
            // tricks one hand has by rank. That hand keeps its trumps meanwhile
            // if it has enough other cards to play to the side-suit tricks.
            if has_trumps
                && lead_need - fast as isize <= MAX_RANK_TRICK_GAP
                && lead_need > side_tricks as isize
            {
                let opp_trumps = self.hands[left_hand_opp(seat_to_play)]
                    .suit(self.trump)
                    .union(self.hands[right_hand_opp(seat_to_play)].suit(self.trump));
                for seat in [seat_to_play, partner(seat_to_play)] {
                    let hand_trumps = self.hands[seat].suit(self.trump);
                    if self.hands[seat].size() - hand_trumps.size() < side_tricks {
                        continue;
                    }
                    let (trump_tricks, trump_rank_winners) = self.trump_rank_tricks(
                        depth,
                        hand_trumps,
                        opp_trumps,
                        lead_need - side_tricks as isize,
                    );
                    if trump_tricks > 0 {
                        return SearchResult {
                            ns_tricks: sure_trick_bound(
                                ns_tricks_won,
                                remaining,
                                side_tricks + trump_tricks,
                                is_ns(seat_to_play),
                            ),
                            rank_winners: side_rank_winners.union(trump_rank_winners),
                        };
                    }
                }
            }
            // Bob Salita's b12bb3f: draw the opponents' trumps with our top
            // trumps, then cash as at no-trump.
            if has_trumps && lead_need - fast as isize <= MAX_DRAW_TRUMP_GAP {
                let (draw_tricks, draw_rank_winners) = self.draw_trumps_then_cash(depth);
                if draw_tricks as isize >= lead_need {
                    return SearchResult {
                        ns_tricks: sure_trick_bound(
                            ns_tricks_won,
                            remaining,
                            draw_tricks,
                            is_ns(seat_to_play),
                        ),
                        rank_winners: draw_rank_winners,
                    };
                }
            }

            // Slow tricks pruning
            // For trump contracts with trumps remaining: use top trump tricks, then slow trump tricks
            // For NT contracts OR trump contract with no trumps remaining: use slow NT tricks
            let (slow, slow_rank_winners) = if has_trumps {
                // Trump contract with trumps remaining
                let (top, top_rw) = self.top_trump_tricks_opponent(depth);
                if top > 0 {
                    (top, top_rw)
                } else {
                    // Try slow trump tricks (finesse positions)
                    self.slow_trump_tricks_opponent(depth)
                }
            } else {
                // NT contract OR trump contract with no trumps remaining
                self.slow_tricks_opponent(depth)
            };
            // Bob Salita's b12bb3f: when the other side needs just one trick
            // and has none sure, it still gets one if the side on lead cannot
            // take them all.
            let (slow, slow_rank_winners) = if !has_trumps && slow == 0 {
                if other_need == 1 {
                    self.cannot_take_all_no_trump(depth, seat_to_play)
                } else {
                    (slow, slow_rank_winners)
                }
            } else {
                (slow, slow_rank_winners)
            };
            // The other side's trump-length count, as for the side to play.
            let (slow, slow_rank_winners) = match self.trump_length_tricks(
                left_hand_opp(seat_to_play),
                right_hand_opp(seat_to_play),
                partner(seat_to_play),
                seat_to_play,
            ) {
                Some(length) if length >= slow => (length, Cards::new()),
                _ => (slow, slow_rank_winners),
            };
            // Bob Salita's b12bb3f: one trick short, the other side may still
            // have it in one opponent's trumps by rank.
            let (slow, slow_rank_winners) = if has_trumps && other_need - slow as isize == 1 {
                let our_trumps = self.hands[seat_to_play]
                    .suit(self.trump)
                    .union(self.hands[partner(seat_to_play)].suit(self.trump));
                let mut found = (slow, slow_rank_winners);
                for seat in [left_hand_opp(seat_to_play), right_hand_opp(seat_to_play)] {
                    let (tricks, rank_winners) = self.trump_rank_tricks(
                        depth,
                        self.hands[seat].suit(self.trump),
                        our_trumps,
                        other_need,
                    );
                    if tricks > 0 {
                        found = (tricks, rank_winners);
                        break;
                    }
                }
                found
            } else {
                (slow, slow_rank_winners)
            };

            // Debug logging for slow tricks
            if xray_should_log() {
                eprintln!(
                    "SLOW_TRICKS: depth={} seat={} slow={} trump={}",
                    depth, seat_to_play, slow, self.trump
                );
            }

            if slow > 0 {
                if is_ns(seat_to_play) {
                    // NS to play, check if EW's slow tricks limit NS
                    if (ns_tricks_won as usize + remaining - slow) < beta as usize {
                        return SearchResult {
                            ns_tricks: (ns_tricks_won as usize + remaining - slow) as u8,
                            rank_winners: slow_rank_winners,
                        };
                    }
                } else {
                    // EW to play, check if NS's slow tricks give them enough
                    if ns_tricks_won as usize + slow >= beta as usize {
                        return SearchResult {
                            ns_tricks: (ns_tricks_won as usize + slow) as u8,
                            rank_winners: slow_rank_winners,
                        };
                    }
                }
            }
        }

        self.evaluate_playable_cards(depth, beta)
    }

    /// EvaluatePlayableCards - main card evaluation loop
    /// Matches C++ Play::EvaluatePlayableCards
    fn evaluate_playable_cards(&mut self, depth: usize, beta: i8) -> SearchResult {
        self.nodes += 1;

        let trick_idx = depth / 4;
        let card_in_trick = depth & 3;
        let ns_tricks_won = self.plays[depth].ns_tricks_won;
        let seat_to_play = self.plays[depth].seat_to_play;
        let maximizing = is_ns(seat_to_play);

        if card_in_trick == 1 && second_seat_rule_enabled() {
            if let Some(result) = self.second_seat_sure_tricks(depth, beta) {
                // No card was skipped as equivalent to another.
                self.plays[depth].equivalence_runs = Cards::new();
                return result;
            }
        }

        // Get playable cards
        let lead_suit = if card_in_trick == 0 {
            None
        } else {
            Some(self.tricks[trick_idx].lead_suit)
        };
        let playable = get_playable_cards(self.hands, seat_to_play, lead_suit);

        if playable.is_empty() {
            self.plays[depth].equivalence_runs = Cards::new();
            return SearchResult {
                ns_tricks: ns_tricks_won,
                rank_winners: Cards::new(),
            };
        }

        // One choice -- a single card, or cards of one suit with nothing
        // between them at the start of the trick -- is played directly,
        // without hashing, looking up or saving a cut-off card (upstream
        // 6848863). That is 23-31% of nodes, almost all after the lead.
        let (top, bottom) = (playable.top(), playable.bottom());
        if suit_of(top) == suit_of(bottom)
            && self.tricks[trick_idx].all_cards.slice(top, bottom + 1) == playable
        {
            let result = self.play_card_and_search(depth, top, beta);
            let mut runs = self.next_runs(depth);
            // A failure relies on the other cards of the run failing like the
            // one played.
            let failed = if maximizing {
                (result.ns_tricks as i8) < beta
            } else {
                result.ns_tricks as i8 >= beta
            };
            if top != bottom && failed {
                runs = runs.union(Cards::from_bits(!0).slice(top, bottom));
            }
            self.plays[depth].equivalence_runs = runs;
            return result;
        }

        // XRAY logging for playable cards
        if xray_should_log() {
            eprintln!(
                "PLAYABLE: depth={} seat={} count={} cards={:x}",
                depth,
                seat_to_play,
                playable.size(),
                playable.value()
            );
        }

        // Note: Even for single card, we go through the loop to ensure cutoff checks happen.
        // This matches C++ behavior where single cards still get cutoff checks.

        // Build ordered cards - must be LOCAL to avoid corruption by recursive calls
        let mut ordered_cards = OrderedCards::new();

        // Get winning card/seat for cutoff index (only valid when not at trick start)
        let (winning_card, winning_seat) = if card_in_trick > 0 {
            let winning_play_idx = self.plays[depth - 1].winning_play;
            (
                self.plays[winning_play_idx].card_played,
                self.plays[winning_play_idx].seat_to_play,
            )
        } else {
            (0, 0) // Not used at trick start
        };

        // Check cutoff cache first (using C++ style 2-key index)
        let all_cards = self.tricks[trick_idx].all_cards;
        let lead_suit_for_cutoff = lead_suit.unwrap_or(0);
        let (cutoff_hash, key0, key1) = build_cutoff_index_debug(
            self.hands,
            seat_to_play,
            card_in_trick,
            lead_suit_for_cutoff,
            winning_card,
            winning_seat,
            self.trump,
            all_cards,
            beta,
            ns_tricks_won,
        );
        let cutoff_card = if !NO_TT.load(Ordering::Relaxed) {
            self.cutoff_cache.lookup(cutoff_hash, card_in_trick)
        } else {
            None
        };

        // CUTOFF_INDEX logging
        let iter_count = self.nodes_base + self.nodes;
        if xray_should_log() {
            eprintln!(
                "CUTOFF_INDEX: iter={} depth={} key0={:x} key1={:x} seat={}",
                iter_count, depth, key0, key1, seat_to_play
            );
        }

        // MOVE_ORDER logging BEFORE update
        let iter_count = self.nodes_base + self.nodes;
        if xray_should_log() {
            let mut playable_str = String::new();
            for card in playable.iter() {
                if !playable_str.is_empty() {
                    playable_str.push(' ');
                }
                playable_str.push_str(&card_name(card));
            }
            let cutoff_str = match cutoff_card {
                Some(cc) if playable.have(cc) => card_name(cc),
                _ => String::new(),
            };
            eprintln!(
                "MOVE_ORDER_BEFORE: iter={} depth={} play=[{}] playable=[{}] cutoff_cards=[{}]",
                iter_count,
                depth,
                self.format_play_sequence(depth),
                playable_str,
                cutoff_str
            );
        }

        let mut remaining_playable = playable;
        let has_cutoff = if let Some(cc) = cutoff_card {
            if playable.have(cc) {
                // C++ behavior: add cutoff card first, keep remaining in remaining_playable
                ordered_cards.add(cc);
                remaining_playable.remove(cc);
                true
            } else {
                false
            }
        } else {
            false
        };

        if !has_cutoff {
            // No cutoff card - order all playable cards and clear remaining
            self.order_cards(&mut ordered_cards, depth, remaining_playable, beta);
            remaining_playable = Cards::new();
        }

        // MOVE_ORDER logging AFTER update
        if xray_should_log() {
            let mut ordered_str = String::new();
            for i in 0..ordered_cards.len() {
                if !ordered_str.is_empty() {
                    ordered_str.push(' ');
                }
                ordered_str.push_str(&card_name(ordered_cards.card(i)));
            }
            let mut remaining_str = String::new();
            for card in remaining_playable.iter() {
                if !remaining_str.is_empty() {
                    remaining_str.push(' ');
                }
                remaining_str.push_str(&card_name(card));
            }
            eprintln!(
                "MOVE_ORDER_AFTER: iter={} depth={} ordered=[{}] remaining=[{}]",
                iter_count, depth, ordered_str, remaining_str
            );
        }

        #[cfg(feature = "debug_search")]
        {
            eprintln!(
                "evaluate_playable depth={} seat={} playable={:x} ordered_count={}",
                depth,
                seat_to_play,
                playable.value(),
                ordered_cards.len()
            );
            for i in 0..ordered_cards.len() {
                let c = ordered_cards.card(i);
                eprintln!(
                    "  ordered[{}] = {} (have={})",
                    i,
                    card_name(c),
                    self.hands[seat_to_play].have(c)
                );
            }
        }

        // XRAY logging for ordered cards
        if xray_should_log() {
            let mut cards_str = String::new();
            for i in 0..ordered_cards.len() {
                if i > 0 {
                    cards_str.push(' ');
                }
                cards_str.push_str(&card_name(ordered_cards.card(i)));
            }
            eprintln!(
                "ORDERED: depth={} playable={:x} count={} cards=[{}]",
                depth,
                playable.value(),
                ordered_cards.len(),
                cards_str
            );
        }

        // Get my_hand for equivalence checks (all_cards already computed above)
        let my_hand = self.hands[seat_to_play];

        // Search loop
        let mut best = if maximizing {
            0u8
        } else {
            self.num_tricks as u8
        };
        let mut tried_cards = Cards::new();
        let mut rank_winners = Cards::new();
        // min_relevant_ranks[suit] = minimum rank that matters for this suit (0 = TWO, 12 = ACE)
        let mut min_relevant_ranks = [0usize; NUM_SUITS];
        let no_rank_skip = NO_RANK_SKIP.load(Ordering::Relaxed);

        // The runs a failure of every card relies on.
        let mut runs = Cards::new();
        let mut i = 0;
        while i < ordered_cards.len() {
            let card = ordered_cards.card(i);
            let suit = suit_of(card);
            let rank = rank_of(card);

            // Skip if rank is below minimum relevant rank (unless no_rank_skip is set)
            if !no_rank_skip && rank < min_relevant_ranks[suit] {
                tried_cards.add(card);
                // C++ behavior: after trying first card, order remaining playable cards
                if !remaining_playable.is_empty() {
                    self.order_cards(&mut ordered_cards, depth, remaining_playable, beta);
                    remaining_playable = Cards::new();
                }
                i += 1;
                continue;
            }

            // IsEquivalent check - always call, even for first card (matches C++ behavior)
            let equivalent_run =
                self.equivalent_run(card, tried_cards.suit(suit), my_hand, all_cards);
            if !equivalent_run.is_empty() {
                runs = runs.union(equivalent_run);
                tried_cards.add(card);
                // C++ behavior: after trying first card, order remaining playable cards
                if !remaining_playable.is_empty() {
                    self.order_cards(&mut ordered_cards, depth, remaining_playable, beta);
                    remaining_playable = Cards::new();
                }
                i += 1;
                continue;
            }
            tried_cards.add(card);

            // Play and search
            let branch_result = self.play_card_and_search(depth, card, beta);
            let score = branch_result.ns_tricks;
            let branch_rank_winners = branch_result.rank_winners;

            // XRAY logging for score
            if xray_should_log() {
                eprintln!(
                    "SCORE: depth={} card={} score={} best={} beta={} maximizing={} rank_winners={:x} play=[{}]",
                    depth, card_name(card), score, best, beta, maximizing, branch_rank_winners.value(), self.format_play_sequence(depth)
                );
            }

            if maximizing {
                if score > best {
                    best = score;
                }
                if best as i8 >= beta {
                    // XRAY logging for cutoff
                    if xray_should_log() {
                        eprintln!(
                            "CUTOFF: depth={} seat={} card={} score={} best={} beta={} maximizing=true play=[{}]",
                            depth, seat_to_play, card_name(card), score, best, beta, self.format_play_sequence(depth)
                        );
                    }
                    // Store cutoff card
                    if cutoff_card != Some(card) && !NO_TT.load(Ordering::Relaxed) {
                        self.cutoff_cache.store(cutoff_hash, card_in_trick, card);
                    }
                    // One card that works is enough: only its own play relies
                    // on equivalence runs.
                    self.plays[depth].equivalence_runs = self.next_runs(depth);
                    return SearchResult {
                        ns_tricks: best,
                        rank_winners: self.lift_run_winner(depth, card, branch_rank_winners),
                    };
                }
            } else {
                if score < best {
                    best = score;
                }
                if (best as i8) < beta {
                    // XRAY logging for cutoff
                    if xray_should_log() {
                        eprintln!(
                            "CUTOFF: depth={} seat={} card={} score={} best={} beta={} maximizing=false play=[{}]",
                            depth, seat_to_play, card_name(card), score, best, beta, self.format_play_sequence(depth)
                        );
                    }
                    // Store cutoff card (for minimizer, cutoff is when score < beta)
                    if cutoff_card != Some(card) && !NO_TT.load(Ordering::Relaxed) {
                        self.cutoff_cache.store(cutoff_hash, card_in_trick, card);
                    }
                    // One card that works is enough: only its own play relies
                    // on equivalence runs.
                    self.plays[depth].equivalence_runs = self.next_runs(depth);
                    return SearchResult {
                        ns_tricks: best,
                        rank_winners: self.lift_run_winner(depth, card, branch_rank_winners),
                    };
                }
            }

            // Accumulate rank_winners from this branch
            rank_winners.add_cards(branch_rank_winners);
            runs = runs.union(self.next_runs(depth));

            // Update min_relevant_ranks based on branch_rank_winners
            let suit_rank_winners = branch_rank_winners.suit(suit);
            let old_min = min_relevant_ranks[suit];
            if suit_rank_winners.is_empty() {
                // No rank winners in this suit - all higher cards are irrelevant
                min_relevant_ranks[suit] = NUM_RANKS;
            } else {
                // Find bottom (lowest) rank winner in this suit
                let bottom_winner = suit_rank_winners.bottom();
                let bottom_rank = rank_of(bottom_winner);
                // If card played is lower than bottom winner, update min_relevant_ranks
                if rank < bottom_rank {
                    min_relevant_ranks[suit] = min_relevant_ranks[suit].max(bottom_rank);
                }
            }

            // RANK_UPDATE logging
            if min_relevant_ranks[suit] != old_min && xray_should_log() {
                let winner_str = if suit_rank_winners.is_empty() {
                    "none".to_string()
                } else {
                    card_name(suit_rank_winners.bottom())
                };
                eprintln!(
                    "RANK_UPDATE: depth={} card={} suit={} old_min={} new_min={} suit_rank_winners={} play=[{}]",
                    depth, card_name(card), suit, old_min, min_relevant_ranks[suit], winner_str, self.format_play_sequence(depth)
                );
            }

            // C++ behavior: after trying a card, order remaining playable cards
            if !remaining_playable.is_empty() {
                self.order_cards(&mut ordered_cards, depth, remaining_playable, beta);
                remaining_playable = Cards::new();
            }

            i += 1;
        }

        self.plays[depth].equivalence_runs = runs;
        SearchResult {
            ns_tricks: best,
            rank_winners,
        }
    }

    /// Play a card and continue search
    fn play_card_and_search(&mut self, depth: usize, card: usize, beta: i8) -> SearchResult {
        let trick_idx = depth / 4;
        let card_in_trick = depth & 3;
        let seat_to_play = self.plays[depth].seat_to_play;

        self.apply_play(depth, card);

        // Recurse
        let mut result = self.search_with_cache(depth + 1, beta);

        // At the end of a trick the winning card is a rank winner if another
        // card of its suit was played to the trick (C++ GetTrickRankWinner).
        // The trick's cards are what it started with less what the hands
        // still hold, this card included: it is restored below.
        if card_in_trick == 3 {
            let winning_card = self.plays[self.plays[depth].winning_play].card_played;
            let played = self.tricks[trick_idx]
                .all_cards
                .different(self.hands.all_cards());
            if played.suit(suit_of(winning_card)).size() > 1 {
                result.rank_winners.add(winning_card);
            }
        }

        #[cfg(feature = "debug_search")]
        let before_restore = self.hands[seat_to_play].size();

        // Restore hand
        self.hands[seat_to_play].add(card);

        #[cfg(feature = "debug_search")]
        {
            let after_restore = self.hands[seat_to_play].size();
            if after_restore != before_restore + 1 {
                eprintln!(
                    "ERROR: add didn't work! seat={} card={} before={} after={}",
                    seat_to_play,
                    card_name(card),
                    before_restore,
                    after_restore
                );
            }
        }

        #[cfg(feature = "debug_search")]
        eprintln!(
            "  depth={} {} played {} result={}",
            depth,
            seat_to_play,
            card_name(card),
            result.ns_tricks
        );

        result
    }

    /// IsEquivalent check matching C++ Trick::IsEquivalent
    /// If `card` is equivalent to one of the tried cards, the rank positions
    /// from the higher of the two down to just above the lower one (never
    /// empty); else no cards. Bob Salita's `EquivalentRun`, which returns the
    /// run where `IsEquivalent` returned whether there was one.
    fn equivalent_run(
        &self,
        card: usize,
        tried_suit: Cards,
        my_hand: Cards,
        all_cards: Cards,
    ) -> Cards {
        let mut result = false;
        let mut run = Cards::new();

        if !tried_suit.is_empty() {
            let suit = suit_of(card);
            let all_suit = all_cards.suit(suit);
            let my_suit = my_hand.suit(suit);

            // Check above (higher-ranked tried cards)
            let above = tried_suit.slice(0, card);
            if !above.is_empty() {
                let closest_above = above.bottom();
                let between_all = all_suit.slice(closest_above + 1, card);
                let between_my = my_suit.slice(closest_above + 1, card);
                if between_all == between_my {
                    result = true;
                    run = Cards::from_bits(!0).slice(closest_above, card);
                }
            }

            // Check below (lower-ranked tried cards)
            if !result {
                let below = tried_suit.slice(card + 1, NUM_SUITS * NUM_RANKS);
                if !below.is_empty() {
                    let closest_below = below.top();
                    let between_all = all_suit.slice(card + 1, closest_below);
                    let between_my = my_suit.slice(card + 1, closest_below);
                    if between_all == between_my {
                        result = true;
                        run = Cards::from_bits(!0).slice(card, closest_below);
                    }
                }
            }
        }

        // EQUIV logging
        if xray_should_log() {
            let suit = suit_of(card);
            let all_suit = all_cards.suit(suit);
            let my_suit = my_hand.suit(suit);
            eprintln!(
                "EQUIV_V2: card={} tried=0x{:x} hand=0x{:x} all=0x{:x} -> {}",
                card_name(card),
                tried_suit.value(),
                my_suit.value(),
                all_suit.value(),
                if result { "true" } else { "false" }
            );
        }

        run
    }

    /// Bob Salita's `SecondSeatSureTricks` (b12bb3f), from DDS's
    /// `QuickTricksSecondHand`. Second to play to the trick, our side surely
    /// wins it: by a ruff the third hand cannot overruff, or by a card above
    /// the lead and all of the third hand's cards in the suit. At no-trump the
    /// winner then cashes its top cards. The bound, if that is enough for a
    /// cut.
    ///
    /// The rank winners stand on their own: the claim holds wherever the cards
    /// from the top of each suit down to them are in the same hands, so it
    /// relies on no equivalence run.
    fn second_seat_sure_tricks(&self, depth: usize, beta: i8) -> Option<SearchResult> {
        let seat = self.plays[depth].seat_to_play;
        let ns_tricks_won = self.plays[depth].ns_tricks_won as isize;
        let lead = self.plays[depth - 1].card_played;
        let suit = suit_of(lead);
        let my_hand = self.hands[seat];
        let pd_hand = self.hands[partner(seat)];
        let third_hand = self.hands[left_hand_opp(seat)];
        let my_suit = my_hand.suit(suit);
        let pd_suit = pd_hand.suit(suit);
        let third_suit = third_hand.suit(suit);
        let trump = self.trump;
        let side_suit = trump < NOTRUMP && suit != trump;
        if side_suit && third_suit.is_empty() && !third_hand.suit(trump).is_empty() {
            return None; // third hand ruffs
        }
        let mut rank_winners = Cards::new();
        let ours = my_suit.union(pd_suit);
        let we_ruff = side_suit
            && ((my_suit.is_empty() && !my_hand.suit(trump).is_empty())
                || (pd_suit.is_empty() && !pd_hand.suit(trump).is_empty()));
        if !we_ruff {
            // We ruff and the third hand follows suit or has no trump: no
            // rank matters. Otherwise our top card must beat the lead and the
            // third hand.
            if ours.is_empty() {
                return None;
            }
            let top = ours.top();
            if !higher_rank(top, lead)
                || (!third_suit.is_empty() && !higher_rank(top, third_suit.top()))
            {
                return None;
            }
            rank_winners.add(top);
        }
        let remaining = my_hand.size() as isize;
        let beta = beta as isize;
        let needed = if is_ns(seat) {
            beta - ns_tricks_won
        } else {
            remaining - (beta - ns_tricks_won) + 1
        };
        let mut sure_tricks = 1;
        if sure_tricks < needed {
            if trump < NOTRUMP {
                return None;
            }
            // No-trump: the hand that wins this trick leads to the next one
            // and cashes.
            let winner_hand = if pd_suit.is_empty()
                || (!my_suit.is_empty() && higher_rank(my_suit.top(), pd_suit.top()))
            {
                my_hand
            } else {
                pd_hand
            };
            let all_cards = self.tricks[depth / 4].all_cards; // at the trick start
            let all_suit = all_cards.suit(suit);
            let top = all_suit.top();
            if winner_hand.have(top) {
                let mut rest = all_suit;
                rest.remove(top);
                if !rest.is_empty() && winner_hand.have(rest.top()) {
                    sure_tricks += 1;
                    rank_winners.add(rest.top());
                }
            }
            for s in 0..NUM_SUITS {
                if sure_tricks >= needed {
                    break;
                }
                if s == suit {
                    continue;
                }
                let hand_suit = winner_hand.suit(s);
                if hand_suit.is_empty() {
                    continue;
                }
                // Only the lead has been played, so the other suits are as at
                // the trick start.
                let all_s = all_cards.suit(s);
                if all_s == hand_suit {
                    sure_tricks += hand_suit.size() as isize;
                } else if hand_suit.have(all_s.top()) {
                    sure_tricks += 1;
                    rank_winners.add(all_s.top());
                }
            }
            if sure_tricks < needed {
                return None;
            }
        }
        let ns_tricks = if is_ns(seat) {
            ns_tricks_won + sure_tricks
        } else {
            ns_tricks_won + remaining - sure_tricks
        };
        Some(SearchResult {
            ns_tricks: ns_tricks as u8,
            rank_winners,
        })
    }

    /// Bob Salita's `LiftRunWinner` (b12bb3f). At a cut, the card played
    /// could as well have been the top card of the run of equivalent cards it
    /// belongs to: the play after it is the same with the run's cards one
    /// place lower. If the card won the trick as the lowest rank winner of its
    /// suit and that other proof has higher rank winners, returns them.
    fn lift_run_winner(&self, depth: usize, card: usize, rank_winners: Cards) -> Cards {
        if !rank_winners.have(card) {
            return rank_winners;
        }
        let suit = suit_of(card);
        if rank_winners.suit(suit).bottom() != card {
            return rank_winners;
        }
        let suit_cards = self.tricks[depth / 4].all_cards.suit(suit);
        let higher = suit_cards.slice(suit * NUM_RANKS, card);
        let others = higher.different(self.hands[self.plays[depth].seat_to_play]);
        let run_above = if others.is_empty() {
            higher
        } else {
            higher.slice(others.bottom() + 1, card)
        };
        // No run, or its card just above this one would move onto it: nothing
        // to gain.
        if run_above.is_empty() || rank_winners.have(run_above.bottom()) {
            return rank_winners;
        }
        let mut shifted = Cards::new();
        shifted.add(run_above.top());
        for c in rank_winners.intersect(run_above).iter() {
            shifted.add(suit_cards.slice(c + 1, 64).top());
        }
        let mut lifted = rank_winners.different(run_above);
        lifted.remove(card);
        lifted.union(shifted)
    }

    /// The runs the result of the play after `depth` relies on within this
    /// trick: none if that play starts the next trick.
    fn next_runs(&self, depth: usize) -> Cards {
        if depth & 3 == 3 {
            Cards::new()
        } else {
            self.plays[depth + 1].equivalence_runs
        }
    }

    /// Check if card1 beats card2
    #[inline]
    fn wins_over(&self, c1: usize, c2: usize, _lead_suit: usize) -> bool {
        let s1 = suit_of(c1);
        let s2 = suit_of(c2);

        if s1 == s2 {
            return higher_rank(c1, c2);
        }

        if self.trump < NOTRUMP {
            if s1 == self.trump {
                return true;
            }
            if s2 == self.trump {
                return false;
            }
        }

        false
    }

    /// Collect last trick (optimization for single remaining trick)
    fn collect_last_trick(&self, depth: usize) -> SearchResult {
        let ns_tricks_won = self.plays[depth].ns_tricks_won;
        let seat_to_play = self.plays[depth].seat_to_play;

        let mut winning_card = self.hands[seat_to_play].top();
        let mut winning_seat = seat_to_play;

        for i in 1..4 {
            let seat = (seat_to_play + i) % NUM_SEATS;
            let card = self.hands[seat].top();
            if self.wins_over(card, winning_card, suit_of(self.hands[seat_to_play].top())) {
                winning_card = card;
                winning_seat = seat;
            }
        }

        let ns_tricks = if is_ns(winning_seat) {
            ns_tricks_won + 1
        } else {
            ns_tricks_won
        };

        // The winning card is a rank winner only if another card of its suit
        // is in the trick (C++ CollectLastTrick). The hands hold exactly the
        // last trick's four cards.
        let mut rank_winners = Cards::new();
        if self.hands.all_cards().suit(suit_of(winning_card)).size() > 1 {
            rank_winners.add(winning_card);
        }

        SearchResult {
            ns_tricks,
            rank_winners,
        }
    }

    /// Order cards for move ordering - calls order_leads or order_follows
    fn order_cards_static(
        ordered_cards: &mut OrderedCards,
        depth: usize,
        playable: Cards,
        plays: &[PlayState; TOTAL_CARDS],
        tricks: &[Trick; TOTAL_TRICKS],
        hands: &Hands,
        trump: usize,
    ) {
        use super::bridge_solver::{order_follows, order_leads};

        let trick_idx = depth / 4;
        let card_in_trick = depth & 3;
        let seat_to_play = plays[depth].seat_to_play;
        let all_cards = tricks[trick_idx].all_cards;

        if card_in_trick == 0 {
            // Leading - use order_leads
            let ordered = order_leads(playable, hands, seat_to_play, trump, all_cards);
            for i in 0..ordered.len() {
                ordered_cards.add(ordered.card(i));
            }
        } else {
            // Following - use order_follows
            let lead_suit = tricks[trick_idx].lead_suit;
            let winning_play_idx = plays[depth - 1].winning_play;
            let winning_seat = plays[winning_play_idx].seat_to_play;
            let winning_card = plays[winning_play_idx].card_played;

            // Create a closure for wins_over that captures trump and lead_suit
            let wins_over = |c1: usize, c2: usize| -> bool {
                // Card 1 wins over card 2 if:
                // 1. Both same suit and c1 is higher (lower index = higher rank within suit)
                // 2. c1 is trump and c2 is not trump
                let s1 = suit_of(c1);
                let s2 = suit_of(c2);
                if s1 == s2 {
                    c1 < c2 // Lower card index = higher rank
                } else if s1 == trump {
                    true // c1 is trump, c2 is not
                } else {
                    false // c1 is not trump, can't beat c2
                }
            };

            let ordered = order_follows(
                playable,
                hands,
                seat_to_play,
                trump,
                lead_suit,
                winning_seat,
                winning_card,
                card_in_trick,
                wins_over,
            );
            for i in 0..ordered.len() {
                ordered_cards.add(ordered.card(i));
            }
        }
    }

    /// Count fast tricks for a suit, properly handling entries and blocking.
    ///
    /// `max_rank_winners` is the longest holding in the suit among the other
    /// three hands (partner's and both opponents'), before any trimming.
    fn suit_fast_tricks(
        my_suit: Cards,
        my_winners: usize,
        max_rank_winners: usize,
        pd_suit: Cards,
        pd_winners: usize,
        pd_entry: &mut bool,
    ) -> usize {
        // Entry from partner if my top winner can cover partner's bottom card.
        if !pd_suit.is_empty() && my_winners > 0 && higher_rank(my_suit.top(), pd_suit.bottom()) {
            *pd_entry = true;
        }
        if pd_winners == 0 {
            // My winners outlast everyone else's cards, so my small cards win
            // too and the lead stays with me (upstream 3828d36).
            if my_winners >= max_rank_winners {
                return my_suit.size();
            }
            // Partner has no winners.
            return my_winners;
        }
        // Cash all my winners, then partner's - but only if I have cards to lead
        if my_winners == 0 {
            return if !my_suit.is_empty() { pd_winners } else { 0 };
        }
        // Suit blocked by partner (my top is lower than partner's bottom)
        if !pd_suit.is_empty() && lower_rank(my_suit.top(), pd_suit.bottom()) {
            return pd_winners;
        }
        // Suit blocked by me (my bottom is higher than partner's top)
        if !pd_suit.is_empty() && higher_rank(my_suit.bottom(), pd_suit.top()) {
            return my_winners;
        }
        // If partner has no small cards, treat one winner as a small card
        let adjusted_pd_winners = if pd_winners == pd_suit.size() && pd_winners > 0 {
            pd_winners - 1
        } else {
            pd_winners
        };
        my_suit.size().min(my_winners + adjusted_pd_winners)
    }

    /// Count top trump tricks for our side, returning (count, rank_winners)
    fn top_trump_tricks_our_side(&self, seat: Seat, all_cards: Cards) -> (usize, Cards) {
        let my_trumps = self.hands[seat].suit(self.trump);
        let pd_trumps = self.hands[partner(seat)].suit(self.trump);
        let all_trumps = all_cards.suit(self.trump);

        // If we have all the trumps
        if my_trumps == all_trumps {
            return (my_trumps.size(), Cards::new());
        }
        if pd_trumps == all_trumps {
            return (pd_trumps.size(), Cards::new());
        }

        let both_trumps = my_trumps.union(pd_trumps);
        let max_trump_tricks = my_trumps.size().max(pd_trumps.size());
        let mut sure_tricks = 0;
        let mut rank_winners = Cards::new();

        // Count consecutive top trumps held by our side
        for card in all_trumps.iter() {
            if both_trumps.have(card) && sure_tricks < max_trump_tricks {
                sure_tricks += 1;
                rank_winners.add(card);
            } else {
                break;
            }
        }

        (sure_tricks, rank_winners)
    }

    /// Count guaranteed fast tricks from a given seat's perspective.
    /// Returns (count, rank_winners) matching C++ FastTricks().
    /// Also returns the side-suit tricks alone and their rank winners, which
    /// Bob Salita's trump-rank bound builds on.
    ///
    /// Only the suits in `suit_mask` (bit per suit) are counted, which Bob
    /// Salita's rank-winner minimisation uses to drop suits a cut can spare.
    fn fast_tricks_from_seat(
        &self,
        seat: Seat,
        all_cards: Cards,
        suit_mask: u8,
    ) -> (usize, Cards, usize, Cards) {
        let my_hand = self.hands[seat];
        let pd_hand = self.hands[partner(seat)];
        let lho_hand = self.hands[left_hand_opp(seat)];
        let rho_hand = self.hands[right_hand_opp(seat)];

        // Count top trump tricks for our side in trump contracts
        let (mut trump_tricks, mut rank_winners) = if self.trump < NOTRUMP {
            self.top_trump_tricks_our_side(seat, all_cards)
        } else {
            (0, Cards::new())
        };

        let mut pd_rank_winners = Cards::new();
        let mut my_tricks = 0;
        let mut pd_tricks = 0;
        let mut my_entry = false;
        let mut pd_entry = false;

        for suit in 0..NUM_SUITS {
            // Skip trump suit in trump contracts, and suits left out
            if (self.trump < NOTRUMP && suit == self.trump) || suit_mask & (1 << suit) == 0 {
                continue;
            }

            let mut my_suit = my_hand.suit(suit);
            let mut pd_suit = pd_hand.suit(suit);
            let lho_suit = lho_hand.suit(suit);
            let rho_suit = rho_hand.suit(suit);
            let all_suit = all_cards.suit(suit);

            if my_suit.is_empty() && pd_suit.is_empty() {
                continue;
            }

            // Compute max rank winners for each hand (matches C++ logic)
            let my_max_rank_winners = pd_suit.size().max(lho_suit.size()).max(rho_suit.size());
            let pd_max_rank_winners = my_suit.size().max(lho_suit.size()).max(rho_suit.size());

            // In trump contracts, limit side suit winners to opponent's suit length
            // If opponents have trumps, they can ruff once they run out of the suit
            if self.trump < NOTRUMP {
                let mut max_suit_winners = self.num_tricks; // TOTAL_TRICKS equivalent
                if !lho_hand.suit(self.trump).is_empty() {
                    max_suit_winners = lho_suit.size();
                }
                if !rho_hand.suit(self.trump).is_empty() {
                    max_suit_winners = max_suit_winners.min(rho_suit.size());
                }
                // Truncate our suit holdings to max_suit_winners
                while my_suit.size() > max_suit_winners {
                    my_suit.remove(my_suit.bottom());
                }
                while pd_suit.size() > max_suit_winners {
                    pd_suit.remove(pd_suit.bottom());
                }
            }

            // Count winners for each hand and track rank winners
            let mut my_winners = 0;
            let mut pd_winners = 0;
            for card in all_suit.iter() {
                if my_suit.have(card) {
                    my_winners += 1;
                    if my_winners <= my_max_rank_winners {
                        rank_winners.add(card);
                    }
                } else if pd_suit.have(card) {
                    pd_winners += 1;
                    if pd_winners <= pd_max_rank_winners {
                        pd_rank_winners.add(card);
                    }
                } else {
                    break;
                }
            }

            my_tricks += Self::suit_fast_tricks(
                my_suit,
                my_winners,
                my_max_rank_winners,
                pd_suit,
                pd_winners,
                &mut my_entry,
            );
            pd_tricks += Self::suit_fast_tricks(
                pd_suit,
                pd_winners,
                pd_max_rank_winners,
                my_suit,
                my_winners,
                &mut pd_entry,
            );
        }

        let side_suit_tricks = if pd_entry {
            rank_winners = rank_winners.union(pd_rank_winners);
            my_tricks.max(pd_tricks)
        } else {
            my_tricks
        };
        let side_rank_winners = if self.trump < NOTRUMP {
            rank_winners.different(Cards::from_bits(mask_of(self.trump)))
        } else {
            rank_winners
        };

        // Upstream cb5b700: the trump-length count can replace the top-trump
        // count here too, if the longer trump hand holds enough other cards to
        // follow or discard on every side-suit trick without spending a trump;
        // its length tricks then come afterwards. The count depends only on
        // trump lengths, so the trumps leave the rank winners.
        if let Some(length) = self.trump_length_tricks(
            seat,
            partner(seat),
            left_hand_opp(seat),
            right_hand_opp(seat),
        ) {
            let my_trumps = my_hand.suit(self.trump).size();
            let pd_trumps = pd_hand.suit(self.trump).size();
            let max_trumps = my_trumps.max(pd_trumps);
            let my_keeps =
                my_trumps == max_trumps && my_hand.size() - my_trumps >= side_suit_tricks;
            let pd_keeps =
                pd_trumps == max_trumps && pd_hand.size() - pd_trumps >= side_suit_tricks;
            if length >= trump_tricks && (my_keeps || pd_keeps) {
                trump_tricks = length;
                rank_winners = rank_winners.different(all_cards.suit(self.trump));
            }
        }

        // Uncapped. The reference caps at the call site, and reports the
        // number from *before* that cap as `raw`; capping here made our `raw`
        // mean something different from theirs, which read as a divergence in
        // five of the seven fixtures when the two searches in fact agreed.
        (
            trump_tricks + side_suit_tricks,
            rank_winners,
            side_suit_tricks,
            side_rank_winners,
        )
    }

    /// Fast tricks estimation - returns (count, rank_winners, side-suit
    /// tricks, their rank winners)
    fn fast_tricks(&self, depth: usize) -> (usize, Cards, usize, Cards) {
        let seat_to_play = self.plays[depth].seat_to_play;
        let all_cards = self.hands.all_cards();
        let trick_idx = depth / 4;
        let max_tricks = self.num_tricks - trick_idx;
        let (raw, rank_winners, side_tricks, side_rank_winners) =
            self.fast_tricks_from_seat(seat_to_play, all_cards, ALL_SUITS);
        // `min(raw, my_hand.Size())` in the reference. The further clamp to the
        // tricks left in the deal cannot bite -- a hand never holds more cards
        // than there are tricks remaining -- but it is kept as a guard.
        let capped = raw.min(self.hands[seat_to_play].size());
        let result = capped.min(max_tricks);

        // Debug logging when XRAY is enabled and under limit
        if xray_should_log() {
            eprintln!(
                "FAST_TRICKS: depth={} seat={} raw={} capped={} trump={}",
                depth, seat_to_play, raw, capped, self.trump
            );
        }
        (result, rank_winners, side_tricks, side_rank_winners)
    }

    /// Top trump tricks for opponents (trump contracts only)
    /// Counts guaranteed top trump tricks for the opponents (LHO + RHO)
    /// Returns (count, rank_winners) matching C++ TopTrumpTricks
    fn top_trump_tricks_opponent(&self, depth: usize) -> (usize, Cards) {
        let seat_to_play = self.plays[depth].seat_to_play;
        let lho_trumps = self.hands[left_hand_opp(seat_to_play)].suit(self.trump);
        let rho_trumps = self.hands[right_hand_opp(seat_to_play)].suit(self.trump);
        let all_trumps = self.hands.all_cards().suit(self.trump);

        // If one opponent has all the trumps
        if lho_trumps == all_trumps {
            return (lho_trumps.size(), Cards::new());
        }
        if rho_trumps == all_trumps {
            return (rho_trumps.size(), Cards::new());
        }

        let both_trumps = lho_trumps.union(rho_trumps);
        let max_trump_tricks = lho_trumps.size().max(rho_trumps.size());
        let mut sure_tricks = 0;
        let mut rank_winners = Cards::new();

        // Count consecutive top trumps held by opponents
        for card in all_trumps.iter() {
            if both_trumps.have(card) && sure_tricks < max_trump_tricks {
                sure_tricks += 1;
                rank_winners.add(card);
            } else {
                break;
            }
        }

        (sure_tricks, rank_winners)
    }

    /// Slow trump tricks for opponents - detects finesse positions
    /// Returns (1, rank_winners) if opponents have a guaranteed slow trump trick via finesse
    /// Returns (count, rank_winners) matching C++ SlowTrumpTricks
    /// `SlowTrumpTricks` from the opponents' perspective, for slow-trick
    /// pruning. `leading` is false, matching the C++ call site.
    fn slow_trump_tricks_opponent(&self, depth: usize) -> (usize, Cards) {
        let s = self.plays[depth].seat_to_play;
        self.slow_trump_tricks_roles(
            depth,
            left_hand_opp(s),
            right_hand_opp(s),
            partner(s),
            s,
            false,
        )
    }

    /// `SlowTrumpTricks` from the perspective of the side to play, used to
    /// upgrade a zero fast-trick count. `leading` is true, matching the C++.
    fn slow_trump_tricks_own(&self, depth: usize) -> (usize, Cards) {
        let s = self.plays[depth].seat_to_play;
        self.slow_trump_tricks_roles(
            depth,
            s,
            partner(s),
            left_hand_opp(s),
            right_hand_opp(s),
            true,
        )
    }

    /// `SlowTrumpTricks` with its four roles given explicitly.
    ///
    /// The C++ calls this two ways and the role mapping is the entire
    /// difference between them, so it is a parameter here rather than baked
    /// into the body.
    #[allow(clippy::too_many_arguments)]
    fn slow_trump_tricks_roles(
        &self,
        depth: usize,
        my: Seat,
        pd: Seat,
        lho: Seat,
        rho: Seat,
        leading: bool,
    ) -> (usize, Cards) {
        let trump = self.trump;
        slow_trump_tricks(
            self.hands.all_cards().suit(trump),
            self.hands[my].suit(trump),
            self.hands[pd].suit(trump),
            self.hands[lho].suit(trump),
            self.hands[rho].suit(trump),
            self.num_tricks - (depth / 4),
            leading,
        )
    }

    /// Bob Salita's `MinimizeFastTricks` (b12bb3f): given fast tricks that
    /// cut needing only `need` of them, look for the same cut with fewer rank
    /// winners. Returns whether it changed `tricks` and `rank_winners`.
    fn minimize_fast_tricks(
        &self,
        depth: usize,
        need: isize,
        with_trumps: bool,
        tricks: &mut usize,
        rank_winners: &mut Cards,
    ) -> bool {
        let seat = self.plays[depth].seat_to_play;
        // Trump length tricks need no rank winners.
        if with_trumps {
            let len = |s: Seat| self.hands[s].suit(self.trump).size() as isize;
            let length_tricks = len(seat).max(len(partner(seat)))
                - len(left_hand_opp(seat))
                - len(right_hand_opp(seat));
            if length_tricks >= need {
                *tricks = length_tricks as usize;
                *rank_winners = Cards::new();
                return true;
            }
        }
        // The top cards of one suit.
        if self.single_suit_fast_tricks(depth, need, rank_winners) {
            *tricks = need as usize;
            return true;
        }
        if *tricks as isize == need {
            return false;
        }
        // Drop the suits the cut can do without, the one with the lowest rank
        // winner first.
        let mut suit_mask = ALL_SUITS;
        let mut tried_mask = 0u8;
        let mut reduced = false;
        loop {
            let mut drop_suit = None;
            let mut lowest_rank = NUM_RANKS;
            for suit in 0..NUM_SUITS {
                if suit == self.trump
                    || tried_mask & (1 << suit) != 0
                    || rank_winners.suit(suit).is_empty()
                {
                    continue;
                }
                let rank = rank_of(rank_winners.suit(suit).bottom());
                if rank < lowest_rank {
                    lowest_rank = rank;
                    drop_suit = Some(suit);
                }
            }
            let Some(drop_suit) = drop_suit else {
                break;
            };
            tried_mask |= 1 << drop_suit;
            let (fewer, fewer_rank_winners) =
                self.fast_tricks_masked(depth, suit_mask & !(1 << drop_suit));
            if (fewer as isize) < need {
                continue;
            }
            suit_mask &= !(1 << drop_suit);
            *tricks = fewer;
            *rank_winners = fewer_rank_winners;
            reduced = true;
            if fewer as isize == need {
                break;
            }
        }
        reduced
    }

    /// Bob Salita's `SingleSuitFastTricks` (b12bb3f): whether the leader can
    /// cash `need` tricks from the top of one suit with fewer rank winners
    /// than `rank_winners`; if so, sets them.
    fn single_suit_fast_tricks(&self, depth: usize, need: isize, rank_winners: &mut Cards) -> bool {
        let seat = self.plays[depth].seat_to_play;
        let all_cards = self.tricks[depth / 4].all_cards;
        let my_hand = self.hands[seat];
        let pd_hand = self.hands[partner(seat)];
        let lho_hand = self.hands[left_hand_opp(seat)];
        let rho_hand = self.hands[right_hand_opp(seat)];
        let suit_contract = self.trump < NOTRUMP;
        let lho_has_trump = suit_contract && !lho_hand.suit(self.trump).is_empty();
        let rho_has_trump = suit_contract && !rho_hand.suit(self.trump).is_empty();
        let mut best_winners = Cards::new();
        let mut best_score: isize = -1;
        for suit in 0..NUM_SUITS {
            let my_suit = my_hand.suit(suit);
            if (my_suit.size() as isize) < need {
                continue;
            }
            if suit != self.trump {
                // An opponent with trumps must follow suit to every trick.
                let mut max_suit_winners = TOTAL_TRICKS;
                if lho_has_trump {
                    max_suit_winners = lho_hand.suit(suit).size();
                }
                if rho_has_trump {
                    max_suit_winners = max_suit_winners.min(rho_hand.suit(suit).size());
                }
                if (max_suit_winners as isize) < need {
                    continue;
                }
            }
            // Once the others have no card left in the suit, ranks no longer
            // matter.
            let max_rank_winners = pd_hand
                .suit(suit)
                .size()
                .max(lho_hand.suit(suit).size().max(rho_hand.suit(suit).size()));
            let mut winners = 0isize;
            let mut lowest_winner = None;
            let mut suit_winners = Cards::new();
            for card in all_cards.suit(suit).iter() {
                if winners == need || !my_suit.have(card) {
                    break;
                }
                winners += 1;
                if winners as usize <= max_rank_winners {
                    suit_winners.add(card);
                    lowest_winner = Some(card);
                }
            }
            if winners < need {
                continue;
            }
            // Fewest rank winners, then the highest card.
            let score = (TOTAL_TRICKS - suit_winners.size()) as isize * 16
                + lowest_winner.map_or(15, |card| rank_of(card) as isize);
            if score > best_score {
                best_score = score;
                best_winners = suit_winners;
            }
        }
        if best_score < 0 || best_winners.size() >= rank_winners.size() {
            return false;
        }
        *rank_winners = best_winners;
        true
    }

    /// C++ `FastTricks(suit_mask)`: the fast tricks counting only the suits in
    /// `suit_mask`, capped as [`Self::fast_tricks`] caps them.
    fn fast_tricks_masked(&self, depth: usize, suit_mask: u8) -> (usize, Cards) {
        let seat = self.plays[depth].seat_to_play;
        let (raw, rank_winners, _, _) =
            self.fast_tricks_from_seat(seat, self.hands.all_cards(), suit_mask);
        (raw.min(self.hands[seat].size()), rank_winners)
    }

    /// Bob Salita's `FastTricksNoTrump` (b12bb3f): the fast tricks of a
    /// no-trump position given by four hands, `my_hand` on lead.
    fn fast_tricks_no_trump(
        my_hand: Cards,
        pd_hand: Cards,
        lho_hand: Cards,
        rho_hand: Cards,
    ) -> (usize, Cards) {
        let all_cards = my_hand.union(pd_hand).union(lho_hand).union(rho_hand);
        let mut rank_winners = Cards::new();
        let mut pd_rank_winners = Cards::new();
        let (mut my_tricks, mut pd_tricks) = (0, 0);
        let (mut my_entry, mut pd_entry) = (false, false);
        for suit in 0..NUM_SUITS {
            let my_suit = my_hand.suit(suit);
            let pd_suit = pd_hand.suit(suit);
            let opp_max = lho_hand.suit(suit).size().max(rho_hand.suit(suit).size());
            let my_max_rank_winners = pd_suit.size().max(opp_max);
            let pd_max_rank_winners = my_suit.size().max(opp_max);
            let (mut my_winners, mut pd_winners) = (0, 0);
            for card in all_cards.suit(suit).iter() {
                if my_suit.have(card) {
                    my_winners += 1;
                    if my_winners <= my_max_rank_winners {
                        rank_winners.add(card);
                    }
                } else if pd_suit.have(card) {
                    pd_winners += 1;
                    if pd_winners <= pd_max_rank_winners {
                        pd_rank_winners.add(card);
                    }
                } else {
                    break;
                }
            }
            my_tricks += Self::suit_fast_tricks(
                my_suit,
                my_winners,
                my_max_rank_winners,
                pd_suit,
                pd_winners,
                &mut my_entry,
            );
            pd_tricks += Self::suit_fast_tricks(
                pd_suit,
                pd_winners,
                pd_max_rank_winners,
                my_suit,
                my_winners,
                &mut pd_entry,
            );
        }
        let mut fast_tricks = my_tricks;
        if pd_entry {
            fast_tricks = my_tricks.max(pd_tricks);
            rank_winners = rank_winners.union(pd_rank_winners);
        }
        (fast_tricks.min(my_hand.size()), rank_winners)
    }

    /// Bob Salita's `DrawTrumpsWithRun` (b12bb3f): draw the opponents' trumps
    /// in `rounds` rounds with the top trumps `run`, all ours whichever of us
    /// holds them, then count the no-trump fast tricks of the hand left on
    /// lead. Each round the hand on lead leads its lowest trump of the run, or
    /// its lowest trump to partner's lowest of the run.
    fn draw_trumps_with_run(&self, depth: usize, run: Cards, rounds: usize) -> (usize, Cards) {
        let seat = self.plays[depth].seat_to_play;
        let trump = self.trump;
        let mut hand = [self.hands[seat], self.hands[partner(seat)]];
        let mut discards = [0usize; 2];
        let mut leader = 0;
        for _ in 0..rounds {
            let follower = 1 - leader;
            let lead_trumps = hand[leader].suit(trump);
            let follow_trumps = hand[follower].suit(trump);
            if lead_trumps.is_empty() {
                return (0, Cards::new());
            }
            if follow_trumps.is_empty() {
                discards[follower] += 1;
            }
            let lead_run = lead_trumps.intersect(run);
            if !lead_run.is_empty() {
                let lead_card = lead_run.bottom();
                hand[leader].remove(lead_card);
                if !follow_trumps.is_empty() {
                    let follow_card = follow_trumps.bottom();
                    hand[follower].remove(follow_card);
                    // The follower has only higher trumps: it wins the trick.
                    if higher_rank(follow_card, lead_card) {
                        leader = follower;
                    }
                }
            } else {
                let follow_run = follow_trumps.intersect(run);
                if follow_run.is_empty() {
                    return (0, Cards::new());
                }
                hand[leader].remove(lead_trumps.bottom());
                hand[follower].remove(follow_run.bottom());
                leader = follower;
            }
        }
        // A hand out of trumps discarded the lowest cards of its longest side
        // suits.
        for h in 0..2 {
            while discards[h] > 0 {
                discards[h] -= 1;
                let mut longest = None;
                let mut length = 0;
                for suit in 0..NUM_SUITS {
                    if suit != trump && hand[h].suit(suit).size() > length {
                        length = hand[h].suit(suit).size();
                        longest = Some(suit);
                    }
                }
                let Some(longest) = longest else {
                    return (0, Cards::new());
                };
                let bottom = hand[h].suit(longest).bottom();
                hand[h].remove(bottom);
            }
        }
        let lho_hand = self.hands[left_hand_opp(seat)].clear_suit(trump);
        let rho_hand = self.hands[right_hand_opp(seat)].clear_suit(trump);
        let (tricks, rank_winners) =
            Self::fast_tricks_no_trump(hand[leader], hand[1 - leader], lho_hand, rho_hand);
        (
            (rounds + tricks).min(self.hands[seat].size()),
            rank_winners.union(run),
        )
    }

    /// Bob Salita's `DrawTrumpsThenCash` (b12bb3f): we hold the top trumps
    /// needed to draw all the opponents' trumps, so draw them; the rest is a
    /// no-trump position in which our other trumps are winners.
    fn draw_trumps_then_cash(&self, depth: usize) -> (usize, Cards) {
        let seat = self.plays[depth].seat_to_play;
        let trump = self.trump;
        let rounds = self.hands[left_hand_opp(seat)]
            .suit(trump)
            .size()
            .max(self.hands[right_hand_opp(seat)].suit(trump).size());
        if rounds == 0 {
            return (0, Cards::new());
        }
        let ours = self.hands[seat]
            .suit(trump)
            .union(self.hands[partner(seat)].suit(trump));
        let all_trumps = self.tricks[depth / 4].all_cards.suit(trump);
        let run = keep_top(all_trumps, rounds);
        if !ours.include(run) {
            return (0, Cards::new());
        }
        let result = self.draw_trumps_with_run(depth, run, rounds);
        if result.0 > 0 {
            return result;
        }
        // Two of the top trumps may fall together: retry with up to twice as
        // many.
        let mut long_run = Cards::new();
        for (count, card) in all_trumps.iter().enumerate() {
            if !ours.have(card) || count == 2 * rounds {
                break;
            }
            long_run.add(card);
        }
        if long_run == run {
            return (0, Cards::new());
        }
        self.draw_trumps_with_run(depth, long_run, rounds)
    }

    /// Bob Salita's `TrumpRankTricks` (b12bb3f): sure trump tricks of one
    /// hand, holding `trumps`, by rank, whoever leads. Each trump the hand
    /// plays wins its trick unless an opponent plays a higher trump on it, and
    /// an opposing trump does that once. Pins as few top trumps as give
    /// `wanted` tricks -- below the pinned cards any opposing trump may beat
    /// any of the hand's -- and returns the tricks and the pinned cards, or
    /// `(0, none)`. With no card pinned this is the trump-length count.
    fn trump_rank_tricks(
        &self,
        depth: usize,
        trumps: Cards,
        opp_trumps: Cards,
        wanted: isize,
    ) -> (usize, Cards) {
        let n = trumps.size() as isize;
        if wanted <= 0 || n < wanted || n - (trump_kills(trumps, opp_trumps) as isize) < wanted {
            return (0, Cards::new());
        }
        let all_trumps = self.tricks[depth / 4].all_cards.suit(self.trump);
        let num_opp = opp_trumps.size() as isize;
        let mut rest = trumps;
        for i in 1..=n {
            let card = rest.top();
            rest.remove(card);
            let pinned = all_trumps.slice(0, card + 1);
            let mut pinned_opp = opp_trumps.intersect(pinned);
            let unpinned = n - i;
            let unpinned_opp = num_opp - pinned_opp.size() as isize;
            // Unpinned opposing trumps beat unpinned trumps only.
            let mut kills = unpinned.min(unpinned_opp);
            // Pinned opposing trumps beat the other unpinned trumps, then
            // lower pinned ones.
            let mut k = unpinned - kills;
            while k > 0 && !pinned_opp.is_empty() {
                pinned_opp.remove(pinned_opp.bottom());
                kills += 1;
                k -= 1;
            }
            kills += trump_kills(trumps.intersect(pinned), pinned_opp) as isize;
            if n - kills >= wanted {
                return ((n - kills) as usize, pinned);
            }
        }
        (0, Cards::new())
    }

    /// Bob Salita's `CannotTakeAllNoTrump` (b12bb3f): with no trumps out, the
    /// side of `a_seat`, on lead, cannot take all the remaining tricks, so the
    /// other side is sure of one. `(1, rank winners)` if so, else `(0, none)`.
    ///
    /// Every trick is won in the suit led, and a side wins a suit at most as
    /// often as the longer of its two holdings.
    /// 1. The other side keeps the top cards of the suits it tops, so the side
    ///    wins tricks only in the suits it tops itself: too short, and it fails.
    /// 2. With stoppers: in a suit the side tops, the other side's highest card
    ///    stops it if its holder plays after the hand with all the higher cards
    ///    (no finesse) and has a lower card for each of them; the side then
    ///    wins that suit only as often as it has higher cards. A hand of the
    ///    other side keeps a card for each top card or stopper in its charge,
    ///    and has spare cards for the side's winners while
    ///    winners + cards in charge <= tricks.
    ///
    /// The rank winners are the top cards of the other side's suits and, for a
    /// stopped suit, its cards down to the stopper.
    fn cannot_take_all_no_trump(&self, depth: usize, a_seat: Seat) -> (usize, Cards) {
        let all_cards = self.tricks[depth / 4].all_cards;
        let b_seat = [(a_seat + 1) % NUM_SEATS, (a_seat + 3) % NUM_SEATS];
        let a1 = self.hands[a_seat];
        let a2 = self.hands[partner(a_seat)];
        let a_cards = a1.union(a2);
        let num_tricks = self.hands[a_seat].size();
        let mut lengths = 0;
        let mut winners = 0;
        let mut in_charge = [0usize; 2];
        let mut top_rank_winners = Cards::new();
        let mut stopper_rank_winners = Cards::new();
        for suit in 0..NUM_SUITS {
            let all_suit = all_cards.suit(suit);
            if all_suit.is_empty() {
                continue;
            }
            let top = all_suit.top();
            if !a_cards.have(top) {
                let holder = if self.hands[b_seat[0]].have(top) {
                    0
                } else {
                    1
                };
                in_charge[holder] += 1;
                top_rank_winners.add(top);
                continue;
            }
            let a_suit = a_cards.suit(suit);
            let max_length = a1.suit(suit).size().max(a2.suit(suit).size());
            lengths += max_length;
            let b_suit = all_suit.different(a_suit);
            if !b_suit.is_empty() {
                let stopper = b_suit.top();
                let holder = if self.hands[b_seat[0]].have(stopper) {
                    0
                } else {
                    1
                };
                // The hand playing just before the stopper's holder.
                let before = self.hands[(b_seat[holder] + 3) % NUM_SEATS];
                let higher = a_suit.slice(0, stopper);
                if before.include(higher)
                    && self.hands[b_seat[holder]].suit(suit).size() > higher.size()
                {
                    winners += higher.size();
                    in_charge[holder] += 1;
                    stopper_rank_winners =
                        stopper_rank_winners.union(all_suit.slice(0, stopper + 1));
                    continue;
                }
            }
            winners += max_length;
        }
        if lengths < num_tricks {
            return (1, top_rank_winners);
        }
        let most = in_charge[0].max(in_charge[1]);
        if most > 0 && winners + most <= num_tricks {
            return (1, top_rank_winners.union(stopper_rank_winners));
        }
        (0, Cards::new())
    }

    /// Upstream's `TrumpLengthTricks`: each trump of the longer hand of
    /// `my`/`pd` wins a trick unless an opponent's trump falls on it, so that
    /// side takes at least that hand's trump length less both opponents'.
    /// `None` when there are no trumps out or the count is not positive.
    fn trump_length_tricks(&self, my: Seat, pd: Seat, lho: Seat, rho: Seat) -> Option<usize> {
        if self.trump >= NOTRUMP {
            return None;
        }
        let len = |seat: Seat| self.hands[seat].suit(self.trump).size();
        let ours = len(my).max(len(pd));
        let theirs = len(lho) + len(rho);
        (ours > theirs).then(|| ours - theirs)
    }

    /// Slow tricks for opponent (NT contracts only)
    /// When NS is leading, returns EW's slow tricks.
    /// When EW is leading, returns NS's slow tricks.
    /// Returns (count, rank_winners) matching C++ SlowNoTrumpTricks.
    fn slow_tricks_opponent(&self, depth: usize) -> (usize, Cards) {
        let seat_to_play = self.plays[depth].seat_to_play;

        // Opponents of current player
        let lho_hand = self.hands[left_hand_opp(seat_to_play)];
        let rho_hand = self.hands[right_hand_opp(seat_to_play)];

        // Current player's partnership
        let my_hand = self.hands[seat_to_play];
        let pd_hand = self.hands[partner(seat_to_play)];
        let my_side_cards = my_hand.union(pd_hand);

        let all_cards = self.hands.all_cards();
        let mut rank_winners = Cards::new();

        // For each suit where current player has cards
        for suit in 0..NUM_SUITS {
            if my_hand.suit(suit).is_empty() {
                continue;
            }
            let all_suit = all_cards.suit(suit);
            if all_suit.is_empty() {
                continue;
            }
            let top = all_suit.top();
            // If current side has the top card, no slow trick for opponents
            if my_side_cards.have(top) {
                return (0, Cards::new());
            }
            rank_winners.add(top);
        }

        if rank_winners.is_empty() {
            return (0, Cards::new());
        }

        // Check if all rank winners are in one opponent's hand
        if lho_hand.include(rank_winners) || rho_hand.include(rank_winners) {
            (rank_winners.size(), rank_winners)
        } else {
            (1, rank_winners)
        }
    }
}

/// Public test function for slow_trump_tricks_opponent
/// This allows unit testing the finesse detection logic
pub fn slow_trump_tricks_opponent(
    hands: &Hands,
    trump: Suit,
    seat_to_play: Seat,
    num_tricks: usize,
    leading: bool,
) -> usize {
    // From opponent's perspective (LHO = "my", RHO = "pd")
    slow_trump_tricks(
        hands.all_cards().suit(trump),
        hands[left_hand_opp(seat_to_play)].suit(trump),
        hands[right_hand_opp(seat_to_play)].suit(trump),
        hands[partner(seat_to_play)].suit(trump),
        hands[seat_to_play].suit(trump),
        num_tricks,
        leading,
    )
    .0
}

/// Upstream's `SlowTrumpTricks`: tricks the side holding `my` and `pd` is
/// sure to take in trumps however play goes, from finesse and length
/// positions against the top trumps, and the cards that decide it. Only ever
/// asked at the start of a trick, with `num_tricks` cards left in each hand.
///
/// Each test is upstream's, in upstream's order; the first that applies
/// answers.
pub(crate) fn slow_trump_tricks(
    all_trumps: Cards,
    my_trumps: Cards,
    pd_trumps: Cards,
    lho_trumps: Cards,
    rho_trumps: Cards,
    num_tricks: usize,
    leading: bool,
) -> (usize, Cards) {
    if all_trumps.size() < 3 {
        return (0, Cards::new());
    }
    let card = |c: usize| Cards::from_bits(1u64 << c);
    // `include` is "holds all of"; `strictly` is "holds them and more".
    let strictly = |hand: Cards, c: Cards| hand.include(c) && hand != c;
    let a = card(all_trumps.top());
    let k = card(all_trumps.different(a).top());
    let q = card(all_trumps.different(a.union(k)).top());
    let opps = lho_trumps.union(rho_trumps);
    let ours = my_trumps.union(pd_trumps);

    // Kx behind A
    if (strictly(pd_trumps, k) && lho_trumps.include(a))
        || (strictly(my_trumps, k) && rho_trumps.include(a) && (!leading || num_tricks >= 3))
    {
        return (1, a.union(k));
    }
    // Kx against stiff A (upstream e7de277)
    if (strictly(pd_trumps, k) || strictly(my_trumps, k))
        && (rho_trumps == a || lho_trumps == a)
        && (!leading || num_tricks >= 3)
    {
        return (1, a.union(k));
    }
    // KQJ against A (upstream e7de277)
    if all_trumps.size() >= 4 {
        let j = card(all_trumps.different(a.union(k).union(q)).top());
        if opps.include(a)
            && ours.include(k.union(q).union(j))
            && (my_trumps.size() >= 3 || pd_trumps.size() >= 3)
        {
            return (2, a.union(k).union(q).union(j));
        }
    }
    // KQ against A. Upstream c94c80f; before it, this test read `Have(a)`,
    // which C++ turned into `Have(1)` through `operator bool`, so it never
    // fired, and it allowed a singleton where two cards are needed.
    if opps.include(a)
        && ours.include(k.union(q))
        && (my_trumps.size() >= 2 || pd_trumps.size() >= 2)
    {
        return (1, a.union(k).union(q));
    }
    // Qxx behind AK
    if all_trumps.size() >= 5
        && ((pd_trumps.include(q) && pd_trumps.size() >= 3 && lho_trumps.include(a.union(k)))
            || (my_trumps.include(q)
                && my_trumps.size() >= 3
                && rho_trumps.include(a.union(k))
                && (!leading || num_tricks >= 4)))
    {
        return (1, a.union(k).union(q));
    }
    // Qxx against AK tight (upstream e7de277): AK doubleton in one hand, or
    // each opponent down to a single trump.
    if ((pd_trumps.include(q) && pd_trumps.size() >= 3)
        || (my_trumps.include(q) && my_trumps.size() >= 3))
        && (lho_trumps == a.union(k)
            || rho_trumps == a.union(k)
            || (lho_trumps.size() == 1 && rho_trumps.size() == 1))
        && (!leading || num_tricks >= 4)
    {
        return (1, a.union(k).union(q));
    }
    (0, Cards::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge_solver::{PartialTrick, Solver};
    use crate::pattern::PatternCache;

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

    /// `size` cards to each seat from the given suits.
    fn ending(rng: &mut Lcg, size: usize, suits: &[Suit]) -> Hands {
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

    /// North-South's tricks from the position after `lead`, with the
    /// second-seat rule on or off.
    fn solve_after_lead(hands: Hands, trump: usize, leader: Seat, lead: usize, rule: bool) -> u8 {
        let mut after = hands;
        after[leader].remove(lead);
        let mut trick = PartialTrick::new();
        trick.add(lead, leader);
        let solver = Solver::new_mid_trick(after, trump, &trick).expect("one card played");
        SECOND_SEAT_RULE.with(|on| on.set(rule));
        let mut cutoff = CutoffCache::new(12);
        let mut pattern = PatternCache::new(10);
        let ns = solver.solve_mid_trick(&mut cutoff, &mut pattern, &trick);
        SECOND_SEAT_RULE.with(|on| on.set(true));
        ns
    }

    /// Bob Salita's second-seat rule was not fuzzed when he wrote it. Solve
    /// random positions with the second hand to play both with the rule and
    /// without it: a bound it claimed that did not hold would change a value,
    /// at the root or at any second-seat node below. Half the endings are
    /// dealt from two or three suits so ruffs and overruffs decide often.
    #[test]
    fn second_seat_rule_never_changes_a_value() {
        // FUZZ_ROUNDS raises the count for a longer one-off run.
        let rounds: usize = std::env::var("FUZZ_ROUNDS")
            .ok()
            .and_then(|r| r.parse().ok())
            .unwrap_or(4000);
        let mut rng = Lcg(0x5ec0);
        let mut checked = 0;
        for round in 0..rounds {
            let size = 2 + rng.next(6);
            let suits: Vec<Suit> = if round % 2 == 0 {
                (0..NUM_SUITS).collect()
            } else {
                let n = 2 + rng.next(2);
                let mut all: Vec<Suit> = (0..NUM_SUITS).collect();
                for i in (1..all.len()).rev() {
                    all.swap(i, rng.next(i + 1));
                }
                all.truncate(n);
                all
            };
            if suits.len() * NUM_RANKS < size * NUM_SEATS {
                continue;
            }
            let hands = ending(&mut rng, size, &suits);
            // In a short-suited ending, make trumps one of its suits mostly.
            let trump = if round % 2 == 1 && rng.next(4) != 0 {
                suits[rng.next(suits.len())]
            } else {
                rng.next(NUM_SUITS + 1)
            };
            let leader = rng.next(NUM_SEATS);
            let held: Vec<usize> = hands[leader].iter().collect();
            let lead = held[rng.next(held.len())];
            let with = solve_after_lead(hands, trump, leader, lead, true);
            let without = solve_after_lead(hands, trump, leader, lead, false);
            assert_eq!(
                with, without,
                "round {round}: trump {trump}, leader {leader}, lead {lead}, hands {:x} {:x} {:x} {:x}",
                hands[0].value(), hands[1].value(), hands[2].value(), hands[3].value()
            );
            checked += 1;
        }
        assert!(checked > rounds * 3 / 4, "only {checked} positions checked");
    }
}
