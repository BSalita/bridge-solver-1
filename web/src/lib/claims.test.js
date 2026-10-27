import { describe, expect, it } from 'vitest'
import { parseDealString } from './deal.js'
import {
  claimersFor,
  describePosition,
  positionAt,
  suggestedTricks,
  trickText,
  verdictText,
} from './claims.js'

// North declares 4S; East cashes the HA and HK.
const hands = parseDealString(
  'N:AKQT3.J6.KJ42.95 652.AK42.AQ87.T4 J74.QT95.T.AK863 98.873.9653.QJ72'
)
const PLAYS = ['HA', 'H5', 'H3', 'H6', 'HK', 'H9', 'H7', 'HJ']
const board = { hands, leader: 'E' }

describe('positionAt', () => {
  it('counts tricks and finds the next player at a trick boundary', () => {
    const p = positionAt(board, PLAYS, 8, 'S')
    expect(p.taken).toEqual({ NS: 0, EW: 2 })
    expect(p.nextSeat).toBe('E')
    expect(p.trick).toBeNull()
    expect(p.hands.E.hearts).toEqual(['4', '2'])
    expect(p.over).toBe(false)
  })

  it('shows an unfinished trick and only the cards played so far', () => {
    const p = positionAt(board, PLAYS, 6, 'S')
    expect(p.trick.plays.map((e) => e.code)).toEqual(['HK', 'H9'])
    expect(p.nextSeat).toBe('W')
    expect([...p.legal]).toEqual(['H8', 'H7'])
    expect(p.taken).toEqual({ NS: 0, EW: 1 })
    expect(trickText(p.trick)).toBe('East ♥K, South ♥9')
    expect(trickText(null)).toBe('')
  })

  it('starts with the opening leader', () => {
    const p = positionAt(board, PLAYS, 0, 'S')
    expect(p.nextSeat).toBe('E')
    expect(p.legal.size).toBe(13)
  })
})

describe('describePosition', () => {
  it('names the point in the play', () => {
    expect(describePosition(0)).toBe('before the opening lead')
    expect(describePosition(8)).toBe('after trick 2')
    expect(describePosition(9)).toBe('trick 3, after 1 card')
    expect(describePosition(42)).toBe('trick 11, after 2 cards')
  })
})

describe('claimersFor', () => {
  it('is declarer and the two defenders, never dummy', () => {
    expect(claimersFor('N')).toEqual(['N', 'E', 'W'])
    expect(claimersFor('W')).toEqual(['W', 'N', 'S'])
    expect(claimersFor(null)).toEqual([])
  })
})

describe('suggestedTricks', () => {
  it("offers the record's claim to declarer, and the rest to a defender", () => {
    const at = { claim: 7, declarer: 'W', won: 3, left: 4 }
    expect(suggestedTricks({ ...at, claimer: 'W' })).toBe(7)
    expect(suggestedTricks({ ...at, claimer: 'N', won: 5 })).toBe(6)
  })

  it('stays within what is still possible', () => {
    expect(suggestedTricks({ claim: 13, declarer: 'S', claimer: 'S', won: 2, left: 5 })).toBe(7)
    expect(suggestedTricks({ claim: 0, declarer: 'S', claimer: 'S', won: 2, left: 5 })).toBe(2)
  })

  it('offers every trick left when the record has no claim', () => {
    expect(suggestedTricks({ claim: null, declarer: 'S', claimer: 'E', won: 4, left: 9 })).toBe(13)
  })
})

describe('verdictText', () => {
  const ask = (verdict, claimer = 'N') =>
    verdictText({ result: { verdict }, claimer, declarer: 'N', total: 10 })

  it('gives each verdict a tone and says what it means', () => {
    expect(ask('valid').tone).toBe('good')
    expect(ask('valid').text).toContain('North (declarer)')
    expect(ask('invalid').tone).toBe('bad')
    expect(ask('invalid').text).toContain('guess')
    expect(ask('invalid', 'E').text).toContain('partner')
    expect(ask('undecided').tone).toBe('warn')
  })
})
