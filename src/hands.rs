//! Four hands representation - allocation-free
//!
//! Uses a fixed-size array of Cards (4 × u64), no heap allocation.

use super::cards::*;
use super::types::*;

/// Four hands, one per seat - no heap allocation
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Hands {
    hands: [Cards; NUM_SEATS],
}

impl Hands {
    /// Create empty hands
    #[inline]
    pub const fn new() -> Self {
        Hands {
            hands: [Cards::new(); NUM_SEATS],
        }
    }

    /// Get hand for a seat
    #[inline]
    pub fn hand(&self, seat: Seat) -> Cards {
        self.hands[seat]
    }

    /// Get mutable reference to hand
    #[inline]
    pub fn hand_mut(&mut self, seat: Seat) -> &mut Cards {
        &mut self.hands[seat]
    }

    /// Get all cards across all hands
    #[inline]
    pub fn all_cards(&self) -> Cards {
        self.hands[WEST]
            .union(self.hands[NORTH])
            .union(self.hands[EAST])
            .union(self.hands[SOUTH])
    }

    /// Get cards for a partnership (seat and partner)
    #[inline]
    pub fn partnership_cards(&self, seat: Seat) -> Cards {
        self.hands[seat].union(self.hands[partner(seat)])
    }

    /// Get opponent cards
    #[inline]
    pub fn opponent_cards(&self, seat: Seat) -> Cards {
        self.hands[left_hand_opp(seat)].union(self.hands[right_hand_opp(seat)])
    }

    /// Get number of tricks (cards per hand)
    #[inline]
    pub fn num_tricks(&self) -> usize {
        self.hands[WEST].size()
    }

    /// True only when all four seats hold a full 13-card hand.
    ///
    /// PBN files legitimately carry placeholder deals — BridgeComposer writes
    /// `[Deal "N:... ... ... ..."]` for auction-only teaching boards — and those
    /// parse successfully into empty hands. Solving one yields an all-zero
    /// table, so callers that annotate files must skip incomplete deals rather
    /// than record a fabricated result.
    pub fn is_complete(&self) -> bool {
        self.hands.iter().all(|h| h.size() == 13)
    }

    /// Parse from PBN-style deal string
    /// Format: "N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72"
    /// Order after first seat: rotates clockwise (N E S W or W N E S, etc.)
    pub fn from_pbn(s: &str) -> Option<Self> {
        let mut hands = Hands::new();

        // Find the starting seat indicator
        let (start_seat, rest) = if s.len() >= 2 && s.chars().nth(1) == Some(':') {
            let seat_char = s.chars().next()?;
            (char_to_seat(seat_char)?, &s[2..])
        } else {
            (NORTH, s) // Default to North
        };

        // Split into four hands
        let hand_strs: Vec<&str> = rest.split_whitespace().collect();
        if hand_strs.len() != 4 {
            return None;
        }

        // Parse each hand in clockwise order starting from start_seat
        for (i, hand_str) in hand_strs.iter().enumerate() {
            let seat = (start_seat + i) % NUM_SEATS;
            hands.hands[seat] = parse_hand(hand_str)?;
        }

        Some(hands)
    }

    /// The deal as strict PBN: `N:` then North, East, South, West, suits in
    /// S.H.D.C order, ranks high to low, voids as empty suits. This is the
    /// form DDS reads, whatever spelling [`Hands::from_pbn`] accepted.
    pub fn to_pbn(&self) -> String {
        let mut out = String::from("N:");
        for (i, seat) in [NORTH, EAST, SOUTH, WEST].into_iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            for suit in 0..NUM_SUITS {
                if suit > 0 {
                    out.push('.');
                }
                for c in "AKQJT98765432".chars() {
                    if let Some(rank) = char_to_rank(c) {
                        if self.hands[seat].have(card_of(suit, rank)) {
                            out.push(c);
                        }
                    }
                }
            }
        }
        out
    }

    /// DDS's estimate of how hard a deal is to solve (`Scheduler::Fanout`).
    ///
    /// For each hand, the runs of touching ranks in each suit are counted
    /// (KT982 is three: K, T98, 2), and the hand's total is multiplied by one
    /// more than its number of voids; the four hands are summed. More runs and
    /// more voids mean more distinct cards to try at each turn, so a larger
    /// search. Only the order it gives is used, never the value.
    pub fn fanout(&self) -> u32 {
        let mut total = 0;
        for seat in 0..NUM_SEATS {
            let (mut runs, mut voids) = (0u32, 0u32);
            for suit in 0..NUM_SUITS {
                let mut in_run = false;
                let mut any = false;
                for c in "23456789TJQKA".chars() {
                    let held = char_to_rank(c)
                        .is_some_and(|rank| self.hands[seat].have(card_of(suit, rank)));
                    if held && !in_run {
                        runs += 1;
                    }
                    in_run = held;
                    any |= held;
                }
                if !any {
                    voids += 1;
                }
            }
            total += runs * (1 + voids);
        }
        total
    }

    /// Indices of `deals`, the hardest-looking first by [`Hands::fanout`];
    /// ties keep their original order.
    ///
    /// A batch's wall time is set by its last work item to finish. Handed out
    /// in file order, one very hard deal near the end starts late and every
    /// other thread waits for it; started first, it overlaps the rest of the
    /// batch. On 2,000 random deals over 64 threads that was 12.9 s against
    /// 9.2 s, the order DDS's scheduler uses for the same reason.
    pub fn hardest_first(deals: &[Hands]) -> Vec<usize> {
        let mut order: Vec<usize> = (0..deals.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(deals[i].fanout()));
        order
    }

    /// Parse from solver-style format (4 lines: N, W E, S with spaces between suits)
    /// Each hand has suits separated by spaces in S H D C order
    pub fn from_solver_format(n: &str, w: &str, e: &str, s: &str) -> Option<Self> {
        let mut hands = Hands::new();
        hands.hands[NORTH] = parse_hand_spaces(n)?;
        hands.hands[WEST] = parse_hand_spaces(w)?;
        hands.hands[EAST] = parse_hand_spaces(e)?;
        hands.hands[SOUTH] = parse_hand_spaces(s)?;
        Some(hands)
    }
}

impl std::ops::Index<Seat> for Hands {
    type Output = Cards;

    #[inline]
    fn index(&self, seat: Seat) -> &Self::Output {
        &self.hands[seat]
    }
}

impl std::ops::IndexMut<Seat> for Hands {
    #[inline]
    fn index_mut(&mut self, seat: Seat) -> &mut Self::Output {
        &mut self.hands[seat]
    }
}

impl std::fmt::Debug for Hands {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for seat in 0..NUM_SEATS {
            write!(f, "{}: {} ", seat_letter(seat), self.hands[seat])?;
        }
        Ok(())
    }
}

impl std::fmt::Display for Hands {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "        {}", self.hands[NORTH])?;
        writeln!(f, "{}        {}", self.hands[WEST], self.hands[EAST])?;
        writeln!(f, "        {}", self.hands[SOUTH])
    }
}

/// Parse a single hand from PBN format (SHDC separated by dots)
fn parse_hand(s: &str) -> Option<Cards> {
    let mut cards = Cards::new();
    let suits: Vec<&str> = s.split('.').collect();
    if suits.len() != 4 {
        return None;
    }

    for (suit, suit_str) in suits.iter().enumerate() {
        for c in suit_str.chars() {
            if c == '-' {
                continue; // Void marker
            }
            let rank = char_to_rank(c)?;
            cards.add(card_of(suit, rank));
        }
    }

    Some(cards)
}

/// Parse a single hand from solver format (suits separated by spaces, SHDC order)
/// Handles fewer than 4 suits by treating missing suits as voids.
fn parse_hand_spaces(s: &str) -> Option<Cards> {
    // Discard everything outside the rank alphabet first, exactly as the C++
    // reference's `ParseHand` does. That is what lets one parser read both the
    // plain form used by most of its deal files
    //
    //     AQT62 - Q97 AT832
    //
    // and the pretty form its freak deals use, where a suit symbol precedes
    // each holding:
    //
    //     ♠ AQT62 ♥ - ♦ Q97 ♣ AT832
    //
    // Splitting the second on whitespace without filtering yields eight tokens
    // rather than four, which is why those deals could not be read at all.
    const RANK_CHARS: &str = "AaKkQqJjTt1098765432Xx- ";
    let filtered: String = s.chars().filter(|c| RANK_CHARS.contains(*c)).collect();

    let mut cards = Cards::new();
    let suits: Vec<&str> = filtered.split_whitespace().collect();

    // Allow 1-4 suits; missing suits are voids
    if suits.is_empty() || suits.len() > 4 {
        return None;
    }

    for (suit, suit_str) in suits.iter().enumerate() {
        if *suit_str == "-" {
            continue; // Void
        }
        let mut chars = suit_str.chars();
        while let Some(c) = chars.next() {
            if c == '-' {
                continue;
            }
            // The reference spells the ten either `T` or `10`; `char_to_rank`
            // already reads `1` as the ten, so the `0` has to be eaten here.
            // A wildcard `x` needs the cards already dealt to resolve, which
            // this parser does not see, so it is rejected rather than guessed.
            if c == '1' && !chars.as_str().starts_with('0') {
                return None;
            }
            let rank = char_to_rank(c)?;
            if c == '1' {
                chars.next();
            }
            cards.add(card_of(suit, rank));
        }
    }

    Some(cards)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hands_basic() {
        let mut hands = Hands::new();
        assert_eq!(hands.num_tricks(), 0);

        // Add a card to North
        hands[NORTH].add(card_of(SPADE, ACE));
        assert_eq!(hands[NORTH].size(), 1);
        assert!(hands[NORTH].have(card_of(SPADE, ACE)));
    }

    #[test]
    fn test_hands_from_pbn() {
        let pbn = "N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72";
        let hands = Hands::from_pbn(pbn).expect("Should parse");

        // North should have AKQT3 of spades
        assert!(hands[NORTH].have(card_of(SPADE, ACE)));
        assert!(hands[NORTH].have(card_of(SPADE, KING)));
        assert!(hands[NORTH].have(card_of(SPADE, QUEEN)));
        assert!(hands[NORTH].have(card_of(SPADE, TEN)));
        assert!(hands[NORTH].have(card_of(SPADE, THREE)));

        // Each hand should have 13 cards
        assert_eq!(hands[NORTH].size(), 13);
        assert_eq!(hands[EAST].size(), 13);
        assert_eq!(hands[SOUTH].size(), 13);
        assert_eq!(hands[WEST].size(), 13);
    }

    #[test]
    fn test_partnership_cards() {
        let pbn = "N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72";
        let hands = Hands::from_pbn(pbn).expect("Should parse");

        let ns_cards = hands.partnership_cards(NORTH);
        assert_eq!(ns_cards.size(), 26);

        let ew_cards = hands.opponent_cards(NORTH);
        assert_eq!(ew_cards.size(), 26);
    }

    #[test]
    fn test_all_cards() {
        let pbn = "N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72";
        let hands = Hands::from_pbn(pbn).expect("Should parse");

        assert_eq!(hands.all_cards().size(), 52);
    }

    #[test]
    fn test_hands_from_solver_format() {
        let hands = Hands::from_solver_format(
            "AKQT3 J6 KJ42 95", // North
            "98 873 9653 QJ72", // West
            "652 AK42 AQ87 T4", // East
            "J74 QT95 T AK863", // South
        )
        .expect("Should parse");

        assert_eq!(hands[NORTH].size(), 13);
        assert_eq!(hands[WEST].size(), 13);
        assert_eq!(hands[EAST].size(), 13);
        assert_eq!(hands[SOUTH].size(), 13);
        assert_eq!(hands.all_cards().size(), 52);
    }

    #[test]
    fn fanout_counts_runs_and_weights_voids() {
        // Runs per hand, no voids: N AKQ-T-3, J-6, K-J-4-2, 9-5 = 11;
        // E 65-2, AK-4-2, A-Q-87, T-4 = 10; S J-7-4, Q-T9-5, T, AK-8-6-3 = 11;
        // W 98, 87-3, 9-65-3, QJ-7-2 = 9.
        let deal = "N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72";
        assert_eq!(Hands::from_pbn(deal).expect("deal").fanout(), 41);
        // Four solid suits: one run and three voids per hand, 1 * 4 each.
        let solid = "N:AKQJT98765432... .AKQJT98765432.. ..AKQJT98765432. ...AKQJT98765432";
        assert_eq!(Hands::from_pbn(solid).expect("deal").fanout(), 16);
        // Hardest first, ties in their original order.
        let order = Hands::hardest_first(&[
            Hands::from_pbn(solid).expect("deal"),
            Hands::from_pbn(deal).expect("deal"),
            Hands::from_pbn(solid).expect("deal"),
        ]);
        assert_eq!(order, vec![1, 0, 2]);
    }
}
