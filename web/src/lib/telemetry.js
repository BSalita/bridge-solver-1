// The telemetry beacon.
//
// One record when the page loads and one when a solve finishes, so the question
// "is anyone using this, and how slow is it on real hardware" has an answer.
// Everything here is shaped by one constraint, which is ADR-001:
//
//   **No identifier of any kind.** No account, no cookie, no random ID, no
//   fingerprint, no hash of the IP. Two records from the same person cannot be
//   linked together, by us or by anyone else. The consequence is that we cannot
//   count people, only events — a deliberate trade, and the reason no consent
//   banner is required.
//
// The second constraint is the load-bearing one for this site's whole claim:
// **nothing derived from the deal is ever sent.** Not the PBN or LIN text, not
// a handviewer URL, not the contract, not the double-dummy result, not a hash
// of any of them. The payload below is an explicit allowlist for exactly that
// reason — a spread of caller-supplied fields would make it one careless call
// site away from leaking a hand.
//
// Deliberately absent for the same reason: `deviceMemory`, `hardwareConcurrency`,
// screen dimensions, language, and the full user-agent string. The first two are
// the strongest passive fingerprinting signals the page has access to; they are
// read locally to size a worker pool and never transmitted. The server derives a
// coarse browser/OS family from the UA header it already receives.

import { capabilities, timings } from './perf.js'

/** Build-time version, injected by Vite. See `define` in vite.config.js. */
const APP_VERSION = typeof __APP_VERSION__ === 'string' ? __APP_VERSION__ : 'dev'

/**
 * Where the ingest Function lives. Same-origin, which `connect-src 'self'`
 * requires.
 *
 * RELATIVE, not `/t`. The site is served both from this project's own root and
 * mounted under a path at bridge-craftwork.com/bridge-solver/, and an absolute
 * `/t` resolved to the apex there rather than to the tool — so every beacon
 * 404'd, silently, while the solver itself kept working. Relative matches how
 * the rest of the build already addresses its assets (`base: './'` in the Vite
 * config), so it is correct from either address.
 */
const ENDPOINT = './t'

/**
 * Which site embedded us, or `''` when we are top-level.
 *
 * `ancestorOrigins` is the reliable answer and exists on Chrome and Safari;
 * Firefox has no equivalent, so `document.referrer` stands in. A framed page
 * whose referrer is suppressed reports `unknown` rather than guessing, because
 * "framed by someone we cannot name" and "not framed" are different facts and
 * collapsing them would overstate the top-level count.
 *
 * Only the origin is taken, never the path — a host's full URL is none of our
 * business and could itself carry a hand.
 */
export function embedOrigin(win = typeof window === 'undefined' ? null : window) {
  if (!win) return ''
  try {
    const ancestors = win.location.ancestorOrigins
    if (ancestors && ancestors.length) return new URL(ancestors[0]).origin
  } catch {
    // Fall through to the referrer check below.
  }
  try {
    if (win.self !== win.top) {
      return win.document.referrer ? new URL(win.document.referrer).origin : 'unknown'
    }
  } catch {
    // Cross-origin access to `win.top` throws, which itself means we are framed.
    return 'unknown'
  }
  return ''
}

/**
 * Round a duration to the nearest 100 ms.
 *
 * A precise millisecond timing is a surprisingly good fingerprint — it is a
 * high-entropy number tied to one device's exact performance. Bucketing keeps
 * every question we actually want to ask answerable (is this device slow? how
 * slow?) while throwing away the resolution that would make records
 * distinguishable.
 */
export function bucket(ms) {
  const n = Number(ms)
  if (!Number.isFinite(n) || n < 0) return 0
  return Math.round(n / 100) * 100
}

/**
 * The fields every event carries.
 *
 * `tz` is a UTC offset in minutes, not a timezone name: the offset says roughly
 * when in the day people use this, which is the question, while the IANA name is
 * far more identifying and answers nothing extra.
 */
function common() {
  return {
    v: APP_VERSION,
    tz: -new Date().getTimezoneOffset(),
    embed: embedOrigin(),
    simd: capabilities().simd ? 1 : 0,
  }
}

/**
 * Whether `?nolog` is on the URL.
 *
 * An opt-out for automated visits. Browser testing drives the real site, so its
 * page loads and solves were landing in the statistics alongside real ones and
 * skewing them — the timings especially, since an automated browser is slower
 * than a person's and repeats the same board. Excluding it after the fact turned
 * out to be impossible: the only distinguishing field was the browser version,
 * which real traffic reaches soon enough, and the runs were interleaved with
 * real ones so no cut-off date separated them either. Suppressing the record at
 * source is the only thing that works.
 *
 * Deliberately a URL parameter rather than a stored setting: this site writes
 * nothing to the device, and a flag that persisted would be exactly the kind of
 * client-side state ADR-001 rules out. It also has to be set per visit, which
 * makes it hard to leave switched on by accident.
 *
 * Any value will do, including none — `?nolog`, `?nolog=1`.
 */
export function suppressed(win = typeof window === 'undefined' ? null : window) {
  if (!win) return false
  try {
    return new URLSearchParams(win.location.search).has('nolog')
  } catch {
    return false
  }
}

/**
 * Whether to send at all.
 *
 * Off in development, because a dev server has no `/t` to receive it and a
 * console full of failed beacons trains you to ignore the console. Off when the
 * browser has no `sendBeacon`, which is the only transport that survives the tab
 * closing mid-solve. Off when `?nolog` asks for it.
 */
function enabled() {
  if (typeof navigator === 'undefined' || typeof navigator.sendBeacon !== 'function') return false
  if (suppressed()) return false
  return Boolean(import.meta.env?.PROD)
}

/**
 * Send one record. Never throws, never returns a promise worth awaiting.
 *
 * A beacon that fails must cost nothing: no retry, no error surfaced, no effect
 * on the analysis. On GitHub Pages there is no `/t` at all and every call here
 * is a silent 404, which is the correct behaviour rather than a bug to fix.
 */
function send(ev, fields) {
  if (!enabled()) return false
  try {
    return navigator.sendBeacon(ENDPOINT, JSON.stringify({ ev, ...common(), ...fields }))
  } catch {
    // Telemetry is never allowed to break the page.
    return false
  }
}

/** The page opened. Carries the cold-start segments, which is why they exist. */
export function reportLoad() {
  const t = timings()
  return send('load', {
    fetchMs: bucket(t.wasmFetch),
    compileMs: bucket(t.wasmCompile),
  })
}

/**
 * An analysis finished.
 *
 * `cards` is how many cards were analysed — a count, not the cards themselves.
 * `cold` distinguishes the first solve of a page load, which also pays for the
 * engine warming up, from the steady state.
 */
export function reportSolve({ ms, cards, cold, bench, cancelled } = {}) {
  return send('solve', {
    ms: bucket(ms),
    cards: Math.max(0, Math.min(52, Math.round(Number(cards) || 0))),
    cold: cold ? 1 : 0,
    bench: Math.max(0, Math.round(Number(bench) || 0)),
    cancelled: cancelled ? 1 : 0,
  })
}

/**
 * A claim benchmark run finished: every claim of a preset, none stopped early.
 *
 * The claims are generated from fixed seeds, the same for every visitor, so
 * nothing here describes a hand anyone chose. `nodes` and `undecided` are
 * deterministic for a given preset and build; they are sent so a run on a
 * build whose search changed is recognisable, and `mismatch` because a
 * browser that disagrees with the native verdicts has a real wasm bug.
 */
export function reportClaimBench({ preset, ms, claims, nodes, undecided, mismatch } = {}) {
  const whole = (n, max) => Math.max(0, Math.min(max, Math.round(Number(n) || 0)))
  return send('claimbench', {
    preset: preset === 'quick' || preset === 'full' ? preset : '',
    ms: bucket(ms),
    claims: whole(claims, 100_000),
    nodes: whole(nodes, 1e12),
    undecided: whole(undecided, 100_000),
    mismatch: mismatch ? 1 : 0,
  })
}

/** Exposed for tests, which need to assert the gate rather than the transport. */
export const __test = { enabled, common, suppressed }
