//! Cards bitboard representation
//!
//! Uses a 52-bit integer where each bit represents a card.
//! Cards are ordered by suit (SHDC) then rank (Ace high).
//! Bit 0 = Spade Ace, Bit 12 = Spade 2, Bit 13 = Heart Ace, etc.

use super::types::*;

/// Card index lookup tables (initialized at compile time via const fn or lazy)
/// suit_of[card] = suit, rank_of[card] = rank
/// card_of[suit][rank] = card index
///
/// Get suit of a card (0-51 -> 0-3)
#[inline]
pub fn suit_of(card: usize) -> Suit {
    card / NUM_RANKS
}

/// Get rank of a card (0-51 -> 0-12, where 12=Ace)
#[inline]
pub fn rank_of(card: usize) -> Rank {
    NUM_RANKS - 1 - (card % NUM_RANKS)
}

/// Get card index from suit and rank
#[inline]
pub fn card_of(suit: Suit, rank: Rank) -> usize {
    suit * NUM_RANKS + (NUM_RANKS - 1 - rank)
}

/// Get mask for a suit (13 bits)
#[inline]
pub fn mask_of(suit: Suit) -> u64 {
    0x1FFF << (suit * NUM_RANKS)
}

/// Get card name as string
pub fn name_of(card: usize) -> String {
    format!(
        "{}{}",
        suit_name(suit_of(card)).chars().next().unwrap(),
        rank_name(rank_of(card))
    )
}

/// Compare ranks: returns true if card1 has lower rank than card2
#[inline]
pub fn lower_rank(card1: usize, card2: usize) -> bool {
    card1 > card2
}

/// Compare ranks: returns true if card1 has higher rank than card2
#[inline]
pub fn higher_rank(card1: usize, card2: usize) -> bool {
    card1 < card2
}

/// Cards represented as a 52-bit bitboard
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Cards {
    bits: u64,
}

impl Cards {
    /// Create empty card set
    #[inline]
    pub const fn new() -> Self {
        Cards { bits: 0 }
    }

    /// Create from raw bits
    #[inline]
    pub const fn from_bits(bits: u64) -> Self {
        Cards { bits }
    }

    /// Get raw bits value
    #[inline]
    pub fn value(&self) -> u64 {
        self.bits
    }

    /// Count number of cards
    #[inline]
    pub fn size(&self) -> usize {
        self.bits.count_ones() as usize
    }

    /// Check if a card is present
    #[inline]
    pub fn have(&self, card: usize) -> bool {
        self.bits & (1u64 << card) != 0
    }

    /// Check if empty
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bits == 0
    }

    /// Get cards in a specific suit
    #[inline]
    pub fn suit(&self, suit: Suit) -> Cards {
        Cards::from_bits(self.bits & mask_of(suit))
    }

    /// Get highest card (lowest bit index = highest rank)
    #[inline]
    pub fn top(&self) -> usize {
        self.bits.trailing_zeros() as usize
    }

    /// Get lowest card (highest bit index = lowest rank)
    #[inline]
    pub fn bottom(&self) -> usize {
        63 - self.bits.leading_zeros() as usize
    }

    /// Get slice of cards between indices [begin, end)
    #[inline]
    pub fn slice(&self, begin: usize, end: usize) -> Cards {
        // `end` may be 64, "to the end"; `begin` must still apply then. (It
        // did not: the mask was all ones, which no caller noticed until Bob
        // Salita's LiftRunWinner asked for the cards below one.)
        let below_end = if end >= 64 { !0u64 } else { (1u64 << end) - 1 };
        let from_begin = if begin >= 64 { 0 } else { !0u64 << begin };
        Cards::from_bits(self.bits & below_end & from_begin)
    }

    /// Union of two card sets
    #[inline]
    pub fn union(&self, other: Cards) -> Cards {
        Cards::from_bits(self.bits | other.bits)
    }

    /// Intersection of two card sets
    #[inline]
    pub fn intersect(&self, other: Cards) -> Cards {
        Cards::from_bits(self.bits & other.bits)
    }

    /// Difference (cards in self but not in other)
    #[inline]
    pub fn different(&self, other: Cards) -> Cards {
        Cards::from_bits(self.bits & !other.bits)
    }

    /// Complement (all cards not in self)
    #[inline]
    pub fn complement(&self) -> Cards {
        Cards::from_bits(((1u64 << TOTAL_CARDS) - 1) ^ self.bits)
    }

    /// Check if self includes all cards in other
    #[inline]
    pub fn include(&self, other: Cards) -> bool {
        self.intersect(other) == other
    }

    /// Check if self strictly includes other (includes but not equal)
    #[inline]
    pub fn strictly_include(&self, other: Cards) -> bool {
        self.include(other) && self.bits != other.bits
    }

    /// Add a single card
    #[inline]
    pub fn add(&mut self, card: usize) -> &mut Self {
        self.bits |= 1u64 << card;
        self
    }

    /// Remove a single card
    #[inline]
    pub fn remove(&mut self, card: usize) -> &mut Self {
        self.bits &= !(1u64 << card);
        self
    }

    /// Add all cards from another set
    #[inline]
    pub fn add_cards(&mut self, other: Cards) -> &mut Self {
        self.bits |= other.bits;
        self
    }

    /// Remove all cards in another set
    #[inline]
    pub fn remove_cards(&mut self, other: Cards) -> &mut Self {
        self.bits &= !other.bits;
        self
    }

    /// Clear all cards in a suit (mutating)
    #[inline]
    pub fn clear_suit_mut(&mut self, suit: Suit) -> &mut Self {
        self.bits &= !mask_of(suit);
        self
    }

    /// Clear all cards in a suit (returns new Cards)
    #[inline]
    pub fn clear_suit(&self, suit: Suit) -> Cards {
        Cards::from_bits(self.bits & !mask_of(suit))
    }

    /// Calculate high card points
    pub fn points(&self) -> usize {
        let mut points = 0;
        for card in self.iter() {
            let rank = rank_of(card);
            if rank > TEN {
                points += rank - TEN;
            }
        }
        points
    }

    /// Iterate over cards (from highest to lowest)
    pub fn iter(&self) -> CardsIterator {
        CardsIterator { bits: self.bits }
    }
}

impl std::fmt::Debug for Cards {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cards(")?;
        for suit in 0..NUM_SUITS {
            let suit_cards = self.suit(suit);
            if !suit_cards.is_empty() {
                write!(f, "{}: ", suit_name(suit).chars().next().unwrap())?;
                for card in suit_cards.iter() {
                    write!(f, "{}", rank_name(rank_of(card)))?;
                }
                write!(f, " ")?;
            }
        }
        write!(f, ")")
    }
}

impl std::fmt::Display for Cards {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for suit in 0..NUM_SUITS {
            let suit_cards = self.suit(suit);
            write!(f, "{} ", suit_name(suit).chars().next().unwrap())?;
            if suit_cards.is_empty() {
                write!(f, "- ")?;
            } else {
                for card in suit_cards.iter() {
                    write!(f, "{}", rank_name(rank_of(card)))?;
                }
                write!(f, " ")?;
            }
        }
        Ok(())
    }
}

/// Iterator over cards in a Cards bitset
pub struct CardsIterator {
    bits: u64,
}

impl Iterator for CardsIterator {
    type Item = usize;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.bits == 0 {
            None
        } else {
            let card = self.bits.trailing_zeros() as usize;
            self.bits &= self.bits - 1; // Clear lowest set bit
            Some(card)
        }
    }
}

impl IntoIterator for Cards {
    type Item = usize;
    type IntoIter = CardsIterator;

    fn into_iter(self) -> Self::IntoIter {
        CardsIterator { bits: self.bits }
    }
}

impl IntoIterator for &Cards {
    type Item = usize;
    type IntoIter = CardsIterator;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_card_indices() {
        // Spade Ace should be card 0
        assert_eq!(card_of(SPADE, ACE), 0);
        assert_eq!(suit_of(0), SPADE);
        assert_eq!(rank_of(0), ACE);

        // Spade 2 should be card 12
        assert_eq!(card_of(SPADE, TWO), 12);
        assert_eq!(rank_of(12), TWO);

        // Heart Ace should be card 13
        assert_eq!(card_of(HEART, ACE), 13);
        assert_eq!(suit_of(13), HEART);

        // Club 2 should be card 51
        assert_eq!(card_of(CLUB, TWO), 51);
        assert_eq!(suit_of(51), CLUB);
        assert_eq!(rank_of(51), TWO);
    }

    #[test]
    fn test_cards_basic() {
        let mut cards = Cards::new();
        assert!(cards.is_empty());
        assert_eq!(cards.size(), 0);

        // Add spade ace
        cards.add(card_of(SPADE, ACE));
        assert!(!cards.is_empty());
        assert_eq!(cards.size(), 1);
        assert!(cards.have(card_of(SPADE, ACE)));

        // Add heart king
        cards.add(card_of(HEART, KING));
        assert_eq!(cards.size(), 2);

        // Remove spade ace
        cards.remove(card_of(SPADE, ACE));
        assert_eq!(cards.size(), 1);
        assert!(!cards.have(card_of(SPADE, ACE)));
    }

    #[test]
    fn test_cards_suit() {
        let mut cards = Cards::new();
        cards.add(card_of(SPADE, ACE));
        cards.add(card_of(SPADE, KING));
        cards.add(card_of(HEART, ACE));

        let spades = cards.suit(SPADE);
        assert_eq!(spades.size(), 2);
        assert!(spades.have(card_of(SPADE, ACE)));
        assert!(spades.have(card_of(SPADE, KING)));
        assert!(!spades.have(card_of(HEART, ACE)));
    }

    #[test]
    fn test_cards_top_bottom() {
        let mut cards = Cards::new();
        cards.add(card_of(SPADE, ACE)); // card 0
        cards.add(card_of(SPADE, TWO)); // card 12
        cards.add(card_of(HEART, KING)); // card 14

        assert_eq!(cards.top(), card_of(SPADE, ACE));
        assert_eq!(cards.bottom(), card_of(HEART, KING));
    }

    #[test]
    fn test_cards_iteration() {
        let mut cards = Cards::new();
        cards.add(card_of(SPADE, ACE));
        cards.add(card_of(SPADE, KING));
        cards.add(card_of(HEART, ACE));

        let collected: Vec<_> = cards.iter().collect();
        assert_eq!(collected.len(), 3);
        // Should be in order from top (highest) to bottom (lowest)
        assert_eq!(collected[0], card_of(SPADE, ACE));
        assert_eq!(collected[1], card_of(SPADE, KING));
        assert_eq!(collected[2], card_of(HEART, ACE));
    }

    #[test]
    fn test_cards_points() {
        let mut cards = Cards::new();
        cards.add(card_of(SPADE, ACE)); // 4 HCP
        cards.add(card_of(SPADE, KING)); // 3 HCP
        cards.add(card_of(HEART, QUEEN)); // 2 HCP
        cards.add(card_of(HEART, JACK)); // 1 HCP
        cards.add(card_of(HEART, TEN)); // 0 HCP

        assert_eq!(cards.points(), 10);
    }

    #[test]
    fn test_cards_union_intersect() {
        let mut a = Cards::new();
        a.add(card_of(SPADE, ACE));
        a.add(card_of(SPADE, KING));

        let mut b = Cards::new();
        b.add(card_of(SPADE, KING));
        b.add(card_of(HEART, ACE));

        let union = a.union(b);
        assert_eq!(union.size(), 3);

        let intersect = a.intersect(b);
        assert_eq!(intersect.size(), 1);
        assert!(intersect.have(card_of(SPADE, KING)));
    }

    #[test]
    fn test_higher_lower_rank() {
        let ace = card_of(SPADE, ACE); // card 0
        let king = card_of(SPADE, KING); // card 1
        let two = card_of(SPADE, TWO); // card 12

        assert!(higher_rank(ace, king));
        assert!(higher_rank(king, two));
        assert!(lower_rank(two, king));
        assert!(lower_rank(king, ace));
    }
}
