//! Refactored search implementation matching C++ structure
//!
//! This module implements a 3-layer search structure matching the C++ reference:
//! - SearchWithCache: Trick boundary handling, TT lookups, XRAY logging
//! - SearchAtTrickStart: Fast/slow tricks pruning
//! - EvaluatePlayableCards: Main card evaluation loop with IsEquivalent

use super::cards::*;
use super::hands::Hands;
use super::pattern::{compute_pattern_hands, Bounds, Pattern, PatternCache, RelativeHands, Shape};
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
) -> (CutoffHash, u64, u64) {
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
        let trick_idx = depth / 4;
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
            let prev_ns_tricks = self.plays[depth - 1].ns_tricks_won;
            let prev_seat = self.plays[depth - 1].seat_to_play;
            self.plays[depth].ns_tricks_won = prev_ns_tricks;
            self.plays[depth].seat_to_play = next_seat(prev_seat);
            #[cfg(feature = "debug_search")]
            eprintln!(
                "search_with_cache depth={} mid-trick: ns_tricks={} seat={}",
                depth, prev_ns_tricks, self.plays[depth].seat_to_play
            );
            return self.evaluate_playable_cards(depth, beta);
        }

        // Trick start: compute ns_tricks_won and seat_to_play from previous trick
        if depth > 0 {
            let prev_ns_tricks = self.plays[depth - 1].ns_tricks_won;
            let prev_winning_play = self.plays[depth - 1].winning_play;
            let prev_winner_seat = self.plays[prev_winning_play].seat_to_play;
            let ns_won = if is_ns(prev_winner_seat) { 1 } else { 0 };
            self.plays[depth].ns_tricks_won = prev_ns_tricks + ns_won;
            self.plays[depth].seat_to_play = prev_winner_seat;
            #[cfg(feature = "debug_search")]
            eprintln!("search_with_cache depth={} trick-start: prev_win_play={} winner_seat={} ns_won={} total_ns={}",
                      depth, prev_winning_play, prev_winner_seat, ns_won, self.plays[depth].ns_tricks_won);
        }

        let ns_tricks_won = self.plays[depth].ns_tricks_won;
        let seat_to_play = self.plays[depth].seat_to_play;

        // XRAY detailed logging (at trick boundaries only, for readability)
        if xray_should_log() && card_in_trick == 0 {
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

        // Store all_cards for this trick
        let all_cards = self.hands.all_cards();
        self.tricks[trick_idx].all_cards = all_cards;

        // Compute shape and relative hands for pattern cache
        if depth == 0 {
            // Initial trick - compute shape from scratch
            self.tricks[trick_idx].shape = Shape::from_hands(self.hands);
            self.tricks[trick_idx]
                .relative_hands
                .compute(self.hands, all_cards);
        } else {
            // Update shape and relative hands from previous trick
            let prev_trick_idx = trick_idx - 1;
            let prev_all_cards = self.tricks[prev_trick_idx].all_cards;

            // Copy shape from previous trick and update for played cards
            let mut shape = self.tricks[prev_trick_idx].shape;
            let prev_start = prev_trick_idx * 4;
            let c1 = self.plays[prev_start].card_played;
            let c2 = self.plays[prev_start + 1].card_played;
            let c3 = self.plays[prev_start + 2].card_played;
            let c4 = self.plays[prev_start + 3].card_played;
            let prev_leader = self.plays[prev_start].seat_to_play;
            shape.play_cards(prev_leader, c1, c2, c3, c4);
            self.tricks[trick_idx].shape = shape;

            // Update relative hands
            self.tricks[trick_idx].relative_hands = self.tricks[prev_trick_idx].relative_hands;
            self.tricks[trick_idx]
                .relative_hands
                .update(self.hands, prev_all_cards, all_cards);
        }

        // Pattern cache lookup (matching C++ common_bounds_cache)
        let shape_value = self.tricks[trick_idx].shape.value();
        // Hashed once for both the lookup below and the store further down.
        let pattern_hash = PatternCache::hash_for(shape_value);
        let mut pattern_cutoff = false;
        let rel_beta = beta - ns_tricks_won as i8;
        if !NO_TT.load(Ordering::Relaxed) {
            if let Some(entry) = self.pattern_cache.lookup(pattern_hash) {
                // Create pattern from current relative hands for lookup
                let new_pattern = Pattern::new(
                    self.tricks[trick_idx].relative_hands.hands,
                    Bounds::new(0, remaining as i8),
                );
                // Use relative beta for cutoff check (bounds are stored relative to ns_tricks_won)
                if let Some((matched_hands, bounds)) =
                    Pattern::lookup_in(&entry.patterns[seat_to_play], &new_pattern, rel_beta)
                        .map(|matched| (matched.hands, matched.bounds))
                {
                    // Compute rank_winners from matched pattern (matching C++ GetRankWinners)
                    let matched_pattern = Pattern::new(matched_hands, bounds);
                    let rank_winners = matched_pattern.get_rank_winners(all_cards);

                    let adj_lower = bounds.lower + ns_tricks_won as i8;
                    let adj_upper = bounds.upper + ns_tricks_won as i8;
                    if adj_lower >= beta {
                        if xray_should_log() {
                            eprintln!(
                                "PATTERN_HIT: depth={} seat={} beta={} ns_tricks_won={} bounds=[{},{}] adj_lower={} LOWER_CUT shape={:x} hands=[{:x},{:x},{:x},{:x}]",
                                depth, seat_to_play, beta, ns_tricks_won, bounds.lower, bounds.upper, adj_lower,
                                shape_value,
                                matched_pattern.hands[WEST].value(),
                                matched_pattern.hands[NORTH].value(),
                                matched_pattern.hands[EAST].value(),
                                matched_pattern.hands[SOUTH].value()
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

        // Search at trick start (with pruning)
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
                all_cards,
                result.rank_winners,
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

    /// SearchAtTrickStart - fast/slow tricks pruning
    /// Matches C++ Play::SearchAtTrickStart
    fn search_at_trick_start(&mut self, depth: usize, beta: i8) -> SearchResult {
        let trick_idx = depth / 4;
        let ns_tricks_won = self.plays[depth].ns_tricks_won;
        let seat_to_play = self.plays[depth].seat_to_play;
        let remaining = self.num_tricks - trick_idx;

        if !NO_PRUNING.load(Ordering::Relaxed) {
            // Fast tricks pruning
            let (fast, fast_rank_winners) = self.fast_tricks(depth);

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

            if is_ns(seat_to_play) && ns_tricks_won as usize + fast >= beta as usize {
                return SearchResult {
                    ns_tricks: (ns_tricks_won as usize + fast) as u8,
                    rank_winners: fast_rank_winners,
                };
            }
            if !is_ns(seat_to_play) && (ns_tricks_won as usize + remaining - fast) < beta as usize {
                return SearchResult {
                    ns_tricks: (ns_tricks_won as usize + remaining - fast) as u8,
                    rank_winners: fast_rank_winners,
                };
            }

            // Slow tricks pruning
            // For trump contracts with trumps remaining: use top trump tricks, then slow trump tricks
            // For NT contracts OR trump contract with no trumps remaining: use slow NT tricks
            let all_cards = self.hands.all_cards();
            let has_trumps = self.trump < NOTRUMP && !all_cards.suit(self.trump).is_empty();
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

        // Get playable cards
        let lead_suit = if card_in_trick == 0 {
            None
        } else {
            Some(self.tricks[trick_idx].lead_suit)
        };
        let playable = get_playable_cards(self.hands, seat_to_play, lead_suit);

        if playable.is_empty() {
            return SearchResult {
                ns_tricks: ns_tricks_won,
                rank_winners: Cards::new(),
            };
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
            Self::order_cards_static(
                &mut ordered_cards,
                depth,
                remaining_playable,
                &self.plays,
                &self.tricks,
                self.hands,
                self.trump,
            );
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
                    Self::order_cards_static(
                        &mut ordered_cards,
                        depth,
                        remaining_playable,
                        &self.plays,
                        &self.tricks,
                        self.hands,
                        self.trump,
                    );
                    remaining_playable = Cards::new();
                }
                i += 1;
                continue;
            }

            // IsEquivalent check - always call, even for first card (matches C++ behavior)
            if self.is_equivalent(card, tried_cards.suit(suit), my_hand, all_cards) {
                tried_cards.add(card);
                // C++ behavior: after trying first card, order remaining playable cards
                if !remaining_playable.is_empty() {
                    Self::order_cards_static(
                        &mut ordered_cards,
                        depth,
                        remaining_playable,
                        &self.plays,
                        &self.tricks,
                        self.hands,
                        self.trump,
                    );
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
                    return SearchResult {
                        ns_tricks: best,
                        rank_winners: branch_rank_winners,
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
                    return SearchResult {
                        ns_tricks: best,
                        rank_winners: branch_rank_winners,
                    };
                }
            }

            // Accumulate rank_winners from this branch
            rank_winners.add_cards(branch_rank_winners);

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
                Self::order_cards_static(
                    &mut ordered_cards,
                    depth,
                    remaining_playable,
                    &self.plays,
                    &self.tricks,
                    self.hands,
                    self.trump,
                );
                remaining_playable = Cards::new();
            }

            i += 1;
        }

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

        // Record this play
        self.plays[depth].card_played = card;

        #[cfg(feature = "debug_search")]
        let before_size = self.hands[seat_to_play].size();

        // Remove from hand
        self.hands[seat_to_play].remove(card);

        #[cfg(feature = "debug_search")]
        {
            let after_size = self.hands[seat_to_play].size();
            if after_size != before_size - 1 {
                eprintln!("ERROR: remove didn't work! depth={} seat={} card={} before={} after={} hand={:x}",
                          depth, seat_to_play, card_name(card), before_size, after_size, self.hands[seat_to_play].value());
            }
        }

        // Update trick state
        if card_in_trick == 0 {
            // Leading
            self.tricks[trick_idx].lead_suit = suit_of(card);
            self.plays[depth].winning_play = depth;
            #[cfg(feature = "debug_search")]
            eprintln!(
                "  depth={} {} leads {} winning_play={}",
                depth,
                seat_to_play,
                card_name(card),
                depth
            );
        } else {
            // Following - check if we beat the current winner
            let current_winner_idx = self.plays[depth - 1].winning_play;
            let current_winner_card = self.plays[current_winner_idx].card_played;

            if self.wins_over(card, current_winner_card, self.tricks[trick_idx].lead_suit) {
                self.plays[depth].winning_play = depth;
                #[cfg(feature = "debug_search")]
                eprintln!(
                    "  depth={} {} plays {} WINS over {} winning_play={}",
                    depth,
                    seat_to_play,
                    card_name(card),
                    card_name(current_winner_card),
                    depth
                );
            } else {
                self.plays[depth].winning_play = current_winner_idx;
                #[cfg(feature = "debug_search")]
                eprintln!(
                    "  depth={} {} plays {} loses to {} winning_play={}",
                    depth,
                    seat_to_play,
                    card_name(card),
                    card_name(current_winner_card),
                    current_winner_idx
                );
            }
        }

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
    fn is_equivalent(
        &self,
        card: usize,
        tried_suit: Cards,
        my_hand: Cards,
        all_cards: Cards,
    ) -> bool {
        let mut result = false;

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

        result
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
    fn suit_fast_tricks(
        my_suit: Cards,
        my_winners: usize,
        pd_suit: Cards,
        pd_winners: usize,
        pd_entry: &mut bool,
    ) -> usize {
        // Entry from partner if my top winner can cover partner's bottom card.
        if !pd_suit.is_empty() && my_winners > 0 && higher_rank(my_suit.top(), pd_suit.bottom()) {
            *pd_entry = true;
        }
        // Partner has no winners.
        if pd_winners == 0 {
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
    fn fast_tricks_from_seat(&self, seat: Seat, all_cards: Cards) -> (usize, Cards) {
        let my_hand = self.hands[seat];
        let pd_hand = self.hands[partner(seat)];
        let lho_hand = self.hands[left_hand_opp(seat)];
        let rho_hand = self.hands[right_hand_opp(seat)];

        // Count top trump tricks for our side in trump contracts
        let (trump_tricks, mut rank_winners) = if self.trump < NOTRUMP {
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
            // Skip trump suit in trump contracts
            if self.trump < NOTRUMP && suit == self.trump {
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

            my_tricks +=
                Self::suit_fast_tricks(my_suit, my_winners, pd_suit, pd_winners, &mut my_entry);
            pd_tricks +=
                Self::suit_fast_tricks(pd_suit, pd_winners, my_suit, my_winners, &mut pd_entry);
        }

        let side_suit_tricks = if pd_entry {
            rank_winners = rank_winners.union(pd_rank_winners);
            my_tricks.max(pd_tricks)
        } else {
            my_tricks
        };

        // Uncapped. The reference caps at the call site, and reports the
        // number from *before* that cap as `raw`; capping here made our `raw`
        // mean something different from theirs, which read as a divergence in
        // five of the seven fixtures when the two searches in fact agreed.
        (trump_tricks + side_suit_tricks, rank_winners)
    }

    /// Fast tricks estimation - returns (count, rank_winners)
    fn fast_tricks(&self, depth: usize) -> (usize, Cards) {
        let seat_to_play = self.plays[depth].seat_to_play;
        let all_cards = self.hands.all_cards();
        let trick_idx = depth / 4;
        let max_tricks = self.num_tricks - trick_idx;
        let (raw, rank_winners) = self.fast_tricks_from_seat(seat_to_play, all_cards);
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
        (result, rank_winners)
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
