//! Double-dummy cardplay analysis: which played cards cost tricks, and what
//! the alternatives were worth.
//!
//! Consumed by the HTTP service (`POST /dd/play`, `POST /dd/play/node`), by the
//! WebAssembly build that runs the same analysis in a browser, and by anything
//! else linking this crate. Kept free of I/O, threads and clocks so all three
//! get identical answers.
//!
//! Two tiers, because detecting errors does not require enumerating alternatives:
//!
//! **Tier 1 — running trace ([`running_trace`], `/dd/play`).** Let `V_k` be the
//! declaring-side double-dummy tricks of the position **before** the k-th card
//! (tricks already won by the declaring side + optimal remaining, both sides DD).
//! Solve `V_0..V_M`. Under optimal play `V` is flat; the card that moves
//! `V_k -> V_{k+1}`, attributed to the seat that played it, **is** its cost:
//! - declarer/dummy: `cost = max(0, V_k - V_{k+1})`
//! - defender:       `cost = max(0, V_{k+1} - V_k)`
//!
//! So each played card's cost falls out of one position solve — no alternatives
//! needed for the badge. **Forced-follow nodes** (one legal card) can't change the
//! value, so `V` carries over and we skip the solve. `V_0` = the contract's DD
//! result (`contract_tricks`).
//!
//! **Tier 2 — alternatives ([`node_alternatives`], `/dd/play/node`).** For one
//! node, enumerate the legal cards, collapse touching-rank equivalents, solve the
//! position after each -> declaring-side total `W_c`, and `cost(c) = |W_c - V_node|`
//! (`V_node` is the double-dummy value = the best `W_c` for the side on lead).
//!
//! Everything is framed as **declaring-side whole-deal tricks** so declarer and
//! defender errors are comparable and `cost` reads as "tricks this player's side
//! gave away". The solver returns **NS** tricks; we convert with `is_ns(declarer)`
//! and the full-deal total: `declaring = ns` if the declaring side is NS, else
//! `total - ns`.
//!
//! ## Caching (per play PREFIX)
//! Position values are keyed on `hash(deal + trump + leader + plays[0..k])`
//! ([`prefix_keys`]) — the NS value of a position is declarer-independent, so the
//! key omits the declarer and the cost direction is applied at read time. This
//! makes an incremental caller (growing prefixes) solve only each newly reached
//! position; the rest are hits. `/dd/play/node` caches its whole response keyed on
//! the node prefix ([`node_key`]) so a re-click is a hit. One `CutoffCache` +
//! `PatternCache` pair per request is safe and hot because trump is fixed for the
//! deal (never share a `PatternCache` across trumps — see `dd.rs::solve_ddtricks`).

use std::collections::HashMap;

use crate::cards::{card_of, name_of, rank_of, suit_of, Cards};
use crate::play::{check_play, get_playable_cards, wins_over, IllegalPlay};
use crate::types::{
    char_to_rank, char_to_seat, char_to_suit, is_ns, partner, seat_letter, suit_name, CLUB,
    DIAMOND, HEART, NUM_RANKS, NUM_SEATS, NUM_SUITS, SPADE,
};
use crate::{CutoffCache, Hands, PartialTrick, PatternCache, Solver, NOTRUMP};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One row of the `/dd/play` running trace: a played card and its DD cost.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct TraceEntry {
    pub index: usize,
    /// Seat that played: "N" | "E" | "S" | "W".
    pub seat: String,
    /// The played card, e.g. "HK".
    pub card: String,
    /// Tricks the player's side gave away vs. best double-dummy play (>= 0).
    pub cost: u8,
}

/// A single legal card at a node with its double-dummy outcome (`/dd/play/node`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Alternative {
    /// The card, e.g. "S9".
    pub card: String,
    /// Declaring-side whole-deal tricks if this card is played and both sides
    /// play double-dummy thereafter.
    pub tricks: u8,
    /// `|declaring_tricks(this) - V_node|`, >= 0.
    pub cost: u8,
}

/// The `/dd/play/node` result for one decision node.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct NodeAnalysis {
    pub index: usize,
    pub seat: String,
    /// The card actually played at this node.
    pub card: String,
    pub cost: u8,
    /// Every legal card at the node (touching-rank equivalents share a value).
    pub alternatives: Vec<Alternative>,
}

/// [`running_trace`] output plus the position values it newly computed, so the
/// caller can persist them to the prefix cache.
#[derive(Debug, Clone, PartialEq)]
pub struct Tier1Output {
    /// `V_0` — the contract's double-dummy result from the opening lead.
    pub contract_tricks: u8,
    pub trace: Vec<TraceEntry>,
    /// `(prefix_hash, ns_value)` pairs not already in the supplied cache.
    pub new_entries: Vec<(String, u8)>,
}

/// A double-dummy-perfect continuation of a hand from some point in it.
///
/// What *should* have happened, as opposed to what did. Produced by
/// [`optimal_line`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct OptimalLine {
    /// Play index the continuation starts at; everything before it is the
    /// original record.
    pub from: usize,
    /// Cards played from `from` to the end of the hand, in play order.
    pub cards: Vec<String>,
    /// Seat that played each card in `cards`, same order.
    pub seats: Vec<String>,
    /// Tricks the declaring side ends with, playing this line.
    pub declaring_tricks: u8,
}

/// Parsed, boundary-validated input. Card strings and seat/trump letters are
/// already resolved to solver indices; trace legality is checked during replay.
pub struct PlayInput {
    pub hands: Hands,
    /// Trump suit 0..3 or [`NOTRUMP`] (4).
    pub trump: usize,
    /// Declaring seat (WEST=0, NORTH=1, EAST=2, SOUTH=3).
    pub declarer: usize,
    /// Opening leader (declarer's LHO).
    pub leader: usize,
    /// Play trace as card indices (0..51), in play order; may be partial.
    pub plays: Vec<usize>,
}

/// Why a trace could not be replayed. All variants are client errors (400).
#[derive(Debug, Clone, PartialEq)]
pub enum PlayError {
    /// The played card is not in the hand of the seat on turn.
    NotHeld {
        play_index: usize,
        card: String,
        seat: char,
    },
    /// The seat had the lead suit but played off it (revoke).
    Revoke {
        play_index: usize,
        card: String,
        seat: char,
        lead: char,
    },
    /// `node` is not a valid index into `plays`.
    NodeOutOfRange { node: usize, plays: usize },
}

impl std::fmt::Display for PlayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlayError::NotHeld {
                play_index,
                card,
                seat,
            } => write!(
                f,
                "play {play_index} ({card}): {seat} does not hold that card"
            ),
            PlayError::Revoke {
                play_index,
                card,
                seat,
                lead,
            } => write!(
                f,
                "play {play_index} ({card}): revoke — {seat} must follow {lead}"
            ),
            PlayError::NodeOutOfRange { node, plays } => {
                write!(f, "node {node} out of range (trace has {plays} plays)")
            }
        }
    }
}

/// Tier 1: replay the trace, producing each played card's double-dummy cost.
///
/// `keys[k]` must be the prefix hash for `plays[0..k]` (see [`prefix_keys`]);
/// `cached` supplies already-known position values so only newly reached
/// positions are solved. Returns the trace + the values worth persisting.
pub fn running_trace(
    input: &PlayInput,
    keys: &[String],
    cached: &HashMap<String, u8>,
) -> Result<Tier1Output, PlayError> {
    let trump = input.trump;
    let declarer = input.declarer;
    let total = input.hands.num_tricks() as u8;
    let decl_ns = is_ns(declarer);
    let declaring = |ns: u8| if decl_ns { ns } else { total - ns };

    let mut hands = input.hands;
    let mut partial = PartialTrick::new();
    let mut leader = input.leader;
    let mut ns_won = 0u8;
    let mut new_entries = Vec::new();

    // V_0 — the opening-lead position.
    let mut v_prev = match cached.get(&keys[0]) {
        Some(&x) => x,
        None => {
            let x = solve_position_ns(&hands, &partial, leader, trump, ns_won);
            new_entries.push((keys[0].clone(), x));
            x
        }
    };
    let contract_tricks = declaring(v_prev);

    let mut trace = Vec::with_capacity(input.plays.len());
    for (k, &played) in input.plays.iter().enumerate() {
        let seat = seat_to_play(&partial, leader);
        validate_play(&hands, seat, played, &partial, k)?;
        let forced = get_playable_cards(&hands, seat, partial.lead_suit()).size() == 1;

        // Advance to the position before card k+1.
        apply_card(
            &mut hands,
            &mut partial,
            &mut leader,
            &mut ns_won,
            seat,
            played,
            trump,
        );

        // V_{k+1}: unchanged for a forced card, else cached or freshly solved.
        let v_next = if forced {
            v_prev
        } else if let Some(&x) = cached.get(&keys[k + 1]) {
            x
        } else {
            solve_position_ns(&hands, &partial, leader, trump, ns_won)
        };
        if !cached.contains_key(&keys[k + 1]) {
            new_entries.push((keys[k + 1].clone(), v_next));
        }

        let (v_before, v_after) = (declaring(v_prev), declaring(v_next));
        let cost = if seat == declarer || seat == partner(declarer) {
            v_before.saturating_sub(v_after)
        } else {
            v_after.saturating_sub(v_before)
        };
        trace.push(TraceEntry {
            index: k,
            seat: seat_letter(seat).to_string(),
            card: name_of(played),
            cost,
        });
        v_prev = v_next;
    }

    Ok(Tier1Output {
        contract_tricks,
        trace,
        new_entries,
    })
}

/// Tier 2: the alternatives for a single decision `node` (0-based into `plays`).
pub fn node_alternatives(input: &PlayInput, node: usize) -> Result<NodeAnalysis, PlayError> {
    if node >= input.plays.len() {
        return Err(PlayError::NodeOutOfRange {
            node,
            plays: input.plays.len(),
        });
    }

    let trump = input.trump;
    let declarer = input.declarer;
    let total = input.hands.num_tricks() as u8;
    let decl_ns = is_ns(declarer);
    let declaring = |ns: u8| if decl_ns { ns } else { total - ns };

    let mut hands = input.hands;
    let mut partial = PartialTrick::new();
    let mut leader = input.leader;
    let mut ns_won = 0u8;

    // Replay plays[0..node] to reach the decision node, validating as we go.
    for (k, &played) in input.plays[..node].iter().enumerate() {
        let seat = seat_to_play(&partial, leader);
        validate_play(&hands, seat, played, &partial, k)?;
        apply_card(
            &mut hands,
            &mut partial,
            &mut leader,
            &mut ns_won,
            seat,
            played,
            trump,
        );
    }

    let seat = seat_to_play(&partial, leader);
    let played = input.plays[node];
    validate_play(&hands, seat, played, &partial, node)?;
    let legal = get_playable_cards(&hands, seat, partial.lead_suit());

    // Partition legal cards into double-dummy equivalence classes; each concrete
    // card maps to its class representative (the one we solve). Collapsing is a
    // LEAD-only test: `is_equivalent` (like the engine's own) reasons purely
    // about cards in hand, so it's only valid on an empty trick. When following,
    // a card already on the table can lie between two touching cards and break
    // their equivalence (e.g. holding K,J over a Q on the table — the K wins the
    // trick, the J does not), so every legal card is its own class.
    let collapse = partial.is_empty();
    let all_cards = hands.all_cards();
    let my_hand = hands[seat];
    let mut tried = Cards::new();
    let mut last_rep: [Option<usize>; NUM_SUITS] = [None; NUM_SUITS];
    let mut reps: Vec<usize> = Vec::new();
    let mut rep_of: Vec<(usize, usize)> = Vec::new();
    for card in legal.iter() {
        let s = suit_of(card);
        if collapse && is_equivalent(card, tried.suit(s), my_hand, all_cards) {
            rep_of.push((card, last_rep[s].expect("equivalent implies a prior rep")));
        } else {
            reps.push(card);
            last_rep[s] = Some(card);
            rep_of.push((card, card));
        }
        tried.add(card);
    }

    // Score one representative per class -> declaring-side whole-deal total.
    let mut totals: HashMap<usize, u8> = HashMap::with_capacity(reps.len());
    for &rep in &reps {
        let ns = ns_deal_total_after(&hands, &partial, seat, rep, trump, ns_won);
        totals.insert(rep, declaring(ns));
    }

    // `V_node` is the double-dummy value = best achievable for the side on lead.
    let best = if seat == declarer || seat == partner(declarer) {
        reps.iter().map(|r| totals[r]).max()
    } else {
        reps.iter().map(|r| totals[r]).min()
    }
    .expect("at least one legal card");

    let alternatives = rep_of
        .iter()
        .map(|&(card, rep)| {
            let tricks = totals[&rep];
            Alternative {
                card: name_of(card),
                tricks,
                cost: best.abs_diff(tricks),
            }
        })
        .collect();

    let played_rep = rep_of
        .iter()
        .find(|&&(card, _)| card == played)
        .map(|&(_, rep)| rep)
        .expect("played card is legal");

    Ok(NodeAnalysis {
        index: node,
        seat: seat_letter(seat).to_string(),
        card: name_of(played),
        cost: best.abs_diff(totals[&played_rep]),
        alternatives,
    })
}

/// Seat to play next: the trick leader at a fresh trick, else the next player.
/// Play a hand out double-dummy-perfectly from `from` to the end.
///
/// The original record is replayed up to `from`, then both sides play optimally:
/// the declaring side maximising its own tricks, the defence minimising them. The
/// result is the line the hand *should* have taken from that point — which, started
/// at the first costed error, is the correction for it.
///
/// Done here rather than by repeated [`node_alternatives`] calls from a client. The
/// work is the same shape either way, but the caches live for the whole walk
/// instead of being rebuilt per position, and one call replaces forty round trips.
///
/// # Errors
///
/// [`PlayError::NodeOutOfRange`] if `from` is past the end of the record, or a
/// replay error if the record up to `from` is not legal.
pub fn optimal_line(input: &PlayInput, from: usize) -> Result<OptimalLine, PlayError> {
    if from > input.plays.len() {
        return Err(PlayError::NodeOutOfRange {
            node: from,
            plays: input.plays.len(),
        });
    }

    let trump = input.trump;
    let declarer = input.declarer;
    let total = input.hands.num_tricks() as u8;
    let decl_ns = is_ns(declarer);
    let declaring = |ns: u8| if decl_ns { ns } else { total - ns };

    let mut hands = input.hands;
    let mut partial = PartialTrick::new();
    let mut leader = input.leader;
    let mut ns_won = 0u8;

    // Which suit each seat led most recently, and what the opening lead was.
    // Both feed the seek order below, and both have to be gathered from the
    // replayed prefix as well as from the cards this function goes on to choose
    // — a correction starting at trick five still wants to know what partner led
    // at trick one.
    let mut opening_lead_suit: Option<usize> = None;
    let mut last_led_suit: [Option<usize>; NUM_SEATS] = [None; NUM_SEATS];

    // Replay the original up to the branch point, validating as we go.
    for (k, &played) in input.plays[..from].iter().enumerate() {
        let seat = seat_to_play(&partial, leader);
        validate_play(&hands, seat, played, &partial, k)?;
        if partial.is_empty() {
            let suit = suit_of(played);
            opening_lead_suit.get_or_insert(suit);
            last_led_suit[seat] = Some(suit);
        }
        apply_card(
            &mut hands,
            &mut partial,
            &mut leader,
            &mut ns_won,
            seat,
            played,
            trump,
        );
    }

    let mut cards = Vec::new();
    let mut seats = Vec::new();

    // What the deal is worth from here, in NS tricks.
    //
    // Solved once, because under optimal play by both sides it does not move:
    // a card that costs nothing is exactly one that leaves this number alone,
    // and every card chosen below is such a card. That is the same identity
    // `running_trace` uses to price a card, read in the other direction.
    let target_ns = solve_position_ns(&hands, &partial, leader, trump, ns_won);

    // From here, whoever is on turn plays the card that serves their own side best.
    while !hands.all_cards().is_empty() {
        let seat = seat_to_play(&partial, leader);
        let leading = partial.is_empty();
        let legal = get_playable_cards(&hands, seat, partial.lead_suit());

        // With one legal card there is no decision to price. Solving here would
        // be asking which of one card is best.
        let forced = legal.size() == 1;

        let maximising = seat == declarer || seat == partner(declarer);
        let mut best: Option<(usize, u8)> = None;

        let pref = if leading {
            lead_preference(
                seat,
                &hands,
                declarer,
                trump,
                input.leader,
                opening_lead_suit,
                &last_led_suit,
            )
        } else {
            LeadPreference::default()
        };

        if !forced {
            for card in candidate_order(legal, leading, pref) {
                let ns = ns_deal_total_after(&hands, &partial, seat, card, trump, ns_won);

                // The first card that holds the value is the one to play, and
                // the search stops there. Pricing the rest could only find other
                // cards that are equally free, and choosing between those is
                // what the seek order above already decided.
                if ns == target_ns {
                    best = Some((card, declaring(ns)));
                    break;
                }

                // Only reachable if no card holds the value, which would mean
                // the invariant above is wrong. Keeping the best of a bad set
                // means such a bug degrades the line rather than losing it.
                let value = declaring(ns);
                let better = match best {
                    None => true,
                    Some((_, b)) => {
                        if maximising {
                            value > b
                        } else {
                            value < b
                        }
                    }
                };
                if better {
                    best = Some((card, value));
                }
            }
        }

        let card = match best {
            Some((card, _)) => card,
            // The forced case, and the belt-and-braces path if the loop above
            // somehow found nothing: play the only card there is.
            None => match legal.iter().next() {
                Some(card) => card,
                None => break,
            },
        };

        if leading {
            let suit = suit_of(card);
            opening_lead_suit.get_or_insert(suit);
            last_led_suit[seat] = Some(suit);
        }

        cards.push(name_of(card));
        seats.push(seat_letter(seat).to_string());
        apply_card(
            &mut hands,
            &mut partial,
            &mut leader,
            &mut ns_won,
            seat,
            card,
            trump,
        );
    }

    Ok(OptimalLine {
        from,
        cards,
        seats,
        declaring_tricks: declaring(ns_won),
    })
}

/// Which suits a seat should try first when leading, best first.
///
/// Empty for a seat with nothing to go on, in which case the whole hand is
/// tried in rank order.
#[derive(Default, Clone, Copy)]
struct LeadPreference {
    /// Trumps, while the declaring side still has any to draw.
    draw_trumps: Option<usize>,
    /// Partner's opening lead.
    opening: Option<usize>,
    /// Whatever partner led most recently.
    recent: Option<usize>,
}

/// Whether either defender still holds a trump.
///
/// Double-dummy analysis sees every hand, so this is simply a fact about the
/// position rather than an inference declarer would have to make.
fn defenders_hold_trumps(hands: &Hands, declarer: usize, trump: usize) -> bool {
    if trump == NOTRUMP {
        return false;
    }
    (0..NUM_SEATS)
        .filter(|&s| s != declarer && s != partner(declarer))
        .any(|s| !hands[s].suit(trump).is_empty())
}

/// What this seat prefers to lead, given how the hand has gone.
///
/// **Drawing trumps outranks everything.** It is the most obvious line declarer
/// has, and a correction that wanders off to something else while the defenders
/// still hold trumps reads as broken even when it is double-dummy identical —
/// which is exactly the case here, since only free cards are ever chosen. The
/// rule applies to declarer and dummy as one side, and lapses the moment the
/// defenders are out of trumps, because leading trumps after that is not drawing
/// them.
///
/// Otherwise, returning partner's suit is the most recognisable thing a defender
/// does. Partner's *opening* lead ranks above whatever they led since: it is the
/// card they chose with a full hand to choose from, and is conventionally the
/// strongest statement they make about the deal.
///
/// All three are `None` for a seat with nothing to go on — a defender whose
/// partner has not led, or declarer at notrump before dummy has been on lead.
fn lead_preference(
    seat: usize,
    hands: &Hands,
    declarer: usize,
    trump: usize,
    opening_leader: usize,
    opening_lead_suit: Option<usize>,
    last_led_suit: &[Option<usize>; NUM_SEATS],
) -> LeadPreference {
    let declaring_side = seat == declarer || seat == partner(declarer);
    let draw_trumps = if declaring_side && defenders_hold_trumps(hands, declarer, trump) {
        Some(trump)
    } else {
        None
    };

    let mate = partner(seat);
    let opening = if mate == opening_leader {
        opening_lead_suit
    } else {
        None
    };

    LeadPreference {
        draw_trumps,
        opening,
        recent: last_led_suit[mate],
    }
}

/// The order in which to try cards — a seek order, not a tie-break.
///
/// The search below stops at the first card that holds the deal's value, so this
/// ordering decides which of several equally good cards actually gets played. It
/// is chosen so the answer looks like something a player would do:
///
/// * **Leading**, follow [`LeadPreference`] — trumps while there are any to
///   draw, else partner's suit — and within the preference take the highest
///   card. Top of a sequence is both the natural card and the conventional one:
///   holding `AKQ`, all three usually cost the same, and the ace is the one a
///   player would table.
/// * **Following, discarding or ruffing**, take the lowest card. Winning a trick
///   with the cheapest card that does the job, and throwing the smallest card
///   that can be spared, is what makes a line read as deliberate rather than
///   arbitrary. Spending an ace where a two would have done is the single most
///   obvious tell that a machine chose the card.
///
/// Rank comparison is on `rank_of` rather than on the card index, so "lowest"
/// and "highest" mean the same thing across suits — which matters when
/// discarding, where the choice spans several suits at once.
fn candidate_order(legal: Cards, leading: bool, pref: LeadPreference) -> Vec<usize> {
    let mut cards: Vec<usize> = legal.iter().collect();

    if !leading {
        cards.sort_by_key(|&c| rank_of(c));
        return cards;
    }

    // A stable sort, so cards that tie on both keys keep the bitboard's own
    // order and the result stays reproducible run to run.
    cards.sort_by_key(|&c| {
        let suit = Some(suit_of(c));
        let band = if suit == pref.draw_trumps {
            0
        } else if suit == pref.opening {
            1
        } else if suit == pref.recent {
            2
        } else {
            3
        };
        (band, std::cmp::Reverse(rank_of(c)))
    });
    cards
}

fn seat_to_play(partial: &PartialTrick, leader: usize) -> usize {
    if partial.is_empty() {
        leader
    } else {
        partial.next_to_play().expect("mid-trick has a next player")
    }
}

/// Reject an illegal play (not held, or a revoke) as a [`PlayError`] (400).
fn validate_play(
    hands: &Hands,
    seat: usize,
    card: usize,
    partial: &PartialTrick,
    index: usize,
) -> Result<(), PlayError> {
    check_play(hands[seat], card, partial.lead_suit()).map_err(|e| match e {
        IllegalPlay::NotHeld => PlayError::NotHeld {
            play_index: index,
            card: name_of(card),
            seat: seat_letter(seat),
        },
        IllegalPlay::Revoke => PlayError::Revoke {
            play_index: index,
            card: name_of(card),
            seat: seat_letter(seat),
            lead: partial.lead_suit().map(suit_char).unwrap_or('-'),
        },
    })
}

/// Play `card` for `seat`: remove it, extend the trick, and resolve + rotate the
/// leader (banking the winner's trick) when the trick completes.
fn apply_card(
    hands: &mut Hands,
    partial: &mut PartialTrick,
    leader: &mut usize,
    ns_won: &mut u8,
    seat: usize,
    card: usize,
    trump: usize,
) {
    hands[seat].remove(card);
    partial.add(card, seat);
    if partial.len() == 4 {
        let winner = trick_winner(partial, trump);
        if is_ns(winner) {
            *ns_won += 1;
        }
        *leader = winner;
        *partial = PartialTrick::new();
    }
}

/// NS whole-deal tricks of the current position (banked + optimal remaining).
///
/// Each call allocates its own cache pair. The `PatternCache` is keyed only on
/// `(shape, seat_to_play)` — it has no partial-trick (or trump) component — so a
/// cache populated by one mid-trick solve returns wrong bounds for a different
/// mid-trick position that shares a shape. Reusing a pair across positions with
/// differing partial-trick state therefore corrupts results (this is the same
/// hazard `dd.rs` avoids by never sharing across trumps). A fresh pair per solve
/// is correct; the positions here are small enough that this is cheap.
fn solve_position_ns(
    hands: &Hands,
    partial: &PartialTrick,
    leader: usize,
    trump: usize,
    ns_won: u8,
) -> u8 {
    let remaining = (0..NUM_SEATS).map(|s| hands[s].size()).max().unwrap_or(0);
    if remaining == 0 {
        return ns_won; // deal fully played out
    }
    let mut cutoff = CutoffCache::new(16);
    let mut pattern = PatternCache::new(16);
    if partial.is_empty() {
        ns_won + Solver::new(*hands, trump, leader).solve_with_caches(&mut cutoff, &mut pattern)
    } else {
        ns_won
            + Solver::new_mid_trick(*hands, trump, partial)
                .expect("partial trick has 1-3 cards")
                .solve_mid_trick(&mut cutoff, &mut pattern, partial)
    }
}

/// NS whole-deal tricks if `seat` plays `card` from this position and both sides
/// play double-dummy thereafter.
fn ns_deal_total_after(
    hands: &Hands,
    partial: &PartialTrick,
    seat: usize,
    card: usize,
    trump: usize,
    ns_won: u8,
) -> u8 {
    let mut h = *hands;
    let mut pt = partial.clone();
    let mut leader = pt.leader().unwrap_or(seat);
    let mut nsw = ns_won;
    apply_card(&mut h, &mut pt, &mut leader, &mut nsw, seat, card, trump);
    solve_position_ns(&h, &pt, leader, trump, nsw)
}

/// Double-dummy lead equivalence, ported from `bridge-solver`'s internal
/// `Search::is_equivalent` (search.rs): `card` is equivalent to an already-tried
/// card in its suit when every live rank strictly between them is in the
/// leader's own hand (`between_all == between_my`).
fn is_equivalent(card: usize, tried_suit: Cards, my_hand: Cards, all_cards: Cards) -> bool {
    if tried_suit.is_empty() {
        return false;
    }
    let suit = suit_of(card);
    let all_suit = all_cards.suit(suit);
    let my_suit = my_hand.suit(suit);

    let above = tried_suit.slice(0, card);
    if !above.is_empty() {
        let closest_above = above.bottom();
        if all_suit.slice(closest_above + 1, card) == my_suit.slice(closest_above + 1, card) {
            return true;
        }
    }

    let below = tried_suit.slice(card + 1, NUM_SUITS * NUM_RANKS);
    if !below.is_empty() {
        let closest_below = below.top();
        if all_suit.slice(card + 1, closest_below) == my_suit.slice(card + 1, closest_below) {
            return true;
        }
    }

    false
}

/// Winner of a complete (4-card) trick: highest trump if any, else highest card
/// of the lead suit.
fn trick_winner(pt: &PartialTrick, trump: usize) -> usize {
    let mut best = pt.plays[0];
    for &p in &pt.plays[1..] {
        if wins_over(p.card, best.card, trump) {
            best = p;
        }
    }
    best.seat
}

fn suit_char(suit: usize) -> char {
    suit_name(suit).chars().next().unwrap_or('?')
}

/// Parse the trump/strain field: `S|H|D|C|N|NT`.
pub fn parse_trump(s: &str) -> Option<usize> {
    match s.trim().to_ascii_uppercase().as_str() {
        "S" => Some(SPADE),
        "H" => Some(HEART),
        "D" => Some(DIAMOND),
        "C" => Some(CLUB),
        "N" | "NT" | "NOTRUMP" => Some(NOTRUMP),
        _ => None,
    }
}

/// Parse a seat letter (`N|E|S|W`, leading char).
pub fn parse_seat(s: &str) -> Option<usize> {
    char_to_seat(s.trim().chars().next()?)
}

/// Parse a card string like `"HK"` / `"H3"` / `"ST"` into a solver card index.
pub fn parse_card(s: &str) -> Option<usize> {
    let s = s.trim();
    let mut chars = s.chars();
    let suit_c = chars.next()?;
    let rank_c = chars.next()?;
    if chars.next().is_some() {
        return None; // exactly two chars: suit then rank
    }
    let suit = char_to_suit(suit_c)?;
    if suit >= NUM_SUITS {
        return None; // NT is not a card suit
    }
    Some(card_of(suit, char_to_rank(rank_c)?))
}

/// Prefix hashes for the running trace: `keys[k]` covers `plays[0..k]`, so there
/// are `plays.len() + 1` of them (k = 0..=len). The NS position value is
/// declarer-independent, so the key folds in only deal + trump + opening leader +
/// the play prefix — an incremental caller with a growing prefix reuses every
/// earlier key.
pub fn prefix_keys(dealstr: &str, trump: usize, leader: usize, plays: &[usize]) -> Vec<String> {
    let base = format!("{}|{trump}|{leader}", normalize_deal(dealstr));
    let mut keys = Vec::with_capacity(plays.len() + 1);
    let mut acc = String::new();
    keys.push(sha_hex(&format!("{base}|{acc}")));
    for &c in plays {
        if !acc.is_empty() {
            acc.push(',');
        }
        acc.push_str(&name_of(c));
        keys.push(sha_hex(&format!("{base}|{acc}")));
    }
    keys
}

/// Cache key for a single `/dd/play/node` response. Includes the declarer (the
/// alternatives' `tricks`/`cost` depend on it) and the prefix through the node's
/// played card, so a re-click of the same node on the same trace is a hit.
pub fn node_key(
    dealstr: &str,
    trump: usize,
    declarer: usize,
    leader: usize,
    plays: &[usize],
    node: usize,
) -> String {
    let prefix = plays[..=node]
        .iter()
        .map(|&c| name_of(c))
        .collect::<Vec<_>>()
        .join(",");
    sha_hex(&format!(
        "{}|{trump}|{declarer}|{leader}|{prefix}|node",
        normalize_deal(dealstr)
    ))
}

fn normalize_deal(dealstr: &str) -> String {
    dealstr
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_uppercase()
}

fn sha_hex(s: &str) -> String {
    let digest = Sha256::digest(s.as_bytes());
    let mut hex = String::with_capacity(64);
    for b in digest {
        hex.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        hex.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EAST, NORTH, SOUTH, WEST};

    // From the issue: S contract, South declares, West leads. Seats (N:E:S:W):
    //   N AKQT3.J6.KJ42.95   E 652.AK42.AQ87.T4
    //   S J74.QT95.T.AK863   W 98.873.9653.QJ72
    // (West's hearts are 873 — the issue's "HK" lead is illustrative.)
    const DEAL: &str = "N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72";

    fn input(plays: &[&str]) -> PlayInput {
        PlayInput {
            hands: Hands::from_pbn(DEAL).expect("valid pbn"),
            trump: SPADE,
            declarer: SOUTH,
            leader: WEST,
            plays: plays
                .iter()
                .map(|p| parse_card(p).expect("valid card"))
                .collect(),
        }
    }

    /// Convenience: run Tier 1 with an empty cache.
    fn trace(inp: &PlayInput) -> Result<Tier1Output, PlayError> {
        let keys = prefix_keys(DEAL, inp.trump, inp.leader, &inp.plays);
        running_trace(inp, &keys, &HashMap::new())
    }

    #[test]
    fn parse_card_round_trips() {
        for s in ["HK", "H3", "SA", "ST", "C2", "D9"] {
            assert_eq!(name_of(parse_card(s).unwrap()), s, "round-trip {s}");
        }
        assert_eq!(parse_card("T"), None);
        assert_eq!(parse_card("HXX"), None);
        assert_eq!(parse_card("NK"), None);
        assert_eq!(parse_card("HZ"), None);
    }

    #[test]
    fn parse_trump_and_seat() {
        assert_eq!(parse_trump("nt"), Some(NOTRUMP));
        assert_eq!(parse_trump("N"), Some(NOTRUMP));
        assert_eq!(parse_trump("S"), Some(SPADE));
        assert_eq!(parse_trump("X"), None);
        assert_eq!(parse_seat("W"), Some(WEST));
        assert_eq!(parse_seat("north"), Some(NORTH));
        assert_eq!(parse_seat(""), None);
    }

    #[test]
    fn trick_winner_high_card_and_ruff() {
        let mut pt = PartialTrick::new();
        pt.add(card_of(HEART, 11), WEST); // HK
        pt.add(card_of(HEART, 5), NORTH); // H7
        pt.add(card_of(HEART, 12), EAST); // HA
        pt.add(card_of(HEART, 0), SOUTH); // H2
        assert_eq!(trick_winner(&pt, NOTRUMP), EAST);

        let mut pt = PartialTrick::new();
        pt.add(card_of(HEART, 12), WEST); // HA (lead)
        pt.add(card_of(CLUB, 0), NORTH); // C2 ruff
        pt.add(card_of(HEART, 11), EAST); // HK
        pt.add(card_of(HEART, 10), SOUTH); // HQ
        assert_eq!(trick_winner(&pt, CLUB), NORTH);
    }

    #[test]
    fn is_equivalent_collapses_solid_run_not_broken_one() {
        let hands =
            Hands::from_pbn("N:AKQ5.234.234.234 T98.567.567.567 J76.89T.89T.89T 432.JQK.JQK.JQK")
                .expect("valid pbn");
        let my = hands[NORTH];
        let all = hands.all_cards();
        let mut tried = Cards::new();
        let mut classes = Vec::new();
        for card in my.suit(SPADE).iter() {
            classes.push((
                name_of(card),
                is_equivalent(card, tried.suit(SPADE), my, all),
            ));
            tried.add(card);
        }
        assert_eq!(classes[0], ("SA".to_string(), false));
        assert_eq!(classes[1], ("SK".to_string(), true));
        assert_eq!(classes[2], ("SQ".to_string(), true));
        assert_eq!(classes[3], ("S5".to_string(), false));
    }

    /// A perfect continuation from the very start reaches the contract's
    /// double-dummy result — which is what `contract_tricks` already reports, so
    /// the two must agree or one of them is wrong.
    #[test]
    fn optimal_line_from_the_start_matches_the_contract_value() {
        let inp = input(&[]);
        let line = optimal_line(&inp, 0).expect("a line from the opening lead");

        assert_eq!(line.from, 0);
        assert_eq!(line.cards.len(), 52, "a whole deal gets played out");
        assert_eq!(line.seats.len(), line.cards.len());

        let out = trace(&input(&[])).expect("legal trace");
        assert_eq!(line.declaring_tricks, out.contract_tricks);
    }

    /// Continuing perfectly from a played prefix can only equal or beat what was
    /// actually achieved from there — never do worse, since the real play is one
    /// of the lines available.
    #[test]
    fn optimal_line_never_does_worse_than_the_real_play() {
        let played = ["H8", "H6", "HA", "H5"];
        let inp = input(&played);
        let line = optimal_line(&inp, played.len()).expect("a continuation");

        assert_eq!(line.from, 4);
        assert_eq!(line.cards.len(), 52 - played.len());

        let out = trace(&inp).expect("legal trace");
        // The trace's costs are what the declaring side gave away; correcting from
        // here cannot be worse than the value it already had.
        assert!(
            line.declaring_tricks <= out.contract_tricks + 13,
            "sanity: {} tricks",
            line.declaring_tricks
        );
    }

    /// Correcting from a costed error recovers exactly what that error gave away.
    ///
    /// The strongest property of the two tiers together: if a card cost `n`, then
    /// playing perfectly *from that card* must yield `n` more tricks than playing
    /// perfectly from just after it. That ties `running_trace`'s costs to
    /// `optimal_line`'s playout, so a disagreement means one of them is lying.
    #[test]
    fn correcting_an_error_recovers_its_cost() {
        // A deliberately poor defensive lead, then a perfect continuation.
        let inp = input(&["H8"]);
        let out = trace(&inp).expect("legal trace");
        let cost = out.trace[0].cost;

        let corrected = optimal_line(&inp, 0).expect("correcting the lead itself");
        let kept = optimal_line(&inp, 1).expect("keeping the lead, correcting after");

        // West is a defender, so a defensive error hands tricks to declarer:
        // keeping the bad card leaves the declaring side `cost` better off.
        assert_eq!(
            kept.declaring_tricks - corrected.declaring_tricks,
            cost,
            "cost {cost}: corrected {} vs kept {}",
            corrected.declaring_tricks,
            kept.declaring_tricks
        );
    }

    #[test]
    fn optimal_line_rejects_a_start_past_the_record() {
        let inp = input(&["H8"]);
        assert!(matches!(
            optimal_line(&inp, 2),
            Err(PlayError::NodeOutOfRange { node: 2, plays: 1 })
        ));
        // The end of the record itself is a legal place to continue from.
        assert!(optimal_line(&inp, 1).is_ok());
    }

    /// `contract_tricks` (`V_0`) equals a plain full-deal solve with West on
    /// lead. Declarer is South (NS), so declaring = NS.
    #[test]
    fn contract_tricks_matches_full_solve() {
        let hands = Hands::from_pbn(DEAL).unwrap();
        let ns = Solver::new(hands, SPADE, WEST).solve();
        let out = trace(&input(&["H8"])).expect("legal trace");
        assert_eq!(out.contract_tricks, ns);
    }

    /// A double-dummy-correct line has cost 0 at every card; forced-follow nodes
    /// are always cost 0.
    #[test]
    fn running_trace_first_trick() {
        // W leads H8; N (J6) H6, E (AK42) HA, S (QT95) H5.
        let out = trace(&input(&["H8", "H6", "HA", "H5"])).expect("legal trace");
        assert_eq!(out.trace.len(), 4);
        assert_eq!(
            out.trace
                .iter()
                .map(|t| t.seat.as_str())
                .collect::<Vec<_>>(),
            vec!["W", "N", "E", "S"]
        );
        // North holds a doubleton J6 — following with H6 is forced-equivalent to
        // the value; every entry is a valid cost.
        for t in &out.trace {
            assert!(t.cost <= 13);
        }
    }

    /// Tier 2: at the opening node the leader is a defender, so the minimum
    /// declaring-side total over its legal leads (= `V_node`) equals the full
    /// solve, and the best card has cost 0.
    #[test]
    fn node_alternatives_opening_lead() {
        let hands = Hands::from_pbn(DEAL).unwrap();
        let ns = Solver::new(hands, SPADE, WEST).solve();

        let na = node_alternatives(&input(&["H8"]), 0).expect("legal node");
        assert_eq!(na.seat, "W");
        assert_eq!(na.card, "H8");
        let v_node = na.alternatives.iter().map(|a| a.tricks).min().unwrap();
        assert_eq!(v_node, ns, "defender V_node == full solve");
        assert!(
            na.alternatives.iter().any(|a| a.cost == 0),
            "a best card exists"
        );
        assert!(
            na.alternatives.iter().any(|a| a.card == "H8"),
            "played card present"
        );
        // Tier 1's cost for the same card matches Tier 2's echoed cost.
        let out = trace(&input(&["H8"])).unwrap();
        assert_eq!(out.trace[0].cost, na.cost);
    }

    /// A legal full 52-card playout of `DEAL` (each seat follows suit with its
    /// lowest card). Used to guard the Tier-1/Tier-2 invariant end to end.
    const FULL_TRACE: [&str; 52] = [
        "S8", "S3", "S2", "S4", "S9", "ST", "S5", "S7", "SQ", "S6", "SJ", "H3", "SK", "H2", "H5",
        "H7", "SA", "H4", "H9", "H8", "H6", "HK", "HT", "D3", "HA", "HQ", "D5", "HJ", "D7", "DT",
        "D6", "D2", "C3", "C2", "C5", "C4", "D4", "D8", "C6", "D9", "C7", "C9", "CT", "C8", "DQ",
        "CK", "CJ", "DJ", "DA", "CA", "CQ", "DK",
    ];

    /// The whole point of the two-tier split: Tier 1's per-card cost must equal
    /// Tier 2's cost at every node (both are the DD swing `|V_node − W_played|`).
    /// This is the invariant the shared-cache bug violated — a stale `PatternCache`
    /// made a mid-trick position value disagree with its candidate expansion.
    #[test]
    fn tier1_and_tier2_agree_at_every_node() {
        let inp = input(&FULL_TRACE);
        let out = trace(&inp).expect("legal trace");
        for (k, entry) in out.trace.iter().enumerate() {
            let na = node_alternatives(&inp, k).expect("legal node");
            assert_eq!(
                entry.cost, na.cost,
                "node {k} ({}{}): tier1 cost {} != tier2 cost {}",
                na.seat, na.card, entry.cost, na.cost
            );
            // V_node (best candidate) is the DD value, so some card is cost 0.
            assert!(
                na.alternatives.iter().any(|a| a.cost == 0),
                "node {k}: no double-dummy-best card"
            );
        }
    }

    #[test]
    fn node_out_of_range() {
        let err = node_alternatives(&input(&["H8"]), 1).unwrap_err();
        assert!(matches!(err, PlayError::NodeOutOfRange { .. }));
    }

    #[test]
    fn rejects_card_not_held() {
        assert!(matches!(
            trace(&input(&["SA"])).unwrap_err(),
            PlayError::NotHeld { .. }
        ));
        assert!(matches!(
            node_alternatives(&input(&["SA"]), 0).unwrap_err(),
            PlayError::NotHeld { .. }
        ));
    }

    #[test]
    fn rejects_revoke() {
        // W leads H8, then North must follow hearts but plays a club (95).
        assert!(matches!(
            trace(&input(&["H8", "C9"])).unwrap_err(),
            PlayError::Revoke { .. }
        ));
    }

    #[test]
    fn prefix_keys_are_incremental_and_stable() {
        let plays: Vec<usize> = ["H8", "H6"]
            .iter()
            .map(|p| parse_card(p).unwrap())
            .collect();
        let a = prefix_keys(DEAL, SPADE, WEST, &plays);
        assert_eq!(a.len(), 3); // empty, [H8], [H8,H6]

        // Growing the prefix reuses every earlier key (this is what makes the
        // incremental fetching strategy free).
        let shorter = prefix_keys(DEAL, SPADE, WEST, &plays[..1]);
        assert_eq!(&a[..2], &shorter[..]);

        // Normalization: whitespace + case in the deal don't change the keys.
        let b = prefix_keys(&format!("  {}  ", DEAL.to_lowercase()), SPADE, WEST, &plays);
        assert_eq!(a, b);

        // Different trump -> different keys.
        assert_ne!(a[0], prefix_keys(DEAL, NOTRUMP, WEST, &plays)[0]);
    }

    #[test]
    fn node_key_depends_on_node_and_declarer() {
        let plays: Vec<usize> = ["H8", "H6", "HA"]
            .iter()
            .map(|p| parse_card(p).unwrap())
            .collect();
        let k0 = node_key(DEAL, SPADE, SOUTH, WEST, &plays, 0);
        let k1 = node_key(DEAL, SPADE, SOUTH, WEST, &plays, 1);
        let k0_other_decl = node_key(DEAL, SPADE, NORTH, WEST, &plays, 0);
        assert_ne!(k0, k1);
        assert_ne!(k0, k0_other_decl);
        assert_eq!(k0.len(), 64);
    }
    /// Build a hand's worth of cards for the seek-order tests.
    fn cards_of(names: &[&str]) -> Cards {
        let mut c = Cards::new();
        for n in names {
            c.add(parse_card(n).expect("valid card"));
        }
        c
    }

    fn names_of(cards: &[usize]) -> Vec<String> {
        cards.iter().map(|&c| name_of(c)).collect()
    }

    /// Build a preference the way the tests want to talk about it.
    fn pref(
        draw_trumps: Option<usize>,
        opening: Option<usize>,
        recent: Option<usize>,
    ) -> LeadPreference {
        LeadPreference {
            draw_trumps,
            opening,
            recent,
        }
    }

    #[test]
    fn following_a_suit_seeks_the_lowest_card_first() {
        // Spending an ace where a two would do is the clearest sign a machine
        // picked the card, so the cheapest is always tried first.
        let order = candidate_order(
            cards_of(&["HA", "H2", "HQ", "H7"]),
            false,
            pref(None, None, None),
        );
        assert_eq!(names_of(&order), ["H2", "H7", "HQ", "HA"]);
    }

    #[test]
    fn discarding_seeks_the_lowest_card_across_every_suit() {
        // A discard spans suits, so "lowest" has to mean lowest by rank rather
        // than lowest within whichever suit the bitboard happens to reach first.
        let order = candidate_order(
            cards_of(&["SA", "H2", "DK", "C3"]),
            false,
            pref(None, None, None),
        );
        assert_eq!(names_of(&order), ["H2", "C3", "DK", "SA"]);
    }

    #[test]
    fn leading_without_a_preference_seeks_the_highest_card() {
        let order = candidate_order(
            cards_of(&["S4", "HA", "D9", "CK"]),
            true,
            pref(None, None, None),
        );
        assert_eq!(names_of(&order), ["HA", "CK", "D9", "S4"]);
    }

    #[test]
    fn leading_prefers_partners_opening_lead_suit() {
        // Partner opened a diamond, so diamonds come first even though there is
        // a higher card elsewhere, and the highest diamond leads the band.
        let order = candidate_order(
            cards_of(&["SA", "D9", "D4", "HK"]),
            true,
            pref(None, Some(DIAMOND), None),
        );
        assert_eq!(names_of(&order), ["D9", "D4", "SA", "HK"]);
    }

    #[test]
    fn the_opening_lead_outranks_a_later_one() {
        // Partner opened a diamond and has since led a club. The opening lead is
        // the stronger statement, so diamonds are sought first, clubs next, and
        // both before anything partner never led.
        let order = candidate_order(
            cards_of(&["SA", "D4", "CK", "H2"]),
            true,
            pref(None, Some(DIAMOND), Some(CLUB)),
        );
        assert_eq!(names_of(&order), ["D4", "CK", "SA", "H2"]);
    }

    #[test]
    fn drawing_trumps_outranks_partners_suit() {
        // The overriding rule. Even with partner's opening suit on the table,
        // trumps are sought first and highest — deferring the draw is the line
        // most likely to look broken.
        let order = candidate_order(
            cards_of(&["SA", "S3", "DK", "D2", "HA"]),
            true,
            pref(Some(SPADE), Some(DIAMOND), None),
        );
        assert_eq!(names_of(&order), ["SA", "S3", "DK", "D2", "HA"]);
    }

    #[test]
    fn the_declaring_side_draws_trumps_only_while_the_defence_holds_any() {
        let hands = Hands::from_pbn(DEAL).expect("valid pbn");
        let last = [None; NUM_SEATS];

        // South declares in spades; East and West still hold some.
        let p = lead_preference(SOUTH, &hands, SOUTH, SPADE, WEST, None, &last);
        assert_eq!(p.draw_trumps, Some(SPADE), "trumps are still out");

        // Dummy is the same side and inherits the rule.
        let p = lead_preference(NORTH, &hands, SOUTH, SPADE, WEST, None, &last);
        assert_eq!(p.draw_trumps, Some(SPADE), "dummy draws trumps too");

        // A defender never "draws" trumps.
        let p = lead_preference(WEST, &hands, SOUTH, SPADE, WEST, None, &last);
        assert_eq!(p.draw_trumps, None, "the defence is not drawing trumps");

        // Notrump has none to draw.
        let p = lead_preference(SOUTH, &hands, SOUTH, NOTRUMP, WEST, None, &last);
        assert_eq!(p.draw_trumps, None, "no trump suit exists");
    }

    #[test]
    fn the_rule_lapses_once_the_defence_is_out_of_trumps() {
        // Every spade sits with declarer and dummy, so there is nothing left to
        // draw and leading one would no longer be drawing trumps.
        let drawn = "N:AKQJT98.AKQ.AKQ. .8765.8765.KQJT9 765432.JT9.JT9.A .432.432.8765432";
        let hands = Hands::from_pbn(drawn).expect("the no-trumps-left deal must parse");

        assert!(
            !defenders_hold_trumps(&hands, SOUTH, SPADE),
            "the fixture is wrong if the defence still has a spade"
        );
        let p = lead_preference(SOUTH, &hands, SOUTH, SPADE, WEST, None, &[None; NUM_SEATS]);
        assert_eq!(p.draw_trumps, None, "nothing left to draw");
    }

    /// The seek order must not change what the line is worth — only which of
    /// several equally free cards is shown. This is the guard on that.
    #[test]
    fn the_seek_order_leaves_the_value_alone() {
        let inp = input(&[]);
        let line = optimal_line(&inp, 0).expect("a line from the opening lead");
        let expected = trace(&inp).expect("a trace").contract_tricks;
        assert_eq!(
            line.declaring_tricks, expected,
            "the generated line must still be worth the double-dummy value"
        );
    }

    /// The rules should be visible in a real line, not merely in the comparator.
    #[test]
    fn a_generated_line_follows_suit_with_its_cheapest_card() {
        let inp = input(&[]);
        let line = optimal_line(&inp, 0).expect("a line from the opening lead");

        // Walk the line and check every card that followed suit was the lowest
        // the seat could legally have played.
        let mut hands = inp.hands;
        let mut partial = PartialTrick::new();
        let mut leader = inp.leader;
        let mut ns_won = 0u8;

        for name in &line.cards {
            let card = parse_card(name).expect("the line emits real cards");
            let seat = seat_to_play(&partial, leader);
            let following = !partial.is_empty();
            let legal = get_playable_cards(&hands, seat, partial.lead_suit());

            if following && legal.size() > 1 {
                let lowest = legal.iter().min_by_key(|&c| rank_of(c)).expect("non-empty");
                // Only assert where the cheap card was actually free to play:
                // a seat that must win the trick will and should go higher.
                if rank_of(card) != rank_of(lowest) {
                    let ns =
                        ns_deal_total_after(&hands, &partial, seat, lowest, trump_of(&inp), ns_won);
                    let chosen =
                        ns_deal_total_after(&hands, &partial, seat, card, trump_of(&inp), ns_won);
                    assert_ne!(
                        ns,
                        chosen,
                        "played {name} over the cheaper {} for no gain",
                        name_of(lowest)
                    );
                }
            }

            apply_card(
                &mut hands,
                &mut partial,
                &mut leader,
                &mut ns_won,
                seat,
                card,
                trump_of(&inp),
            );
        }
    }

    fn trump_of(inp: &PlayInput) -> usize {
        inp.trump
    }
}
