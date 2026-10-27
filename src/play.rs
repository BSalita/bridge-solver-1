//! The rules of card play, shared by everything that replays or searches a
//! hand: legal cards, trick winners, and play-record validation.
//!
//! The double-dummy search in `search.rs` keeps its own copy of the
//! trick-winner test on its hot path, so that this module can change without
//! touching the search that has to stay in lock-step with the C++ reference.

use super::cards::*;
use super::hands::Hands;
use super::types::*;

/// Get playable cards for current player
pub fn get_playable_cards(hands: &Hands, seat: Seat, lead_suit: Option<Suit>) -> Cards {
    let hand = hands[seat];

    if let Some(suit) = lead_suit {
        // Must follow suit if possible
        let suit_cards = hand.suit(suit);
        if !suit_cards.is_empty() {
            return suit_cards;
        }
    }

    // Can play any card
    hand
}

/// Does `card` beat `best`, the card currently winning a trick, given `trump`?
///
/// A different suit only wins by ruffing: `best` is always the led suit or an
/// earlier trump, so a discard in a third suit cannot win.
#[inline]
pub fn wins_over(card: usize, best: usize, trump: Suit) -> bool {
    if suit_of(card) == suit_of(best) {
        higher_rank(card, best)
    } else {
        trump < NOTRUMP && suit_of(card) == trump
    }
}

/// The seat that wins a complete trick, given its cards in play order and the
/// seat that led it.
pub fn trick_winner(trump: Suit, leader: Seat, cards: &[usize]) -> Seat {
    let mut best = 0;
    for (i, &card) in cards.iter().enumerate().skip(1) {
        if wins_over(card, cards[best], trump) {
            best = i;
        }
    }
    (leader + best) % NUM_SEATS
}

/// Why a card cannot be played.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IllegalPlay {
    /// The card is not in the hand.
    NotHeld,
    /// The hand holds the led suit but the card is not of it.
    Revoke,
}

/// Check that `card` can be played from `hand` to a trick led in `lead_suit`
/// (`None` when leading).
pub fn check_play(hand: Cards, card: usize, lead_suit: Option<Suit>) -> Result<(), IllegalPlay> {
    if !hand.have(card) {
        return Err(IllegalPlay::NotHeld);
    }
    match lead_suit {
        Some(led) if suit_of(card) != led && !hand.suit(led).is_empty() => Err(IllegalPlay::Revoke),
        _ => Ok(()),
    }
}
