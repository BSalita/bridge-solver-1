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
// Only the PDEP and wasm builds call it outside the tests; elsewhere the
// search uses `unpack_suit_bits`'s branchless form.
#[cfg_attr(
    not(any(target_feature = "bmi2", target_arch = "wasm32")),
    allow(dead_code)
)]
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

/// Gather the bits of `source` at `mask`'s set positions into the low bits,
/// both confined to one suit's thirteen bits: PEXT on one suit. Upstream
/// 60c18a9's branchless compress (Hacker's Delight 7-4) with four stages,
/// enough for a sixteen-bit span.
#[inline]
#[cfg(not(target_arch = "wasm32"))]
pub fn pack_suit_bits(source: u64, mask: u64) -> u64 {
    #[cfg(target_feature = "bmi2")]
    {
        unsafe { core::arch::x86_64::_pext_u64(source, mask) }
    }
    #[cfg(not(target_feature = "bmi2"))]
    {
        let mut mask = mask;
        let mut source = source & mask;
        let mut mk = !mask << 1;
        for i in 0..4 {
            let mut mp = mk ^ (mk << 1);
            mp ^= mp << 2;
            mp ^= mp << 4;
            mp ^= mp << 8;
            let mv = mp & mask;
            mask = (mask ^ mv) | (mv >> (1 << i));
            let t = source & mv;
            source = (source ^ t) | (t >> (1 << i));
            mk &= !mp;
        }
        source
    }
}

/// [`unpack_bits`] for one suit: scatter the relative ranks `packed` (bit `j`
/// the suit's `j`-th highest card) onto the suit's cards in `all_cards`.
///
/// Without PDEP this is upstream 60c18a9's branchless expand (Hacker's
/// Delight 7-5) rather than a bit-at-a-time loop: the mask is shifted down to
/// the suit's own thirteen bits, so four stages suffice, and the result shifted
/// back up. A fixed sequence of shifts and masks in place of a loop whose
/// branches depend on the cards.
#[inline]
pub fn unpack_suit_bits(packed: u64, all_cards: Cards, suit: Suit) -> u64 {
    let shift = suit * NUM_RANKS;
    // PDEP, or on wasm the loop: upstream measured the branchless form ~10%
    // slower on narrow cores and routes its no-SIMD wasm build (which this
    // port's is) back to the loop.
    #[cfg(any(target_feature = "bmi2", target_arch = "wasm32"))]
    {
        let _ = shift;
        unpack_bits(packed, all_cards.suit(suit).value())
    }
    #[cfg(not(any(target_feature = "bmi2", target_arch = "wasm32")))]
    {
        const STAGES: usize = 4;
        let mask0 = all_cards.suit(suit).value() >> shift;
        let mut mask = mask0;
        let mut mk = !mask << 1;
        let mut mv = [0u64; STAGES];
        for (i, slot) in mv.iter_mut().enumerate() {
            let mut mp = mk ^ (mk << 1);
            mp ^= mp << 2;
            mp ^= mp << 4;
            mp ^= mp << 8;
            *slot = mp & mask;
            mask = (mask ^ *slot) | (*slot >> (1 << i));
            mk &= !mp;
        }
        let mut source = packed;
        for (i, &m) in mv.iter().enumerate().rev() {
            let t = source << (1 << i);
            source = (source & !m) | (t & m);
        }
        (source & mask0) << shift
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

    /// Whether `other` lies within these bounds: they are no tighter.
    pub fn include(&self, other: Bounds) -> bool {
        self.intersect(other) == other
    }

    /// Returns true if this bound causes a cutoff at the given beta
    pub fn cutoff(&self, beta: i8) -> bool {
        self.lower >= beta || self.upper < beta
    }
}

/// A pattern's four hands of relative cards as two bit planes and the cards
/// they hold, after Bob Salita's `PatHands` (b12bb3f).
///
/// The seat holding each card is two bits: `low` has the cards North or
/// South holds, `high` those East or South holds, so West is neither. That
/// makes testing a position against a pattern two XORs and a mask rather than
/// four set inclusions, and a pattern 24 bytes of hands rather than 32.
///
/// Bob numbers cards densely across the deck with `pext`. These keep the
/// relative-hands layout instead -- each suit in its own 13 bits, its top at
/// the lowest -- which every position of a shape shares as well, so the
/// planes come from relative hands by OR alone, with no bit packing on any
/// target.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct PatHands {
    low: u64,
    high: u64,
    cards: u64,
}

impl PatHands {
    /// The planes of four hands of relative cards.
    #[inline]
    pub fn from_hands(hands: &Hands) -> Self {
        let (w, n, e, s) = (
            hands[WEST].value(),
            hands[NORTH].value(),
            hands[EAST].value(),
            hands[SOUTH].value(),
        );
        PatHands {
            low: n | s,
            high: e | s,
            cards: w | n | e | s,
        }
    }

    /// The number of cards held.
    #[inline]
    pub fn size(&self) -> usize {
        self.cards.count_ones() as usize
    }

    /// The cards held, whoever holds them.
    #[inline]
    pub fn cards(&self) -> Cards {
        Cards::from_bits(self.cards)
    }

    /// Whether a position, which holds every card of its shape and comes as
    /// its two planes, has each of these cards in the same seat: the pattern
    /// covers it.
    #[inline]
    pub fn matches(&self, low: u64, high: u64) -> bool {
        (((self.low ^ low) | (self.high ^ high)) & self.cards) == 0
    }

    /// Whether every card here is in the same seat's hand in `other`: these
    /// hands are at least as general as `other`'s.
    #[inline]
    pub fn included_in(&self, other: &PatHands) -> bool {
        ((((self.low ^ other.low) | (self.high ^ other.high)) & self.cards)
            | (self.cards & !other.cards))
            == 0
    }

    /// The two planes, for [`Self::matches`].
    #[inline]
    pub fn planes(&self) -> (u64, u64) {
        (self.low, self.high)
    }

    /// The four hands back, for digests and diagnostics.
    pub fn hands(&self) -> Hands {
        let mut hands = Hands::default();
        hands[WEST] = Cards::from_bits(self.cards & !self.low & !self.high);
        hands[NORTH] = Cards::from_bits(self.cards & self.low & !self.high);
        hands[EAST] = Cards::from_bits(self.cards & !self.low & self.high);
        hands[SOUTH] = Cards::from_bits(self.cards & self.low & self.high);
        hands
    }
}

/// A Pattern stores hands with relative cards and bounds
/// Patterns form a tree where more specific patterns are children of more general ones
#[derive(Clone)]
pub struct Pattern {
    pub hands: PatHands,
    pub bounds: Bounds,
    /// How specific the pattern is: the cards it names across all four
    /// hands, fewer being broader. Every list is sorted by it, broadest first
    /// (upstream 38f5734; 205aaca began it with a looser heap order), and
    /// since [`Self::lookup_in`] returns the first match, it decides which
    /// pattern a hit finds.
    pub order: u16,
    pub children: PatternVec,
}

impl Default for Pattern {
    fn default() -> Self {
        Pattern {
            hands: PatHands::default(),
            bounds: Bounds::new(0, TOTAL_TRICKS as i8),
            order: 0,
            children: PatternVec::new(),
        }
    }
}

impl Pattern {
    /// A pattern of four hands of relative cards, with no sub-patterns.
    pub fn new(hands: Hands, bounds: Bounds) -> Self {
        let hands = PatHands::from_hands(&hands);
        Pattern {
            hands,
            bounds,
            order: hands.size() as u16,
            children: PatternVec::new(),
        }
    }

    pub fn reset(&mut self) {
        self.hands = PatHands::default();
        self.bounds = Bounds::new(0, TOTAL_TRICKS as i8);
        self.order = 0;
        self.children.clear();
    }

    /// Look up, in a list of patterns and their sub-patterns, the first that
    /// matches `new_pattern` and causes a cutoff at `beta`.
    ///
    /// A function of the list rather than a method of a pattern since
    /// upstream 255a7f7: a seat's top-level list in a [`ShapeEntry`] is not
    /// owned by any pattern.
    ///
    /// Takes the position as the two planes of its relative hands
    /// ([`PatHands::planes`]), passed by value so that they stay in
    /// registers down the recursion.
    pub fn lookup_in(list: &PatternVec, low: u64, high: u64, beta: i8) -> Option<&Pattern> {
        for child in list {
            if !child.hands.matches(low, high) {
                continue;
            }
            if child.bounds.cutoff(beta) {
                return Some(child);
            }
            if let Some(detail) = Pattern::lookup_in(&child.children, low, high, beta) {
                return Some(detail);
            }
        }
        None
    }

    /// Store `new_pattern` in a list of patterns, upstream 38f5734's way.
    ///
    /// Every list is kept sorted by [`Self::order`], most general first. The
    /// new pattern goes under the first pattern that includes it (or, with the
    /// same hands, only tightens that one's bounds). Failing that it takes its
    /// place in the order and adopts the more specific patterns after it:
    /// each becomes its child, unless its bounds are no tighter than the new
    /// pattern's, in which case it is dropped and its own children adopted.
    ///
    /// Bounds are not pushed down into sub-patterns any more, and nothing is
    /// lifted when they collapse; that and the loose heap order it replaces
    /// (205aaca's bubbling up) are gone.
    pub fn update_in(list: &mut PatternVec, mut new_pattern: Pattern) {
        let n = list.len();
        let mut pos = 0;
        // Go under the most general pattern including the new one.
        while pos < n && list[pos].order <= new_pattern.order {
            let pattern = &mut list[pos];
            if new_pattern.is_subset_of(pattern) {
                if new_pattern.hands_equal(pattern) {
                    pattern.bounds = pattern.bounds.intersect(new_pattern.bounds);
                } else {
                    Pattern::update_in(&mut pattern.children, new_pattern);
                }
                return;
            }
            pos += 1;
        }
        // Adopt the more specific patterns. Those kept stay in order in
        // `list[pos..kept]`; the slots after that hold what was taken.
        let mut sorted = true;
        let mut kept = pos;
        for i in pos..n {
            if !list[i].is_subset_of(&new_pattern) {
                list.as_mut_slice().swap(kept, i);
                kept += 1;
            } else if list[i].bounds.include(new_pattern.bounds) {
                // No tighter than the new pattern: drop it, keep its children.
                sorted &= list[i].children.is_empty();
                new_pattern.children.append(&mut list[i].children);
            } else {
                new_pattern.children.push(std::mem::take(&mut list[i]));
            }
        }
        if !sorted {
            Pattern::sort_by_order(&mut new_pattern.children);
        }
        // Put the new pattern at `pos`, the kept ones one slot later, and drop
        // the rest.
        if kept == n {
            list.push(new_pattern);
        } else {
            list[kept] = new_pattern;
        }
        list.as_mut_slice()[pos..=kept].rotate_right(1);
        list.truncate(kept + 1);
    }

    /// Insertion sort by [`Self::order`], as upstream's `Sort`: stable, and
    /// the lists it sees are short and nearly sorted.
    fn sort_by_order(list: &mut PatternVec) {
        let children = list.as_mut_slice();
        for i in 1..children.len() {
            let mut j = i;
            while j > 0 && children[j - 1].order > children[j].order {
                children.swap(j, j - 1);
                j -= 1;
            }
        }
    }

    /// Check if this pattern is a subset of (more specific than) another
    /// A pattern is more specific if each hand includes (is subset of) the other
    fn is_subset_of(&self, other: &Pattern) -> bool {
        other.hands.included_in(&self.hands)
    }

    /// Check if hands are equal
    fn hands_equal(&self, other: &Pattern) -> bool {
        self.hands == other.hands
    }

    /// Convert relative rank winners back to actual cards
    pub fn get_rank_winners(&self, all_cards: Cards) -> Cards {
        Self::rank_winners_of(self.hands.cards(), all_cards)
    }

    /// [`Self::get_rank_winners`] for a pattern's relative cards, without a
    /// pattern.
    pub fn rank_winners_of(relative_rank_winners: Cards, all_cards: Cards) -> Cards {
        let mut rank_winners = Cards::new();
        for suit in 0..NUM_SUITS {
            let rel_suit = relative_rank_winners.suit(suit);
            if rel_suit.is_empty() {
                continue;
            }
            let packed = rel_suit.value() >> (suit * NUM_RANKS);
            let unpacked = unpack_suit_bits(packed, all_cards, suit);
            rank_winners = rank_winners.union(Cards::from_bits(unpacked));
        }
        rank_winners
    }
}

/// ShapeEntry is what gets stored in the cache: one per shape, holding a
/// list of patterns for each seat to play (upstream bf4a521; the seat used to
/// be part of the key, one entry per shape and seat).
///
/// The lists are bare since upstream 255a7f7. Each was the children of a root
/// pattern whose own hands and bounds went unused once cf50f97 stopped
/// keeping a copy of the last match there; dropping it shrinks an entry from
/// four patterns to four lists.
///
/// Aligned to a cache line, which it fits now that [`PatternVec`] is twelve
/// bytes, so a lookup reads one line for the hash and the seat's list (Bob
/// Salita, b12bb3f).
#[derive(Default)]
#[repr(C, align(64))]
pub struct ShapeEntry {
    pub hash: u64,
    pub patterns: [PatternVec; NUM_SEATS],
}

impl ShapeEntry {
    pub fn reset(&mut self, hash: u64) {
        self.hash = hash;
        for list in &mut self.patterns {
            list.clear();
        }
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
    load_count: usize,
}

impl PatternCache {
    /// The table size most callers build at. Two bits under the cut-off
    /// cache's 16 because each entry holds four seats' trees since upstream
    /// bf4a521, which shrank its own default by two bits (15 to 13) for the
    /// same reason. The size cannot change what the search finds; see
    /// [`crate::search::CutoffCache::reset`].
    pub const DEFAULT_BITS: usize = 14;

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

    /// The slot key for a shape: upstream's `Cache::Hash`, one multiply.
    /// Since bf4a521 the seat to play picks a tree inside the entry rather
    /// than being part of the key.
    ///
    /// Public because callers hash once and pass the result to both
    /// [`Self::lookup`] and, on a miss, [`Self::get_or_create`]. The two used
    /// to hash independently, and the recursion between them puts the second
    /// call beyond anything the compiler could common up.
    pub fn hash_for(shape: u64) -> u64 {
        shape.wrapping_mul(0x9b8b4567327b23c7)
    }

    #[inline]
    fn index(&self, hash: u64) -> usize {
        // Use top bits for index (like C++). The mask is derived from the
        // length rather than stored beside it so that LLVM can see
        // `idx <= len - 1` and drop the bounds check on the probes below;
        // from a separate field it cannot know the two agree.
        (hash >> (64 - self.bits)) as usize & (self.entries.len() - 1)
    }

    /// Look up a shape entry: linear probing to the first empty slot, which
    /// always comes because entries are never removed and the table is resized
    /// at three quarters full. (Upstream 581cefe dropped the tracked probe
    /// distance that used to bound this.)
    pub fn lookup(&self, hash: u64) -> Option<&ShapeEntry> {
        let mask = self.entries.len() - 1;
        let mut idx = self.index(hash);
        loop {
            let entry = &self.entries[idx];
            if entry.hash == hash {
                return Some(entry);
            }
            if entry.hash == 0 {
                return None;
            }
            idx = (idx + 1) & mask;
        }
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

        // Native: upstream 60c18a9's branchless compress of each of West's,
        // North's and East's cards over the suit's own bits. South is not
        // compressed: the hands partition the suit, so South holds exactly the
        // relative ranks the other three did not claim.
        #[cfg(not(target_arch = "wasm32"))]
        let packed = {
            let shift = suit * NUM_RANKS;
            let mask = all_value >> shift;
            let mut packed = [0u64; NUM_SEATS];
            for seat in [WEST, NORTH, EAST] {
                packed[seat] = pack_suit_bits(hands[seat].value() >> shift, mask);
            }
            let occupied = (1u64 << mask.count_ones()) - 1;
            packed[SOUTH] = occupied & !(packed[WEST] | packed[NORTH] | packed[EAST]);
            packed
        };
        // wasm: one walk of the suit, card by card, for all four seats --
        // upstream keeps the loop for its no-SIMD wasm build, measured faster
        // there on narrow cores. (Before the branchless form this walk was the
        // native code too, and worth 3.6% over four separate compresses.)
        #[cfg(target_arch = "wasm32")]
        let packed = {
            let mut packed = [0u64; NUM_SEATS];
            let mut m = all_value;
            let mut bit = 1u64;
            while m != 0 {
                let lowest = m & m.wrapping_neg();
                for seat in [WEST, NORTH, EAST] {
                    if hands[seat].value() & lowest != 0 {
                        packed[seat] |= bit;
                    }
                }
                bit <<= 1;
                m &= m - 1;
            }
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
    // `m & m.wrapping_neg()`: see `convert_suit` for the lint.
    #[allow(unknown_lints, clippy::manual_isolate_lowest_one)]
    pub fn update(&mut self, hands: &Hands, prev_all_cards: Cards, new_all_cards: Cards) {
        let changed = prev_all_cards.different(new_all_cards);
        let mut remaining = changed;
        while !remaining.is_empty() {
            let card = remaining.top();
            let suit = suit_of(card);
            remaining = remaining.clear_suit(suit);
            // Upstream 31802f9: if every card left in the suit outranks every
            // card just played from it, the survivors keep relative positions
            // 0..k-1 and only the positions above them go -- no conversion.
            // Lower bit index is higher rank, so `removed & -removed` is the
            // highest card played, and a smaller value outranks it.
            let removed = changed.suit(suit).value();
            let survivors = new_all_cards.suit(suit).value();
            if survivors < (removed & removed.wrapping_neg()) {
                let keep = ((1u64 << survivors.count_ones()) - 1) << (suit * NUM_RANKS);
                let stale = Cards::from_bits(mask_of(suit) & !keep);
                for seat in 0..NUM_SEATS {
                    self.hands[seat] = self.hands[seat].different(stale);
                }
                continue;
            }
            self.convert_suit(hands, suit, new_all_cards.suit(suit));
        }
    }
}

/// Compute pattern hands from relative hands and rank winners: the cards a
/// stored pattern keeps, per suit, from the top of the suit down.
///
/// Bob Salita's `ComputePatternCards` (b12bb3f). The lowest rank winner is
/// extended down only through the runs of equivalent cards the proof relied
/// on (`runs`, rank positions), not through its holder's whole run as before.
/// Then, if that holder holds every card from there to the bottom of the
/// suit, the cards above already determine the suit, so the pattern stops
/// above them -- which also keeps the suit's bottom card, which cannot win by
/// rank, out of it.
///
/// Returns the pattern hands (relative) and the kept cards as actual cards.
pub fn compute_pattern_hands(
    relative_hands: &Hands,
    hands: &Hands,
    all_cards: Cards,
    rank_winners: Cards,
    runs: Cards,
) -> (Hands, Cards) {
    let mut relative_rank_winners = Cards::new();
    let mut extended_rank_winners = Cards::new();

    for suit in 0..NUM_SUITS {
        let rw_suit = rank_winners.suit(suit);
        if rw_suit.is_empty() {
            continue;
        }
        let all_suit = all_cards.suit(suit);
        // A run ends just above a card in play, so this stops at one, inside
        // the suit.
        let mut bottom_card = rw_suit.bottom();
        while runs.have(bottom_card) {
            bottom_card += 1;
        }
        debug_assert_eq!(suit_of(bottom_card), suit);
        // The number of cards kept, counting down from the top of the suit.
        let mut kept = all_suit.slice(0, bottom_card).size() + 1;
        let shift = suit * NUM_RANKS;
        for seat in 0..NUM_SEATS {
            if !hands[seat].have(bottom_card) {
                continue;
            }
            // The seat's cards of the suit as relative ranks, the top at bit 0.
            let suit_cards = relative_hands[seat].suit(suit).value() >> shift;
            let lowest = kept + (!(suit_cards >> kept)).trailing_zeros() as usize;
            if lowest == all_suit.size() {
                kept = lowest - (!(suit_cards << (64 - lowest))).leading_zeros() as usize;
            }
            break;
        }
        if kept == 0 {
            continue;
        }
        let rel_winners = Cards::from_bits(((1u64 << kept) - 1) << shift);
        relative_rank_winners = relative_rank_winners.union(rel_winners);
        extended_rank_winners = extended_rank_winners.union(Cards::from_bits(unpack_suit_bits(
            (1u64 << kept) - 1,
            all_cards,
            suit,
        )));
    }

    // Pattern hands are relative hands intersected with relative rank winners
    let mut pattern_hands = Hands::default();
    for seat in 0..NUM_SEATS {
        pattern_hands[seat] = relative_hands[seat].intersect(relative_rank_winners);
    }

    (pattern_hands, extended_rank_winners)
}

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
    let hands = p.hands.hands();
    for seat in 0..NUM_SEATS {
        h = mix(h ^ mix(hands[seat].value()));
    }
    h = mix(h ^ mix(p.bounds.lower as i64 as u64));
    h = mix(h ^ mix(p.bounds.upper as i64 as u64));
    for (i, child) in p.children.iter().enumerate() {
        h = mix(h ^ mix(pattern_digest(child, depth + 1) ^ (i as u64)));
    }
    h
}

/// An entry's four trees, each tagged with its seat so that swapping two
/// seats' trees changes the digest.
fn entry_digest(entry: &ShapeEntry) -> u64 {
    entry
        .patterns
        .iter()
        .enumerate()
        .fold(0, |d, (seat, list)| {
            let list_digest = list
                .iter()
                .enumerate()
                .fold(0, |h, (i, p)| h ^ mix(pattern_digest(p, 1) ^ i as u64));
            d ^ mix(list_digest ^ seat as u64)
        })
}

/// Patterns in an entry's four lists, sub-patterns included.
fn entry_nodes(entry: &ShapeEntry) -> usize {
    entry
        .patterns
        .iter()
        .flat_map(|list| list.iter())
        .map(pattern_nodes)
        .sum()
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
            .map(|e| (e.hash, entry_nodes(e), entry_digest(e)))
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
            for pattern in entry.patterns.iter().flat_map(|list| list.iter()) {
                Self::dump_tree(iter, entry.hash, pattern, 0);
            }
        }
    }

    fn dump_tree(iter: usize, hash: u64, p: &Pattern, depth: usize) {
        let hands = p.hands.hands();
        eprintln!(
            "CACHETREE: iter={iter} hash={hash:016x} d={depth} hands=[{:x},{:x},{:x},{:x}] bounds=[{},{}]",
            hands[0].value(),
            hands[1].value(),
            hands[2].value(),
            hands[3].value(),
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
            .map(entry_nodes)
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
            digest ^= mix(entry.hash) ^ entry_digest(entry);
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
