// The claim page's bookkeeping: where the hand stands at a point in the play,
// who may claim, and what a verdict means in words.
//
// Kept apart from the page so it can be tested without a browser or the engine.
// The engine decides the claims (`check_claim`); this only frames the question
// and the answer.

import { replay, remainingHands, trickNumberOf, TRICK_SIZE, nextToPlay } from './cardplay.js'
import { SUIT_SYMBOLS, formatCard, seatAtIndex, sideOf } from './cards.js'

const SEAT_NAMES = { N: 'North', E: 'East', S: 'South', W: 'West' }

/** A seat's name in full: `"North"`. */
export function seatName(seat) {
  return SEAT_NAMES[seat] || seat
}

/**
 * The seats that may claim: declarer and the two defenders. Dummy never claims;
 * declarer claims for both hands.
 */
export function claimersFor(declarer) {
  if (!SEAT_NAMES[declarer]) return []
  return [declarer, seatAtIndex(declarer, 1), seatAtIndex(declarer, 3)]
}

/**
 * The hand as it stands after the first `at` cards of `plays`.
 *
 * Returns `{ hands, trick, taken, nextSeat, legal, over }`: the cards each seat
 * still holds, the trick on the table (the cards of an unfinished trick, or
 * `null` at a trick boundary), each side's tricks so far, who plays next and
 * what they may play, and whether all thirteen tricks are done.
 */
export function positionAt(board, plays, at, trump) {
  const shown = plays.slice(0, at)
  const { tricks, seatOf } = replay(shown, board.leader, trump)
  const hands = remainingHands(board.hands, shown, seatOf, shown.length)
  const taken = { NS: 0, EW: 0 }
  for (const t of tricks) if (t.winner) taken[sideOf(t.winner)] += 1
  const last = tricks[tricks.length - 1]
  const trick = last && !last.complete ? { leader: last.leader, plays: last.plays } : null
  const next = nextToPlay(board.hands, shown, board.leader, trump)
  return {
    hands,
    trick,
    taken,
    nextSeat: next?.seat ?? null,
    legal: next?.legal ?? new Set(),
    over: !next,
  }
}

/**
 * The cards of an unfinished trick in words, `"South ♥5, West ♥A"`: the
 * stacked phone layout has no trick in the middle of the table.
 */
export function trickText(trick) {
  if (!trick?.plays?.length) return ''
  return trick.plays
    .map((p) => `${seatName(p.seat)} ${SUIT_SYMBOLS[p.suit]}${formatCard(p.rank)}`)
    .join(', ')
}

/** "Before the opening lead", "after trick 3", "trick 4, after 2 cards". */
export function describePosition(at) {
  if (at === 0) return 'before the opening lead'
  const inTrick = at % TRICK_SIZE
  if (inTrick === 0) return `after trick ${at / TRICK_SIZE}`
  const n = trickNumberOf(at)
  return `trick ${n}, after ${inTrick} ${inTrick === 1 ? 'card' : 'cards'}`
}

/**
 * The claim to suggest when a hand is opened or the claimer changes.
 *
 * A LIN record's `mc` is the number of tricks declarer ended with, whoever
 * claimed, so it is offered as declarer's claim and as the defenders' share of
 * the rest. Without one, every remaining trick: the claim players make most.
 */
export function suggestedTricks({ claim, declarer, claimer, won, left }) {
  if (claim != null && Number.isFinite(Number(claim))) {
    const declarerTotal = Number(claim)
    const total = sideOf(claimer) === sideOf(declarer) ? declarerTotal : 13 - declarerTotal
    return Math.min(won + left, Math.max(won, total))
  }
  return won + left
}

/**
 * The verdict in words. `result` is the engine's answer for `claimer` claiming
 * `total` tricks for their side.
 */
export function verdictText({ result, claimer, declarer, total }) {
  const who = seatName(claimer)
  const role = claimer === declarer ? 'declarer' : 'a defender'
  const tricks = `${total} ${total === 1 ? 'trick' : 'tricks'}`
  switch (result.verdict) {
    case 'valid':
      return {
        tone: 'good',
        title: 'Valid',
        text:
          `${who} (${role}) has a line that takes ${tricks} for their side however the ` +
          `cards ${who} cannot see are placed.`,
      }
    case 'invalid':
      return {
        tone: 'bad',
        title: 'Not valid',
        text:
          `Some placement of the cards ${who} cannot see beats every line ${who} could ` +
          `choose, so ${tricks} is not guaranteed. ` +
          (claimer === declarer
            ? 'It may still make double dummy: the claim fails when declarer would have to guess.'
            : "A defender's claim must also work whatever partner holds and however partner plays."),
      }
    default:
      return {
        tone: 'warn',
        title: 'Undecided',
        text:
          'The search ran out of its node budget before settling it. Treat that as not valid, ' +
          "as BBO's checker does; it happens mostly early in a hand, with many cards unseen.",
      }
  }
}
