import { describe, expect, it } from 'vitest'
import { bucket, embedOrigin, fineBucket, suppressed } from './telemetry.js'

describe('bucket', () => {
  /*
   * A precise millisecond timing is a high-entropy number tied to one device's
   * exact performance — a good fingerprint. Rounding keeps "is this device
   * slow, and how slow" answerable while discarding the resolution that would
   * make two records distinguishable.
   */
  it('rounds to the nearest 100ms', () => {
    expect(bucket(0)).toBe(0)
    expect(bucket(49)).toBe(0)
    expect(bucket(50)).toBe(100)
    expect(bucket(377)).toBe(400)
    expect(bucket(1234)).toBe(1200)
  })

  it('never emits a negative or non-finite bucket', () => {
    expect(bucket(-5)).toBe(0)
    expect(bucket(NaN)).toBe(0)
    expect(bucket(Infinity)).toBe(0)
    expect(bucket(undefined)).toBe(0)
    expect(bucket('nonsense')).toBe(0)
  })

  it('reads a numeric string, since timings arrive from several places', () => {
    expect(bucket('250')).toBe(300)
  })
})

describe('embedOrigin', () => {
  const win = (overrides) => ({
    location: {},
    document: { referrer: '' },
    ...overrides,
  })

  it('is empty when the page is top-level', () => {
    const w = win()
    w.self = w
    w.top = w
    expect(embedOrigin(w)).toBe('')
  })

  it('prefers ancestorOrigins, and takes only the origin', () => {
    // The host's full URL is none of our business and could itself carry a hand
    // in its query string.
    const w = win({ location: { ancestorOrigins: ['https://host.example/lesson?hand=SECRET'] } })
    expect(embedOrigin(w)).toBe('https://host.example')
  })

  it('falls back to the referrer where ancestorOrigins is absent (Firefox)', () => {
    const w = win({ document: { referrer: 'https://host.example/some/page' } })
    w.self = w
    w.top = {}
    expect(embedOrigin(w)).toBe('https://host.example')
  })

  /*
   * "Framed by someone we cannot name" and "not framed" are different facts.
   * Collapsing them to '' would overstate the top-level count.
   */
  it('reports unknown when framed with the referrer suppressed', () => {
    const w = win()
    w.self = w
    w.top = {}
    expect(embedOrigin(w)).toBe('unknown')
  })

  it('reports unknown when reading window.top throws (cross-origin)', () => {
    const w = win()
    w.self = w
    Object.defineProperty(w, 'top', {
      get() {
        throw new Error('cross-origin')
      },
    })
    expect(embedOrigin(w)).toBe('unknown')
  })

  it('survives a malformed referrer rather than throwing into the caller', () => {
    const w = win({ document: { referrer: 'not a url' } })
    w.self = w
    w.top = {}
    expect(() => embedOrigin(w)).not.toThrow()
  })

  it('is empty with no window at all, so it is safe under a test runner', () => {
    expect(embedOrigin(null)).toBe('')
  })
})

describe('the nolog opt-out', () => {
  /*
   * Browser testing drives the real site, so without this its page loads and
   * solves land in the public statistics beside real ones — which is exactly
   * what happened, and it could not be undone afterwards: the only field that
   * distinguished the automated runs was the browser version, and they were
   * interleaved with real traffic so no cut-off date separated them either.
   * Suppressing at source is the only thing that works, so this gate is
   * load-bearing rather than a convenience.
   */
  const win = (search) => ({ location: { search } })

  it('is off by default', () => {
    expect(suppressed(win(''))).toBe(false)
    expect(suppressed(win('?hand=abc'))).toBe(false)
  })

  it('takes the flag with or without a value', () => {
    expect(suppressed(win('?nolog'))).toBe(true)
    expect(suppressed(win('?nolog=1'))).toBe(true)
    expect(suppressed(win('?nolog=0'))).toBe(true)
  })

  it('takes the flag alongside a hand, which is how testing uses it', () => {
    expect(suppressed(win('?lin=pn%7Ca&nolog'))).toBe(true)
  })

  it('is not fooled by a parameter that merely contains the word', () => {
    expect(suppressed(win('?nologging=1'))).toBe(false)
    expect(suppressed(win('?x=nolog'))).toBe(false)
  })

  it('says nothing outside a browser rather than throwing', () => {
    expect(suppressed(null)).toBe(false)
  })
})

describe('fineBucket', () => {
  it('keeps milliseconds where most claims land', () => {
    expect(fineBucket(3.4)).toBe(3)
    expect(fineBucket(0.2)).toBe(0)
    expect(fineBucket(456)).toBe(460)
    expect(fineBucket(4321)).toBe(4300)
  })

  it('treats nonsense as zero', () => {
    expect(fineBucket(-1)).toBe(0)
    expect(fineBucket('x')).toBe(0)
    expect(fineBucket(Infinity)).toBe(0)
  })
})
