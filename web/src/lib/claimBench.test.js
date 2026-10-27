import { describe, expect, it } from 'vitest'
import fixture from './fixtures/claim-bench-v1.json'
import { expected, overall, platformName, resultsText, rowsOf, tally } from './claimBench.js'

// Mirrors `PRESETS` in src/claim_bench.rs. The page reads the real presets
// from the engine; this only has to agree for the fixture check below.
const QUICK = {
  name: 'quick',
  workloads: ['rest-dd', 'strong-dd', 'decl-dd', 'def-dd', 'decl-all'],
  sizes: [8, 10, 13],
  count: 10,
}
const FULL = { ...QUICK, name: 'full', sizes: [7, 8, 9, 10, 11, 12, 13], count: 30 }

describe('the native fixture', () => {
  it('has a result for every claim of both presets', () => {
    for (const preset of [QUICK, FULL]) {
      for (const { workload, size } of rowsOf(preset)) {
        for (let i = 0; i < preset.count; i++) {
          expect(expected(workload, size, i), `${workload}/${size} #${i}`).not.toBeNull()
        }
      }
    }
  })

  it('says nothing for a different budget', () => {
    expect(expected('rest-dd', 8, 0, fixture.budget + 1)).toBeNull()
  })
})

describe('tally', () => {
  const claim = (letter, nodes, ms, exp = { letter, nodes }) => ({ letter, nodes, ms, expected: exp })

  it('counts verdicts and sums nodes and time', () => {
    const t = tally([claim('V', 10, 1), claim('X', 20, 2), claim('?', 30, 3)])
    expect(t).toMatchObject({ claims: 3, valid: 1, invalid: 1, undecided: 1, nodes: 60, ms: 6 })
    expect(t.verdicts).toBe('VX?')
    expect(t.matches).toBe(true)
  })

  it('flags a node count that differs, even with the same verdict', () => {
    expect(tally([claim('V', 10, 1, { letter: 'V', nodes: 11 })]).matches).toBe(false)
  })

  it('has nothing to say without native results', () => {
    expect(tally([claim('V', 10, 1, null)]).matches).toBeNull()
  })

  it('carries a mismatch through to the overall result', () => {
    const good = tally([claim('V', 1, 1)])
    const bad = tally([claim('V', 1, 1, { letter: 'X', nodes: 1 })])
    expect(overall([good, bad]).matches).toBe(false)
  })
})

describe('resultsText', () => {
  it('lays rows out like bridge-claim bench, and says whether they matched', () => {
    const rows = [{ workload: 'rest-dd', size: 8 }]
    const tallies = [tally([{ letter: 'V', nodes: 5, ms: 1.5, expected: { letter: 'V', nodes: 5 } }])]
    const text = resultsText({
      preset: 'quick',
      budget: 2000000,
      version: '2.1.0',
      platform: 'Firefox 140 on Linux',
      rows,
      tallies,
    })
    expect(text).toContain('Firefox 140 on Linux')
    expect(text).toMatch(/rest-dd\s+8\s+1\s+0\s+0\s+5\s+1\.5\s+1\.50\s+V/)
    expect(text).toContain('match the native tool')
  })
})

describe('platformName', () => {
  it('names the browser before the system', () => {
    const ua =
      'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 ' +
      '(KHTML, like Gecko) Version/18.2 Safari/605.1.15'
    expect(platformName(ua)).toBe('Safari 18 on macOS')
  })
})
