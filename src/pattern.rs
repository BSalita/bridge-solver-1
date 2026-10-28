//! Shape-based pattern cache for transposition table
//!
//! This implements the C++ solver's pattern cache which:
//! - Keys by hand shapes (suit lengths) rather than actual cards
//! - Stores relative hands (cards normalized within each suit)
//! - Uses a tree structure to store patterns with bounds
//!
//! This allows matching positions that are equivalent due to card equivalence.

use super::cards::{mask_of, suit_of, Cards};
use super::hands::Hands;
use super::types::*;
use crate::pattern_vec::PatternVec;

/// Unpack bits: scatter source bits to positions where mask has 1s
/// Example: UnpackBits(0b101, 0b11100) = 0b10100 (scatters bits 0,1,2 to positions 2,3,4)
#[inline]
// `m & m.wrapping_neg()` is the classic isolate-lowest-set-bit idiom. Clippy
// (since the toolchain that stabilized it) wants `m.isolate_lowest_one()`, but
// that method is still unstable on older compilers, so adopting it would raise
// this crate's minimum Rust version for no benefit in a hot path. `unknown_lints`
// keeps compilers that predate the lint quiet about the allow itself.
#[allow(unknown_lints, clippy::manual_isolate_lowest_one)]
pub fn unpack_bits(source: u64, mask: u64) -> u64 {
    #[cfg(target_feature = "bmi2")]
    {
        // Use PDEP instruction if available
        unsafe { core::arch::x86_64::_pdep_u64(source, mask) }
    }
    #[cfg(not(target_feature = "bmi2"))]
    {
        if source == 0 {
            return 0;
        }
        let mut unpacked = 0u64;
        let mut bit = 1u64;
        let mut src = source;
        let mut m = mask;
        while src != 0 && m != 0 {
            if src & bit != 0 {
                unpacked |= m & m.wrapping_neg();
                src &= !bit;
            }
            bit <<= 1;
            m &= m - 1;
        }
        unpacked
    }
}

/// Shape encodes the suit lengths for all 4 hands in 64 bits
/// Each hand-suit pair gets 4 bits (0-13), arranged as:
/// bits 60-63: West spades, bits 56-59: West hearts, etc.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Shape {
    value: u64,
}

impl Shape {
    pub fn from_hands(hands: &Hands) -> Self {
        let mut value = 0u64;
        for seat in 0..NUM_SEATS {
            for suit in 0..NUM_SUITS {
                let len = hands[seat].suit(suit).size() as u64;
                value |= len << Self::offset(seat, suit);
            }
        }
        Shape { value }
    }

    /// Update shape after a trick is played
    pub fn play_cards(&mut self, seat: Seat, c1: usize, c2: usize, c3: usize, c4: usize) {
        self.value -= 1u64 << Self::offset(seat, suit_of(c1));
        self.value -= 1u64 << Self::offset((seat + 1) % NUM_SEATS, suit_of(c2));
        self.value -= 1u64 << Self::offset((seat + 2) % NUM_SEATS, suit_of(c3));
        self.value -= 1u64 << Self::offset((seat + 3) % NUM_SEATS, suit_of(c4));
    }

    pub fn value(&self) -> u64 {
        self.value
    }

    #[inline]
    fn offset(seat: Seat, suit: Suit) -> u32 {
        (60 - (seat * NUM_SUITS + suit) * 4) as u32
    }
}

/// Bounds represent the proven range of tricks for a position
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Bounds {
    pub lower: i8,
    pub upper: i8,
}

impl Bounds {
    pub fn new(lower: i8, upper: i8) -> Self {
        Bounds { lower, upper }
    }

    pub fn is_empty(&self) -> bool {
        self.upper < self.lower
    }

    pub fn intersect(&self, other: Bounds) -> Bounds {
        Bounds {
            lower: self.lower.max(other.lower),
            upper: self.upper.min(other.upper),
        }
    }

    /// Returns true if this bound causes a cutoff at the given beta
    pub fn cutoff(&self, beta: i8) -> bool {
        self.lower >= beta || self.upper < beta
    }
}

/// A Pattern stores hands with relative cards and bounds
/// Patterns form a tree where more specific patterns are children of more general ones
#[derive(Clone)]
pub struct Pattern {
    pub hands: Hands,
    pub bounds: Bounds,
    pub children: PatternVec,
}

impl Default for Pattern {
    fn default() -> Self {
        Pattern {
            hands: Hands::default(),
            bounds: Bounds::new(0, TOTAL_TRICKS as i8),
            children: PatternVec::new(),
        }
    }
}

impl Pattern {
    pub fn new(hands: Hands, bounds: Bounds) -> Self {
        Pattern {
            hands,
            bounds,
            children: PatternVec::new(),
        }
    }

    pub fn reset(&mut self) {
        self.hands = Hands::default();
        self.bounds = Bounds::new(0, TOTAL_TRICKS as i8);
        self.children.clear();
    }

    /// Look up a pattern that matches and causes a cutoff at beta
    pub fn lookup(&self, new_pattern: &Pattern, beta: i8) -> Option<&Pattern> {
        for child in &self.children {
            if !new_pattern.is_subset_of(child) {
                continue;
            }
            if child.bounds.cutoff(beta) {
                return Some(child);
            }
            if let Some(detail) = child.lookup(new_pattern, beta) {
                return Some(detail);
            }
        }
        None
    }

    /// Update the pattern tree with a new pattern
    pub fn update(&mut self, mut new_pattern: Pattern) {
        for i in 0..self.children.len() {
            let child = &mut self.children[i];

            if new_pattern.hands_equal(child) {
                // Same pattern - update bounds
                child.update_bounds(new_pattern.bounds);
                return;
            } else if new_pattern.is_subset_of(child) {
                // New pattern is more specific - add under existing
                new_pattern.bounds = new_pattern.bounds.intersect(child.bounds);
                if !new_pattern.bounds.is_empty() && new_pattern.bounds != child.bounds {
                    child.update(new_pattern);
                }
                return;
            } else if child.is_subset_of(&new_pattern) {
                // New pattern is more general - absorb child.
                //
                // The absorbed child's *position* is load-bearing. The
                // reference finishes with `pattern.MoveFrom(new_pattern)`,
                // overwriting slot `i` in place, so the new pattern inherits
                // the position of the child it absorbed. Removing slot `i` and
                // pushing the result instead reorders the children twice over:
                // `swap_remove` drops the last child into the hole, and the new
                // pattern lands at the end. `Pattern::lookup` returns the first
                // child that matches, so the order decides *which* pattern a
                // hit returns, and with it the rank winners that hit reports.
                child.update_bounds(new_pattern.bounds);
                if child.bounds != new_pattern.bounds {
                    // The child becomes a sub-pattern of the new one; slot `i`
                    // is overwritten below, so leaving a default behind is safe.
                    let absorbed = std::mem::take(&mut self.children[i]);
                    new_pattern.children.push(absorbed);
                } else {
                    // `new_pattern.patterns.swap(pattern.patterns)` in the
                    // reference: a swap, not an append, so any children the new
                    // pattern arrived with are dropped rather than merged.
                    std::mem::swap(&mut new_pattern.children, &mut self.children[i].children);
                }
                // Check remaining children
                let mut j = i + 1;
                while j < self.children.len() {
                    if self.children[j].is_subset_of(&new_pattern) {
                        self.children[j].update_bounds(new_pattern.bounds);
                        if self.children[j].bounds != new_pattern.bounds {
                            new_pattern.children.push(self.children.swap_remove(j));
                        } else if new_pattern.children.is_empty() {
                            std::mem::swap(
                                &mut new_pattern.children,
                                &mut self.children[j].children,
                            );
                            self.children.swap_remove(j);
                        } else {
                            let mut removed = self.children.swap_remove(j);
                            new_pattern.children.append(&mut removed.children);
                        }
                    } else {
                        j += 1;
                    }
                }
                self.children[i] = new_pattern;
                return;
            }
        }
        // No relationship - add as new child
        self.children.push(new_pattern);
    }

    /// Update bounds and propagate to children
    fn update_bounds(&mut self, new_bounds: Bounds) {
        let old_bounds = self.bounds;
        self.bounds = self.bounds.intersect(new_bounds);
        if self.bounds.is_empty() || self.bounds == old_bounds {
            return;
        }
        let mut i = 0;
        while i < self.children.len() {
            self.children[i].update_bounds(self.bounds);
            if self.children[i].bounds != self.bounds {
                i += 1;
                continue;
            }
            // Child bounds now match parent - flatten.
            //
            // Order of operations matters, and is not the obvious one. The
            // reference lifts the child's sub-patterns onto the end of the list
            // *first* and only then deletes the child, so its swap-with-last
            // removal can pull one of the just-appended sub-patterns into the
            // hole. Deleting first and appending after -- which is what
            // `swap_remove(i)` followed by `extend` does -- leaves the same
            // patterns in a different order, and `Pattern::lookup` returns the
            // first child that matches.
            let mut subpatterns = PatternVec::new();
            std::mem::swap(&mut subpatterns, &mut self.children[i].children);
            self.children.append(&mut subpatterns);
            self.children.swap_remove(i);
            // `i` is not advanced: swap_remove has put a new child here.
        }
    }

    /// Check if this pattern is a subset of (more specific than) another
    /// A pattern is more specific if each hand includes (is subset of) the other
    fn is_subset_of(&self, other: &Pattern) -> bool {
        self.hands[WEST].include(other.hands[WEST])
            && self.hands[NORTH].include(other.hands[NORTH])
            && self.hands[EAST].include(other.hands[EAST])
            && self.hands[SOUTH].include(other.hands[SOUTH])
    }

    /// Check if hands are equal
    fn hands_equal(&self, other: &Pattern) -> bool {
        self.hands[WEST] == other.hands[WEST]
            && self.hands[NORTH] == other.hands[NORTH]
            && self.hands[EAST] == other.hands[EAST]
            && self.hands[SOUTH] == other.hands[SOUTH]
    }

    /// Convert relative rank winners back to actual cards
    pub fn get_rank_winners(&self, all_cards: Cards) -> Cards {
        let relative_rank_winners = self.hands.all_cards();
        let mut rank_winners = Cards::new();
        for suit in 0..NUM_SUITS {
            let rel_suit = relative_rank_winners.suit(suit);
            if rel_suit.is_empty() {
                continue;
            }
            let packed = rel_suit.value() >> (suit * NUM_RANKS);
            let unpacked = unpack_bits(packed, all_cards.suit(suit).value());
            rank_winners = rank_winners.union(Cards::from_bits(unpacked));
        }
        rank_winners
    }
}

/// ShapeEntry is what gets stored in the cache
#[derive(Default)]
pub struct ShapeEntry {
    pub hash: u64,
    pub pattern: Pattern,
}

impl ShapeEntry {
    pub fn reset(&mut self, hash: u64) {
        self.hash = hash;
        self.pattern.reset();
    }

    /// Look up a matching pattern that causes a cutoff.
    ///
    /// Takes `&mut self` because a hit on a child is *promoted* into the root
    /// slot, matching the C++ reference's `ShapeEntry::Lookup`. That promotion
    /// is not merely a most-recently-used speed-up: `Pattern::update` compares
    /// candidates against the root's hands and bounds, so a root left stale
    /// grows a differently-shaped tree, and the generalisations it then stores
    /// are not the reference's. Omitting it is what produced the wrong tables
    /// in #14.
    ///
    /// Returns the matched hands by value; the caller only reads them, and
    /// borrowing would conflict with the promotion.
    pub fn lookup(&mut self, new_pattern: &Pattern, beta: i8) -> Option<(Hands, Bounds)> {
        // Root fast path.
        if self.pattern.bounds.cutoff(beta) && new_pattern.is_subset_of(&self.pattern) {
            return Some((self.pattern.hands, self.pattern.bounds));
        }
        // Otherwise search the children, and promote whatever matched.
        let found = self
            .pattern
            .lookup(new_pattern, beta)
            .map(|matched| (matched.hands, matched.bounds));
        if let Some((hands, bounds)) = found {
            self.pattern.hands = hands;
            self.pattern.bounds = bounds;
        }
        found
    }
}

/// The common bounds cache - hash table of ShapeEntries
///
/// Open-addressed with linear probing, like the C++ reference's `Cache` and
/// like [`CutoffCache`](crate::CutoffCache). This was direct-mapped once --
/// one slot per hash, evicting whatever was there on a collision -- which
/// silently threw away entries the reference keeps and cost cutoffs later.
/// Because collisions get more frequent as the table fills, the loss grew with
/// the length of the search.
pub struct PatternCache {
    entries: Box<[ShapeEntry]>,
    bits: usize,
    /// The size this cache was built at, and the floor [`Self::reset`] shrinks
    /// back to; see [`crate::search::CutoffCache::reset`].
    base_bits: usize,
    probe_distance: usize,
    load_count: usize,
}

impl PatternCache {
    pub fn new(bits: usize) -> Self {
        let size = 1 << bits;
        let mut entries = Vec::with_capacity(size);
        for _ in 0..size {
            entries.push(ShapeEntry::default());
        }
        PatternCache {
            entries: entries.into_boxed_slice(),
            bits,
            base_bits: bits,
            probe_distance: 0,
            load_count: 0,
        }
    }

    /// Empty the cache, ready for the next search, without giving up the
    /// allocation.
    ///
    /// The counterpart of [`crate::search::CutoffCache::reset`], and reset for
    /// the same reason and on the same terms: the reference's
    /// `common_bounds_cache` is a global that `Solve` only clears per trump.
    /// See that method for why the size cannot reach the answers -- this table
    /// never evicts either -- and for why the capacity is kept only as far as
    /// the last search justified it.
    ///
    /// The in-place scan skips slots whose hash is zero. That is not an
    /// approximation: a zero hash already reads as empty everywhere, since
    /// [`Self::lookup`] stops at one and [`Self::get_or_create`] overwrites it
    /// before handing it back. So an untouched slot needs nothing done to it,
    /// and clearing costs one read per slot rather than a rewritten pattern
    /// tree.
    pub fn reset(&mut self) {
        let bits = self.bits_for(self.load_count);
        if bits == self.bits {
            for entry in self.entries.iter_mut() {
                if entry.hash != 0 {
                    entry.reset(0);
                }
            }
        } else {
            // Dropping the old table hands its pattern-tree blocks back to the
            // pool, so the shrink costs the allocator one table and nothing
            // else.
            let size = 1 << bits;
            let mut entries = Vec::with_capacity(size);
            for _ in 0..size {
                entries.push(ShapeEntry::default());
            }
            self.entries = entries.into_boxed_slice();
            self.bits = bits;
        }
        self.probe_distance = 0;
        self.load_count = 0;
    }

    /// The smallest table, never below the size this cache was built at, that
    /// holds `load` entries without tripping the resize in
    /// [`Self::get_or_create`]. The threshold is written the way that method
    /// writes it so the two cannot drift apart.
    fn bits_for(&self, load: usize) -> usize {
        let mut bits = self.base_bits;
        while load >= (1usize << bits) / 4 * 3 {
            bits += 1;
        }
        bits
    }

    /// Hash function matching C++ Cache template
    /// The slot key for a shape and seat.
    ///
    /// Public because callers hash once and pass the result to both
    /// [`Self::lookup`] and, on a miss, [`Self::get_or_create`]. The two used
    /// to hash independently, and the recursion between them puts the second
    /// call beyond anything the compiler could common up.
    pub fn hash_for(shape: u64, seat_to_play: Seat) -> u64 {
        Self::hash(shape, seat_to_play)
    }

    fn hash(shape: u64, seat_to_play: Seat) -> u64 {
        const HASH_RAND: [u64; 2] = [0x9b8b4567327b23c7, 0x643c986966334873];
        let key0 = shape.wrapping_add(HASH_RAND[0]);
        let key1 = (seat_to_play as u64).wrapping_add(HASH_RAND[1]);
        key0.wrapping_mul(key1)
    }

    #[inline]
    fn index(&self, hash: u64) -> usize {
        // Use top bits for index (like C++). The mask is derived from the
        // length rather than stored beside it so that LLVM can see
        // `idx <= len - 1` and drop the bounds check on the probes below;
        // from a separate field it cannot know the two agree.
        (hash >> (64 - self.bits)) as usize & (self.entries.len() - 1)
    }

    /// Look up a shape entry.
    ///
    /// Mutable because a lookup promotes the matched pattern into the entry's
    /// root slot; see [`ShapeEntry::lookup`].
    pub fn lookup(&mut self, hash: u64) -> Option<&mut ShapeEntry> {
        let base = self.index(hash);
        let mask = self.entries.len() - 1;
        let mut found = None;
        // Only as far as anything has ever been placed; an empty slot means
        // the entry cannot be further along, because insertion never skips one.
        for d in 0..self.probe_distance {
            let idx = (base + d) & mask;
            let h = self.entries[idx].hash;
            if h == hash {
                found = Some(idx);
                break;
            }
            if h == 0 {
                break;
            }
        }
        found.map(move |idx| &mut self.entries[idx])
    }

    /// Get or create a shape entry for update
    pub fn get_or_create(&mut self, hash: u64) -> &mut ShapeEntry {
        let size = self.entries.len();
        if self.load_count >= size / 4 * 3 {
            self.resize();
        }

        let base = self.index(hash);
        // See `index` for why the mask comes from the length.
        let mask = self.entries.len() - 1;
        let mut d = 0;
        let slot = loop {
            let idx = (base + d) & mask;
            let h = self.entries[idx].hash;
            if h == hash {
                break idx;
            }
            if h == 0 {
                self.probe_distance = self.probe_distance.max(d + 1);
                self.load_count += 1;
                self.entries[idx].reset(hash);
                break idx;
            }
            d += 1;
        };
        &mut self.entries[slot]
    }

    /// Double the table and re-place every entry.
    ///
    /// Kept at three quarters full so the probe in `get_or_create` always finds
    /// an empty slot and terminates.
    fn resize(&mut self) {
        let old = std::mem::take(&mut self.entries);
        self.bits += 1;
        let new_size = 1 << self.bits;
        let mut entries = Vec::with_capacity(new_size);
        for _ in 0..new_size {
            entries.push(ShapeEntry::default());
        }
        self.entries = entries.into_boxed_slice();
        self.probe_distance = 0;
        self.load_count = 0;

        // Moved, not cloned: a ShapeEntry owns a pattern tree.
        for entry in old.into_vec() {
            if entry.hash == 0 {
                continue;
            }
            let base = self.index(entry.hash);
            let mask = self.entries.len() - 1;
            let mut d = 0;
            loop {
                let idx = (base + d) & mask;
                if self.entries[idx].hash == 0 {
                    self.probe_distance = self.probe_distance.max(d + 1);
                    self.load_count += 1;
                    self.entries[idx] = entry;
                    break;
                }
                d += 1;
            }
        }
    }
}

/// RelativeHands computation - converts actual cards to relative cards
/// Relative cards are packed so that card ranks are relative to remaining cards
#[derive(Clone, Copy, Default)]
pub struct RelativeHands {
    pub hands: Hands,
}

impl RelativeHands {
    /// Convert a suit to relative cards
    ///
    /// The four hands must partition `all_suit_cards`; every caller derives the
    /// one from the other, and the `debug_assert` below holds them to it.
    // `m & m.wrapping_neg()` is the classic isolate-lowest-set-bit idiom; see
    // [`unpack_bits`] for why the stable-but-unstable method is not used.
    #[allow(unknown_lints, clippy::manual_isolate_lowest_one)]
    pub fn convert_suit(&mut self, hands: &Hands, suit: Suit, all_suit_cards: Cards) {
        let all_value = all_suit_cards.value();
        debug_assert_eq!(
            all_value,
            hands.all_cards().suit(suit).value(),
            "convert_suit needs the four hands to partition all_suit_cards"
        );

        // One walk of the mask for all four seats rather than one walk each.
        //
        // This was four calls to a `pack_bits` that mirrored `unpack_bits`
        // below. aarch64 has no PEXT, so each was a software compress: one
        // iteration per card still out in the suit, serially dependent through
        // `m`. The mask is the same for all four seats, so three of those four
        // walks were redundant -- and South need not be walked at all, because
        // the hands partition the suit and South holds exactly the bits the
        // other three did not claim.
        //
        // Worth 3.6% over the bench corpus; see `bench/results/release-profile.md`.
        //
        // On an x86-64 target built with BMI2 (`-C target-cpu=native` or
        // `x86-64-v3`) each of the three seats is one `_pext_u64` instead, and
        // South is still the complement. That is 16% faster over the 200-deal
        // lock-step corpus on a Zen 3 Threadripper than the default x86-64
        // build. Zen 1 and Zen 2 run PEXT in microcode and should not be built
        // with BMI2 enabled.
        #[cfg(target_feature = "bmi2")]
        let packed = {
            // SAFETY: this block is only compiled when the target enables BMI2,
            // so the PEXT instruction is available on any CPU it runs on.
            let pext = |v: u64| unsafe { core::arch::x86_64::_pext_u64(v, all_value) };
            let mut packed = [0u64; NUM_SEATS];
            for seat in [WEST, NORTH, EAST] {
                packed[seat] = pext(hands[seat].value());
            }
            // A suit holds at most 13 cards, so the shift cannot overflow.
            let occupied = (1u64 << all_value.count_ones()) - 1;
            packed[SOUTH] = occupied & !(packed[WEST] | packed[NORTH] | packed[EAST]);
            packed
        };
        #[cfg(not(target_feature = "bmi2"))]
        let packed = {
            let mut packed = [0u64; NUM_SEATS];
            let mut m = all_value;
            let mut bit = 1u64;
            while m != 0 {
                let lowest = m & m.wrapping_neg(); // isolate lowest bit
                for seat in [WEST, NORTH, EAST] {
                    if hands[seat].value() & lowest != 0 {
                        packed[seat] |= bit;
                    }
                }
                bit <<= 1;
                m &= m - 1; // clear lowest bit
            }
            // `bit` is now `1 << (cards in the suit)`, so `bit - 1` is the mask
            // of occupied relative ranks.
            packed[SOUTH] = (bit - 1) & !(packed[WEST] | packed[NORTH] | packed[EAST]);
            packed
        };

        for (seat, &packed) in packed.iter().enumerate() {
            // Clear the suit and add relative cards
            self.hands[seat] = self.hands[seat].clear_suit(suit);
            let relative = Cards::from_bits(packed << (suit * NUM_RANKS));
            self.hands[seat] = self.hands[seat].union(relative);
        }
    }

    /// Compute relative hands for all suits
    pub fn compute(&mut self, hands: &Hands, all_cards: Cards) {
        for suit in 0..NUM_SUITS {
            self.convert_suit(hands, suit, all_cards.suit(suit));
        }
    }

    /// Update relative hands after a trick (only recompute changed suits)
    pub fn update(&mut self, hands: &Hands, prev_all_cards: Cards, new_all_cards: Cards) {
        let changed = prev_all_cards.different(new_all_cards);
        let mut remaining = changed;
        while !remaining.is_empty() {
            let card = remaining.top();
            let suit = suit_of(card);
            remaining = remaining.clear_suit(suit);
            self.convert_suit(hands, suit, new_all_cards.suit(suit));
        }
    }
}

/// Compute pattern hands from relative hands and rank winners
pub fn compute_pattern_hands(
    relative_hands: &Hands,
    all_cards: Cards,
    rank_winners: Cards,
) -> (Hands, Cards) {
    let mut relative_rank_winners = Cards::new();
    let mut extended_rank_winners = Cards::new();

    for suit in 0..NUM_SUITS {
        let rw_suit = rank_winners.suit(suit);
        if rw_suit.is_empty() {
            continue;
        }

        // Find the bottom rank winner in relative terms
        let bottom_winner = rw_suit.bottom();
        let rel_bottom = relative_card_in_suit(bottom_winner, all_cards.suit(suit));

        // Find which seat has this relative card
        let mut actual_rel_bottom = rel_bottom;
        for seat in 0..NUM_SEATS {
            let rel_hand = relative_hands[seat].suit(suit);
            if rel_hand.have(rel_bottom) {
                // Extend to lowest equivalent card
                let suit_value = rel_hand.value() >> (suit * NUM_RANKS);
                let shift = rel_bottom - suit * NUM_RANKS + 1;
                if shift < 64 {
                    let above = suit_value >> shift;
                    actual_rel_bottom += above.trailing_ones() as usize;
                }
                // If it's the suit bottom, extend upward: to its highest
                // equivalent card, and then one rank higher.
                //
                // The reference counts the *run* of consecutive cards held at
                // and below this one, stopping at the first gap. Taking the
                // position of the highest card below it instead — which is
                // what this did — over-subtracts whenever that run is broken,
                // producing a pattern that retains cards the reference drops.
                let all_rel = relative_hands.all_cards().suit(suit);
                if actual_rel_bottom == all_rel.bottom() {
                    let idx = actual_rel_bottom - suit * NUM_RANKS;
                    let run = (!(suit_value << (63 - idx))).leading_zeros() as usize;
                    actual_rel_bottom = actual_rel_bottom.saturating_sub(run);
                }
                break;
            }
        }

        // Build relative rank winners for this suit
        let suit_mask = mask_of(suit);
        let rel_winners = Cards::from_bits(suit_mask).slice(0, actual_rel_bottom + 1);
        relative_rank_winners = relative_rank_winners.union(rel_winners);

        // Unpack to actual cards
        let packed = rel_winners.value() >> (suit * NUM_RANKS);
        let unpacked = unpack_bits(packed, all_cards.suit(suit).value());
        extended_rank_winners = extended_rank_winners.union(Cards::from_bits(unpacked));
    }

    // Pattern hands are relative hands intersected with relative rank winners
    let mut pattern_hands = Hands::default();
    for seat in 0..NUM_SEATS {
        pattern_hands[seat] = relative_hands[seat].intersect(relative_rank_winners);
    }

    (pattern_hands, extended_rank_winners)
}

/// Helper: get relative card index in a suit
fn relative_card_in_suit(card: usize, all_suit_cards: Cards) -> usize {
    let suit = suit_of(card);
    let rank = ACE - all_suit_cards.slice(0, card).size();
    card_of(suit, rank)
}

/// Helper: construct card from suit and rank
fn card_of(suit: Suit, rank: usize) -> usize {
    suit * NUM_RANKS + (ACE - rank)
}

const ACE: usize = 12;

/// A content digest of a cache, for finding where two solvers' caches part
/// company.
///
/// Two rules make the value comparable across independent implementations:
///
///  * **Order-independent across slots.** The digest XORs each live slot's
///    contribution, so table size, hash function and probe order -- all of
///    which differ between us and the C++ reference -- cannot affect it.
///  * **Structure-sensitive within a pattern.** A pattern is a tree and its
///    shape decides what `lookup` will match, so the walk is pre-order and
///    mixes each child's index. Two caches holding the same patterns in a
///    different tree shape are genuinely different caches and must not
///    collide.
///
/// `splitmix64`, so a reimplementation has something exact to match.
#[inline]
pub(crate) fn mix_for_digest(x: u64) -> u64 {
    mix(x)
}

#[inline]
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn pattern_digest(p: &Pattern, depth: u64) -> u64 {
    let mut h = mix(depth);
    for seat in 0..NUM_SEATS {
        h = mix(h ^ mix(p.hands[seat].value()));
    }
    h = mix(h ^ mix(p.bounds.lower as i64 as u64));
    h = mix(h ^ mix(p.bounds.upper as i64 as u64));
    for (i, child) in p.children.iter().enumerate() {
        h = mix(h ^ mix(pattern_digest(child, depth + 1) ^ (i as u64)));
    }
    h
}

fn pattern_nodes(p: &Pattern) -> usize {
    1 + p.children.iter().map(pattern_nodes).sum::<usize>()
}

impl PatternCache {
    /// One line per live entry, sorted by hash so the two implementations'
    /// differing slot layouts do not matter.
    ///
    /// The aggregate digest says *that* two caches differ; this says *which*
    /// entry, which is the difference between knowing there is a bug and being
    /// able to look at it.
    pub fn dump(&self, iter: usize) {
        let mut rows: Vec<(u64, usize, u64)> = self
            .entries
            .iter()
            .filter(|e| e.hash != 0)
            .map(|e| {
                (
                    e.hash,
                    pattern_nodes(&e.pattern),
                    pattern_digest(&e.pattern, 0),
                )
            })
            .collect();
        rows.sort_unstable();
        for (hash, nodes, digest) in rows {
            eprintln!(
                "CACHEENTRY: iter={iter} hash={hash:016x} nodes={nodes} digest={digest:016x}"
            );
        }
        // And the trees themselves, so a differing digest can be read rather
        // than merely detected.
        let mut sorted: Vec<&ShapeEntry> = self.entries.iter().filter(|e| e.hash != 0).collect();
        sorted.sort_unstable_by_key(|e| e.hash);
        for entry in sorted {
            Self::dump_tree(iter, entry.hash, &entry.pattern, 0);
        }
    }

    fn dump_tree(iter: usize, hash: u64, p: &Pattern, depth: usize) {
        eprintln!(
            "CACHETREE: iter={iter} hash={hash:016x} d={depth} hands=[{:x},{:x},{:x},{:x}] bounds=[{},{}]",
            p.hands[0].value(),
            p.hands[1].value(),
            p.hands[2].value(),
            p.hands[3].value(),
            p.bounds.lower,
            p.bounds.upper
        );
        for child in &p.children {
            Self::dump_tree(iter, hash, child, depth + 1);
        }
    }

    /// Table capacity, live entries, and total pattern nodes across them.
    ///
    /// For attributing memory: the table is `capacity * size_of::<ShapeEntry>()`
    /// contiguous bytes, while every pattern node beyond an entry's root owns a
    /// separate `Vec` allocation, and those are what the C++ reference pools.
    pub fn footprint(&self) -> (usize, usize, usize, usize) {
        let live = self.entries.iter().filter(|e| e.hash != 0).count();
        let nodes: usize = self
            .entries
            .iter()
            .filter(|e| e.hash != 0)
            .map(|e| pattern_nodes(&e.pattern))
            .sum();
        (
            self.entries.len(),
            live,
            nodes,
            std::mem::size_of::<ShapeEntry>(),
        )
    }

    /// Live entries, and a digest of their contents.
    pub fn digest(&self) -> (usize, u64) {
        let mut count = 0;
        let mut digest = 0u64;
        for entry in self.entries.iter() {
            if entry.hash == 0 {
                continue;
            }
            count += 1;
            digest ^= mix(entry.hash) ^ pattern_digest(&entry.pattern, 0);
        }
        (count, digest)
    }
}

/// Byte sizes of the pattern-tree types, for attributing memory.
///
/// Returns `(Pattern, its children vector, Hands)`. The middle one is the
/// reason the first is what it is: a `Vec` is three words, `PatternVec` two.
pub fn type_sizes() -> (usize, usize, usize) {
    (
        std::mem::size_of::<Pattern>(),
        std::mem::size_of::<PatternVec>(),
        std::mem::size_of::<Hands>(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four-calls-per-suit compress that `convert_suit` replaced, kept as
    /// the oracle for the single mask walk that took its place.
    ///
    /// On an x86-64 target with BMI2 this is one `_pext_u64` instruction, which
    /// is why it is worth keeping written down: there, four of these would beat
    /// the walk.
    #[allow(unknown_lints, clippy::manual_isolate_lowest_one)]
    fn pack_bits(source: u64, mask: u64) -> u64 {
        if source == 0 {
            return 0;
        }
        let mut packed = 0u64;
        let mut bit = 1u64;
        let mut m = mask;
        while m != 0 {
            let lowest = m & m.wrapping_neg(); // isolate lowest bit
            if source & lowest != 0 {
                packed |= bit;
            }
            bit <<= 1;
            m &= m - 1; // clear lowest bit
        }
        packed
    }

    /// The single mask walk in [`RelativeHands::convert_suit`] must agree, bit
    /// for bit, with the four independent [`pack_bits`] calls it replaced (see
    /// the oracle above) --
    /// including the South value, which it derives as the complement of the
    /// other three rather than computing.
    #[test]
    fn convert_suit_matches_four_pack_bits() {
        // A deterministic sweep of deals: deal the 13 cards of one suit out to
        // the four seats in every rotation of a handful of splits, so shapes
        // from 13-0-0-0 to 4-3-3-3 and every void combination are covered.
        let mut checked = 0;
        for suit in 0..NUM_SUITS {
            for split in 0..(1usize << 13) {
                // Two-bit-per-card seat assignment is 4^13; instead walk a
                // 13-bit pattern and derive four seats from it deterministically.
                let mut hands = Hands::new();
                let mut all = Cards::new();
                for rank in 0..NUM_RANKS {
                    // Drop some cards entirely: those are the ones already played.
                    if split & (1 << rank) == 0 && rank % 5 != 0 {
                        continue;
                    }
                    let card = suit * NUM_RANKS + rank;
                    let seat = (split.rotate_right(rank as u32) ^ rank) % NUM_SEATS;
                    hands[seat].add(card);
                    all.add(card);
                }

                let all_suit = all.suit(suit);
                let mut expected = Hands::new();
                for seat in 0..NUM_SEATS {
                    let packed = pack_bits(hands[seat].suit(suit).value(), all_suit.value());
                    expected[seat] = Cards::from_bits(packed << (suit * NUM_RANKS));
                }

                let mut actual = RelativeHands::default();
                actual.convert_suit(&hands, suit, all_suit);

                for seat in 0..NUM_SEATS {
                    assert_eq!(
                        actual.hands[seat], expected[seat],
                        "suit {suit} split {split:#015b} seat {seat}"
                    );
                }
                checked += 1;
            }
        }
        assert_eq!(checked, NUM_SUITS * (1 << 13));
    }
}
