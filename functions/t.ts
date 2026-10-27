// Telemetry ingest.
//
// One row per page load and per completed solve, carrying **no identifier of
// any kind** — no account, no cookie, no random ID, no hash of the IP. That is
// ADR-001, and it is the constraint the rest of this file exists to respect:
// two records from the same person cannot be linked, by us or by anyone else.
// The consequence is that we cannot count unique users, only events. That is a
// deliberate trade.
//
// Nothing derived from the deal is accepted or stored. Not the PBN or LIN, not
// the contract, not the trick counts, not a hash of any of them. The client
// never sends it; this endpoint would ignore it if it did. That exclusion is
// the load-bearing privacy claim of the whole site, so the field allowlist
// below is deliberately explicit rather than a spread of whatever arrived.

interface Env {
  SOLVER: AnalyticsEngineDataset
}

/** Largest body we will read. A beacon is a few hundred bytes; this is slack. */
const MAX_BODY_BYTES = 1024

/** Event kinds we accept. Anything else is dropped rather than recorded. */
const EVENTS = new Set(['load', 'solve', 'bench', 'claimbench', 'claim', 'ddbench'])

/**
 * Claim-benchmark presets we accept, mirroring `PRESETS` in
 * src/claim_bench.rs. Anything else is recorded as ''.
 *
 * A `claimbench` record is one completed run of the claim benchmark page. Its
 * claims are generated from fixed seeds, identical for every visitor, so like
 * every other record it carries nothing a person typed or chose to analyse.
 */
const CLAIM_PRESETS = new Set(['quick', 'full'])

/**
 * What a `claim` record measured: `check` for one claim tested on the claim
 * page, `max` for the most one player could claim. A `claim` record carries
 * the time and the number of cards played, like a `solve` record, and nothing
 * about the hand or the claim: not the verdict, the seat, nor the tricks.
 *
 * A `ddbench` record is one completed run of the analysis benchmark, the
 * double-dummy counterpart of `claimbench`: fixed deals, identical for every
 * visitor, with its preset in blob8 and its deal count in the `claims`
 * column.
 */
const CLAIM_KINDS = new Set(['check', 'max'])

/** blob8: the benchmark preset, or what a claim record measured. */
function blob8(ev: string, body: Record<string, unknown>): string {
  if (ev === 'claim') return CLAIM_KINDS.has(String(body.kind)) ? String(body.kind) : ''
  return CLAIM_PRESETS.has(String(body.preset)) ? String(body.preset) : ''
}

/**
 * Clamp a number into a sane range, or return 0.
 *
 * Everything numeric here arrives from a browser we do not control, so each
 * field is bounded before it is written rather than trusted. `NaN`, `Infinity`,
 * a string, or a missing value all become 0.
 */
function num(value: unknown, min: number, max: number): number {
  const n = typeof value === 'number' ? value : Number(value)
  if (!Number.isFinite(n)) return 0
  return Math.min(max, Math.max(min, n))
}

/**
 * A coarse browser/OS family, e.g. `Safari 18` / `iOS`.
 *
 * Deliberately a small lookup rather than a UA-parsing dependency: the point is
 * to know roughly which engines are slow, and a full UA string is a
 * fingerprinting surface we have no use for. Anything unrecognised is `other`,
 * which is a perfectly good answer.
 */
function family(ua: string): { browser: string; os: string } {
  const major = (re: RegExp) => ua.match(re)?.[1] ?? ''

  let browser = 'other'
  if (/Edg\//.test(ua)) browser = `Edge ${major(/Edg\/(\d+)/)}`
  else if (/EdgiOS\//.test(ua)) browser = `Edge ${major(/EdgiOS\/(\d+)/)}`
  else if (/OPR\//.test(ua)) browser = `Opera ${major(/OPR\/(\d+)/)}`
  // On iOS every browser is Safari's engine wearing a different badge, and the
  // badge is the only thing in the UA that differs: Chrome sends `CriOS`, Firefox
  // `FxiOS`, Edge `EdgiOS`, and none of them send `Chrome/` or `Firefox/` at all.
  // Checked before the desktop patterns, which they would otherwise fall past
  // into `other` — which is exactly what happened, and it hid every iPad and
  // iPhone behind an unlabelled bucket. Those are the slow devices this
  // telemetry exists to measure, so losing them defeated the point.
  else if (/CriOS\//.test(ua)) browser = `Chrome ${major(/CriOS\/(\d+)/)}`
  else if (/FxiOS\//.test(ua)) browser = `Firefox ${major(/FxiOS\/(\d+)/)}`
  else if (/Firefox\//.test(ua)) browser = `Firefox ${major(/Firefox\/(\d+)/)}`
  else if (/Chrome\//.test(ua)) browser = `Chrome ${major(/Chrome\/(\d+)/)}`
  else if (/Safari\//.test(ua) && /Version\//.test(ua)) {
    browser = `Safari ${major(/Version\/(\d+)/)}`
  }

  let os = 'other'
  if (/iPhone|iPad|iPod/.test(ua)) os = 'iOS'
  else if (/Android/.test(ua)) os = 'Android'
  else if (/Mac OS X/.test(ua)) os = 'macOS'
  else if (/Windows/.test(ua)) os = 'Windows'
  else if (/Linux/.test(ua)) os = 'Linux'

  return { browser: browser.trim(), os }
}

export const onRequestPost: PagesFunction<Env> = async ({ request, env }) => {
  // Cap the read rather than trusting Content-Length, which is client-supplied.
  const raw = (await request.text()).slice(0, MAX_BODY_BYTES)

  let body: Record<string, unknown>
  try {
    body = JSON.parse(raw)
  } catch {
    // A malformed beacon is not worth an error page — the client fires this
    // with `sendBeacon` and cannot act on a response anyway.
    return new Response(null, { status: 204 })
  }

  const ev = String(body.ev ?? '')
  if (!EVENTS.has(ev)) return new Response(null, { status: 204 })

  const ua = request.headers.get('user-agent') ?? ''
  const { browser, os } = family(ua)

  // `request.cf` is added at the edge. We read the country from it and let the
  // IP itself fall on the floor: it is never written to the dataset, never
  // logged, and never hashed into anything durable.
  const country = (request.cf?.country as string) ?? 'XX'

  // `Sec-Fetch-Dest` tells us `iframe` vs `document` server-side, which works
  // even where the client's `ancestorOrigins` check does not.
  const dest = request.headers.get('sec-fetch-dest') ?? ''

  env.SOLVER.writeDataPoint({
    // Low cardinality, which is what Analytics Engine wants for its sampling
    // equalization — so the event kind rather than anything finer.
    indexes: [ev],
    blobs: [
      browser,
      os,
      country,
      String(body.embed ?? '').slice(0, 128), // '' when top-level
      dest,
      String(body.v ?? '').slice(0, 32), // app version
      body.simd ? '1' : '0',
      // blob8: benchmark preset or claim kind; '' for every other event.
      blob8(ev, body),
    ],
    doubles: [
      num(body.ms, 0, 600_000), // elapsed, already bucketed client-side
      body.cold ? 1 : 0,
      num(body.tz, -840, 840), // UTC offset in minutes
      num(body.cards, 0, 52),
      num(body.bench, 0, 100_000),
      num(body.fetchMs, 0, 600_000),
      num(body.compileMs, 0, 600_000),
      body.cancelled ? 1 : 0,
      // double9..double12: the benchmarks only, 0 otherwise. Everything a
      // benchmark run sends goes here or in `ms`, never in the solve columns
      // above, several of which other panels read without filtering on the
      // event kind.
      num(body.claims, 0, 100_000), // claims checked, or deals solved
      num(body.nodes, 0, 1e12), // search nodes, summed
      num(body.undecided, 0, 100_000), // claims the budget ran out on
      body.mismatch ? 1 : 0, // verdicts differed from the native ones
    ],
  })

  // No body, and nothing for the client to key off. A beacon that fails should
  // never affect the solve, so there is deliberately nothing here to check.
  return new Response(null, { status: 204 })
}

// Only `onRequestPost` is exported, deliberately. Pages answers 405 by itself
// for a method with no matching handler, so an `onRequest` catch-all that
// checked the method and called `next()` was both redundant and wrong: it
// served HEAD correctly but hung GET into a 522, because `next()` from a
// catch-all falls through to static-asset resolution for a path that has none.
