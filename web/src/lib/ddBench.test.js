import { describe, expect, it } from 'vitest'
import fixture from './fixtures/dd-bench-v1.json'
import { expected, resultsText, rowsFor, tally } from './ddBench.js'

describe('the native fixture', () => {
  it('has a table and a node count for all 200 deals', () => {
    expect(fixture.nodes).toHaveLength(200)
    expect(fixture.tricks).toHaveLength(200)
    for (const t of fixture.tricks) expect(t).toMatch(/^[0-9a-d]{20}$/)
  })

  // The lock-step invariant from CLAUDE.md: the web fixture describes the same
  // search as the native corpus total.
  it('totals the lock-step node count', () => {
    expect(fixture.nodes.reduce((a, b) => a + b, 0)).toBe(296_689_028)
  })

  it('has nothing past the end', () => {
    expect(expected(200)).toBeNull()
    expect(expected(0)).toEqual({ tricks: fixture.tricks[0], nodes: fixture.nodes[0] })
  })
})

describe('rowsFor', () => {
  it('groups deals ten to a row', () => {
    expect(rowsFor(20)).toEqual([
      { first: 0, last: 9 },
      { first: 10, last: 19 },
    ])
    expect(rowsFor(25).at(-1)).toEqual({ first: 20, last: 24 })
  })
})

describe('tally', () => {
  const ok = (i, ms = 10) => ({ ...expected(i), ms, expected: expected(i) })

  it('matches when every table and count agrees', () => {
    const t = tally([ok(0), ok(1)])
    expect(t.matches).toBe(true)
    expect(t.deals).toBe(2)
    expect(t.nodes).toBe(fixture.nodes[0] + fixture.nodes[1])
  })

  it('catches a wrong table or node count', () => {
    expect(tally([ok(0), { ...ok(1), nodes: 1 }]).matches).toBe(false)
    expect(tally([{ ...ok(0), tricks: '0'.repeat(20) }]).matches).toBe(false)
  })

  it('says nothing with nothing to compare', () => {
    expect(tally([{ tricks: 'x', nodes: 1, ms: 1, expected: null }]).matches).toBeNull()
  })
})

describe('resultsText', () => {
  it('reads like the native tool and reports the match', () => {
    const results = Array.from({ length: 20 }, (_, i) => ({
      ...expected(i),
      ms: 5,
      expected: expected(i),
    }))
    const text = resultsText({ preset: 'quick', version: '2.1.0', platform: 'Chrome 1 on macOS', results })
    expect(text).toContain('preset quick (20 deals)')
    expect(text).toContain('    11-20')
    expect(text).toContain('tables and nodes match the native tool')
  })
})
