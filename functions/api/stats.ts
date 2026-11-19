// The public performance dashboard's data.
//
// Served at `/api/stats`, deliberately *not* `/stats` — that route belongs to
// the static page a reader actually visits, and a Function mounted there would
// shadow it.
//
// ## Why this queries Analytics Engine directly
//
// The plan called for a separate cron Worker to aggregate once a day and drop a
// blob in KV. That was dropped, and the reasoning is worth keeping. Its stated
// justification was that the AE SQL API needs a token which must never reach a
// browser — true, but this Function is already server-side, so the token can
// live here just as safely. The other two justifications (cheap, spike-immune)
// are cache arguments, and a cache does not need a clock: serving the KV blob
// and refreshing it past a TTL gets the same properties, runs *no* queries on a
// day nobody visits, and is self-healing where a dead cron silently serves
// stale numbers forever. It is also one deploy artifact instead of two, since
// Pages Functions cannot carry cron triggers and a cron would have meant a
// whole second Worker.
//
// The one thing a daily job would buy that this does not is history outliving
// Analytics Engine's three-month retention. Everything below sits inside that
// window, so every figure here is recomputable on demand and the KV blob is a
// pure cache with no independent value. If a record longer than three months is
// ever wanted, that is the change to make, and it has to be made *before* the
// data ages out rather than after.
//
// Publishing these numbers openly is also the strongest available backing for
// the privacy claim: anyone can see exactly what is collected, because all of
// it is on the screen.

interface Env {
  SOLVER_STATS: KVNamespace
  /** Account-scoped token with Account Analytics: Read. A secret, never a var. */
  AE_TOKEN?: string
  /** Not a credential — the account the dataset lives in. Set as a plain var. */
  CF_ACCOUNT_ID?: string
}

/** The key the aggregate lands under. */
const KEY = 'aggregate'

/**
 * How long a cached aggregate is served before a refresh is kicked off.
 *
 * Fifteen minutes rather than the plan's daily cadence because this is a
 * dashboard someone watches while changing things, and an hour-old blob after a
 * deploy reads as broken. The cost ceiling is bounded and small: at most one
 * refresh per window regardless of traffic, so ~96 refreshes a day in the worst
 * case, each a handful of SQL queries.
 */
const TTL_MS = 15 * 60 * 1000

/**
 * The reporting window.
 *
 * Analytics Engine keeps three months, so 90 days is everything there is.
 */
const WINDOW_DAYS = 90

/**
 * Buckets smaller than this are dropped from the country and embed-origin
 * tables.
 *
 * Only those two. They are the dimensions where a small count could plausibly
 * point at one person — a single visitor from a small country, or one niche
 * site embedding the page. The performance distributions are not suppressed:
 * a timing bucket with one event in it says nothing about who produced it, and
 * hiding it would misrepresent the spread, which is the entire point of the
 * page.
 */
const MIN_BUCKET = 5

/**
 * Drop the browser-automation traffic recorded before `?nolog` existed.
 *
 * Automated browser testing drives the real site, so its page loads and solves
 * were landing here alongside real ones — and skewing the timings especially,
 * since an automated browser is slower than a person's and re-solves the same
 * board. The client now suppresses its own record when the URL carries `?nolog`,
 * which is the durable fix; this clause only cleans up what was recorded before
 * that existed.
 *
 * It is deliberately narrow. Telemetry carries no identifier, so the only field
 * that distinguished those runs was the browser version — the automated browser
 * reported `Chrome 151` while the machine driving it was on `Chrome 150`. A
 * plain start date could not be used: the two were interleaved over the same two
 * days, so any date late enough to drop the automated runs also threw away every
 * real one beside them.
 *
 * Hence version *and* date together, and the date is what keeps this from
 * becoming a permanent lie: real traffic reaches Chrome 151 soon enough, and
 * from the cut-off onward those visits count normally. The cut-off is just after
 * the last automated run.
 *
 * Delete this once the window has aged out of Analytics Engine's three-month
 * retention, at which point it can no longer match anything.
 */
const EXCLUDE_AUTOMATED =
  "NOT (blob1 = 'Chrome 151' AND timestamp < toDateTime('2026-08-02 05:00:00'))"

/**
 * The version that produced a record, corrected for the builds that misreported
 * it.
 *
 * Until 2026-11-19 the client reported `web/package.json`'s version, which
 * nobody bumped after the 1.0.0 release, so every build from v1.0.0 to v2.2.0
 * recorded itself as `1.0.0` (and the v0.5.0 build as `0.4.0`). The client now
 * reports the crate's own version from `Cargo.toml`, so this only repairs the
 * history, and it does so from the deploy record rather than a guess: each
 * cut-off is when the Pages workflow's Cloudflare step finished deploying the
 * first build of that version (GitHub Actions runs on `main`):
 *
 *     2026-08-22 13:53:26  1f334bc  0.5.0  (still reporting 0.4.0)
 *     2026-09-23 01:38:22  3da1d75  2.0.0
 *     2026-10-27 13:51:40  9ee1cc0  2.1.0
 *     2026-11-19 07:31:59  eae3721  2.2.0  (cb4d40f at 12:12:08 reports it)
 *
 * The site was on v2.1.0 until 2026-11-19 07:31 even though v2.2.0 was
 * released earlier, because the deploys between 2026-11-17 and then failed.
 *
 * The benchmark records bear this out: their node counts fingerprint the
 * search, and the analysis benchmark at 06:45 that morning searched the
 * pre-2.2 tree (296,689,028 nodes) while the one after 07:31 searched 2.2's
 * (183,796,287).
 *
 * As with the client's own label, a version here means the crate version of
 * the deployed build, so `2.1.0` also covers unreleased work merged to `main`
 * after v2.1.0. A page loaded just before a deploy and used just after it is
 * labelled with the newer version; that window is minutes.
 *
 * Delete the corrections once 2026-11-19 has aged out of the window (Analytics
 * Engine keeps three months), leaving `blob6` alone.
 */
const VERSION = `if(blob6 = '1.0.0',
    if(timestamp < toDateTime('2026-09-23 01:38:22'), '1.0.0',
    if(timestamp < toDateTime('2026-10-27 13:51:40'), '2.0.0',
    if(timestamp < toDateTime('2026-11-19 07:31:59'), '2.1.0', '2.2.0'))),
    if(blob6 = '0.4.0' AND timestamp >= toDateTime('2026-08-22 13:53:26'), '0.5.0', blob6))`

/** Rows come back from the SQL API as NDJSON under `FORMAT JSONEachRow`. */
type Row = Record<string, string | number | null>

/**
 * Run one SQL statement against the Analytics Engine SQL API.
 *
 * Errors are surfaced as a rejection carrying the API's own message: a query
 * that no longer parses is a bug worth seeing, not something to paper over with
 * an empty panel.
 */
async function sql(env: Env, statement: string): Promise<Row[]> {
  // Trimmed because the secret is pasted at a prompt by a person, and a stray
  // newline or space rides along more easily than you would think. It arrives
  // as a `403 Authentication error` — the token rejected outright rather than
  // found wanting a permission — which is an expensive thing to debug for a
  // cause this cheap to rule out.
  const account = env.CF_ACCOUNT_ID?.trim()
  const response = await fetch(
    `https://api.cloudflare.com/client/v4/accounts/${account}/analytics_engine/sql`,
    {
      method: 'POST',
      headers: { Authorization: `Bearer ${env.AE_TOKEN?.trim()}` },
      body: statement,
    }
  )

  const text = await response.text()
  if (!response.ok) throw new Error(`AE ${response.status}: ${text.slice(0, 200)}`)

  // `FORMAT JSONEachRow` returns newline-delimited objects with no envelope,
  // so an error — which *is* enveloped — is also caught by the parse failing.
  const rows: Row[] = []
  for (const line of text.split('\n')) {
    const trimmed = line.trim()
    if (trimmed) rows.push(JSON.parse(trimmed))
  }
  return rows
}

/** Counts arrive as strings (`UInt64`), timings as numbers. Normalise both. */
function num(value: string | number | null | undefined): number {
  const parsed = Number(value)
  return Number.isFinite(parsed) ? parsed : 0
}

/**
 * The window clause every query shares.
 *
 * Both halves matter: the retention window bounds the query, and the exclusion
 * above removes the automation traffic that predates `?nolog`.
 */
const WINDOW = `timestamp > NOW() - INTERVAL '${WINDOW_DAYS}' DAY AND ${EXCLUDE_AUTOMATED}`

/** A weighted percentile. AE samples, and ignoring `_sample_interval` understates. */
function q(quantile: number, column: string): string {
  return `quantileWeighted(${quantile})(${column}, _sample_interval)`
}

/**
 * Every panel on the dashboard, as one query each.
 *
 * Run concurrently, and independently: a single query that stops parsing costs
 * its own panel rather than the whole page. Note the SQL dialect is a subset —
 * `uniq()` and `CASE WHEN` are both rejected, so this uses `count(DISTINCT x)`
 * and nested `if()` instead. Verified against the live API, not inferred.
 */
const QUERIES: Record<string, string> = {
  // Headline counts. `if()` rather than several round trips for one row.
  totals: `SELECT
      sum(if(index1 = 'load', _sample_interval, 0)) AS loads,
      sum(if(index1 = 'solve', _sample_interval, 0)) AS solves,
      sum(if(index1 = 'solve' AND double8 = 1, _sample_interval, 0)) AS cancelled,
      sum(if(index1 = 'claim', _sample_interval, 0)) AS claims,
      count(DISTINCT blob3) AS countries,
      min(timestamp) AS first_seen,
      max(timestamp) AS last_seen
    FROM solver_events WHERE ${WINDOW} FORMAT JSONEachRow`,

  // Volume over time, and the loads-to-solves relationship.
  daily: `SELECT toDate(timestamp) AS d,
      sum(if(index1 = 'load', _sample_interval, 0)) AS loads,
      sum(if(index1 = 'solve', _sample_interval, 0)) AS solves
    FROM solver_events WHERE ${WINDOW} GROUP BY d ORDER BY d FORMAT JSONEachRow`,

  // Variation over time — the shape of the spread, not just its middle.
  dailyPerf: `SELECT toDate(timestamp) AS d, sum(_sample_interval) AS n,
      ${q(0.5, 'double1')} AS p50, ${q(0.9, 'double1')} AS p90
    FROM solver_events WHERE ${WINDOW} AND index1 = 'solve' AND double1 > 0
    GROUP BY d ORDER BY d FORMAT JSONEachRow`,

  // The distribution itself. Client already buckets to 100 ms; 250 ms here
  // keeps the bar count readable without hiding the tail.
  solveHistogram: `SELECT intDiv(toUInt32(double1), 250) * 250 AS bucket,
      sum(_sample_interval) AS n
    FROM solver_events WHERE ${WINDOW} AND index1 = 'solve' AND double1 > 0
    GROUP BY bucket ORDER BY bucket FORMAT JSONEachRow`,

  solvePercentiles: `SELECT sum(_sample_interval) AS n,
      ${q(0.5, 'double1')} AS p50, ${q(0.9, 'double1')} AS p90,
      ${q(0.99, 'double1')} AS p99, min(double1) AS lo, max(double1) AS hi
    FROM solver_events WHERE ${WINDOW} AND index1 = 'solve' AND double1 > 0
    FORMAT JSONEachRow`,

  // Normalised for hand size, so a 52-card board and a 41-card one compare.
  msPerCard: `SELECT sum(_sample_interval) AS n,
      ${q(0.5, 'double1 / double4')} AS p50, ${q(0.9, 'double1 / double4')} AS p90
    FROM solver_events
    WHERE ${WINDOW} AND index1 = 'solve' AND double1 > 0 AND double4 > 0
    FORMAT JSONEachRow`,

  // Variation across platforms — which engines are actually slow.
  byPlatform: `SELECT blob1 AS browser, blob2 AS os, sum(_sample_interval) AS n,
      ${q(0.5, 'double1')} AS p50, ${q(0.9, 'double1')} AS p90
    FROM solver_events WHERE ${WINDOW} AND index1 = 'solve' AND double1 > 0
    GROUP BY browser, os ORDER BY n DESC FORMAT JSONEachRow`,

  // The panel that checks the pre-solve warning against reality.
  //
  // The warning's estimate is a pure function of the device-speed score
  // (`950 * 100 / score` ms, see web/src/lib/benchmark.js), and the score is
  // already collected as `double5`. So predicted-versus-actual needs no new
  // field: bucket by score and compare the band's predicted figure against the
  // measured p50. A systematically over-predicting warning shows up here as a
  // gap, which is exactly the failure reported on an iPad Air — 4 s promised
  // against ~0.5 s actual.
  byBench: `SELECT intDiv(toUInt32(double5), 25) * 25 AS band,
      sum(_sample_interval) AS n, ${q(0.5, 'double5')} AS score,
      ${q(0.5, 'double1')} AS p50, ${q(0.9, 'double1')} AS p90
    FROM solver_events
    WHERE ${WINDOW} AND index1 = 'solve' AND double1 > 0 AND double5 > 0
    GROUP BY band ORDER BY band FORMAT JSONEachRow`,

  // The device envelope: how fast the real-world fleet actually is.
  benchDist: `SELECT intDiv(toUInt32(double5), 25) * 25 AS band,
      sum(_sample_interval) AS n
    FROM solver_events WHERE ${WINDOW} AND double5 > 0
    GROUP BY band ORDER BY band FORMAT JSONEachRow`,

  // Delivery, which the baseline says is ~6% of the problem. Here to keep that
  // claim honest against real connections rather than a local build.
  coldStart: `SELECT sum(_sample_interval) AS n,
      ${q(0.5, 'double6')} AS fetch_p50, ${q(0.9, 'double6')} AS fetch_p90,
      ${q(0.5, 'double7')} AS compile_p50, ${q(0.9, 'double7')} AS compile_p90
    FROM solver_events WHERE ${WINDOW} AND double6 > 0 FORMAT JSONEachRow`,

  // Whether the prefix cache is doing what the baseline claims (~17,000x).
  coldVsWarm: `SELECT double2 AS cold, sum(_sample_interval) AS n,
      ${q(0.5, 'double1')} AS p50
    FROM solver_events WHERE ${WINDOW} AND index1 = 'solve' AND double1 > 0
    GROUP BY cold ORDER BY cold FORMAT JSONEachRow`,

  cards: `SELECT double4 AS cards, sum(_sample_interval) AS n
    FROM solver_events WHERE ${WINDOW} AND index1 = 'solve' AND double4 > 0
    GROUP BY cards ORDER BY cards FORMAT JSONEachRow`,

  // Time-of-day shape, without needing a clock reading from the client.
  tz: `SELECT double3 AS tz, sum(_sample_interval) AS n
    FROM solver_events WHERE ${WINDOW} GROUP BY tz ORDER BY tz FORMAT JSONEachRow`,

  versions: `SELECT ${VERSION} AS v, sum(_sample_interval) AS n,
      min(timestamp) AS first_seen, max(timestamp) AS last_seen
    FROM solver_events WHERE ${WINDOW} AND blob6 != ''
    GROUP BY v ORDER BY n DESC FORMAT JSONEachRow`,

  // Analyses in the wild, by version. Real hands on real devices, so the mix of
  // both moves between versions as well as the code; the benchmarks below, by
  // version too, are the like-for-like comparison. The probe score is here
  // because the probe is a fixed solve: on the same devices a faster engine
  // scores higher, so it moves with the code and with the fleet alike.
  byVersion: `SELECT ${VERSION} AS v, sum(_sample_interval) AS n,
      ${q(0.5, 'double1')} AS p50, ${q(0.9, 'double1')} AS p90,
      ${q(0.5, 'double1 / double4')} AS per_card, ${q(0.5, 'double5')} AS score
    FROM solver_events
    WHERE ${WINDOW} AND index1 = 'solve' AND double1 > 0 AND double4 > 0 AND double5 > 0
      AND blob6 != ''
    GROUP BY v FORMAT JSONEachRow`,

  // The claim benchmark page, one row per preset, version and platform, so a
  // version's gain shows on the same fixed claims and the same platform.
  // `double10` is the nodes searched, which fingerprints the search itself: it
  // changes exactly when the search does, whatever the device. `double1` is
  // the run's total checking time and `double9` the claims in it, so
  // `per_solve` is the median run's time per claim. `double12` flags a run
  // whose verdicts differed from the native tool's, which is a wasm bug, not a
  // slow device.
  claimBench: `SELECT blob8 AS preset, ${VERSION} AS v, blob1 AS browser, blob2 AS os,
      sum(_sample_interval) AS n, ${q(0.5, 'double1')} AS p50, min(double1) AS best,
      max(double9) AS solves, ${q(0.5, 'double1 / double9')} AS per_solve,
      sum(if(double12 = 1, _sample_interval, 0)) AS mismatched,
      max(double10) AS nodes
    FROM solver_events WHERE ${WINDOW} AND index1 = 'claimbench' AND double1 > 0 AND double9 > 0
    GROUP BY preset, v, browser, os ORDER BY preset, v DESC, n DESC FORMAT JSONEachRow`,

  // Claims checked on the claim page, by what was asked: `check` (one claim)
  // or `max` (the most one player could claim). Times are finer-grained than a
  // solve's, since most claims take milliseconds; 0 is a real time here.
  claims: `SELECT blob8 AS kind, sum(_sample_interval) AS n,
      ${q(0.5, 'double1')} AS p50, ${q(0.9, 'double1')} AS p90, max(double1) AS hi
    FROM solver_events WHERE ${WINDOW} AND index1 = 'claim'
    GROUP BY kind ORDER BY n DESC FORMAT JSONEachRow`,

  // The analysis benchmark, laid out like claimBench, with deals in `double9`.
  ddBench: `SELECT blob8 AS preset, ${VERSION} AS v, blob1 AS browser, blob2 AS os,
      sum(_sample_interval) AS n, ${q(0.5, 'double1')} AS p50, min(double1) AS best,
      max(double9) AS solves, ${q(0.5, 'double1 / double9')} AS per_solve,
      sum(if(double12 = 1, _sample_interval, 0)) AS mismatched,
      max(double10) AS nodes
    FROM solver_events WHERE ${WINDOW} AND index1 = 'ddbench' AND double1 > 0 AND double9 > 0
    GROUP BY preset, v, browser, os ORDER BY preset, v DESC, n DESC FORMAT JSONEachRow`,

  // Suppressed. See MIN_BUCKET — these are the two identity-adjacent columns.
  countries: `SELECT blob3 AS country, sum(_sample_interval) AS n
    FROM solver_events WHERE ${WINDOW} AND blob3 != '' AND blob3 != 'XX'
    GROUP BY country HAVING n >= ${MIN_BUCKET} ORDER BY n DESC FORMAT JSONEachRow`,

  embeds: `SELECT blob4 AS origin, sum(_sample_interval) AS n
    FROM solver_events WHERE ${WINDOW} AND blob4 != ''
    GROUP BY origin HAVING n >= ${MIN_BUCKET} ORDER BY n DESC LIMIT 50
    FORMAT JSONEachRow`,
}

/** The shape the page consumes. Panels are `null` when their query failed. */
interface Aggregate {
  generatedAt: string
  windowDays: number
  minBucket: number
  excluded: string
  panels: Record<string, Row[] | null>
  failed: string[]
  /**
   * Why each failed panel failed.
   *
   * Naming the panel without saying what went wrong turned out to be useless
   * the first time this broke in production: sixteen panels failed at once and
   * the response could not distinguish a rejected credential from a query that
   * had stopped parsing. These are the SQL API's own messages, truncated. They
   * carry the status and its complaint, never the token.
   */
  errors: Record<string, string>
}

/**
 * Query everything and assemble the blob.
 *
 * `allSettled` rather than `all`: one panel's query breaking should cost that
 * panel, not the dashboard. What broke is reported in `failed` so a silently
 * missing chart is distinguishable from a genuinely empty one.
 */
async function aggregate(env: Env): Promise<Aggregate> {
  const names = Object.keys(QUERIES)
  const settled = await Promise.allSettled(names.map((name) => sql(env, QUERIES[name])))

  const panels: Record<string, Row[] | null> = {}
  const failed: string[] = []
  const errors: Record<string, string> = {}

  settled.forEach((outcome, i) => {
    if (outcome.status === 'fulfilled') {
      panels[names[i]] = outcome.value
    } else {
      panels[names[i]] = null
      failed.push(names[i])
      errors[names[i]] = String(outcome.reason?.message ?? outcome.reason).slice(0, 200)
    }
  })

  // Normalise the numerics once, here, so the page never has to know that AE
  // returns UInt64 as a string. Date-like columns are left alone.
  const textual = new Set(['d', 'first_seen', 'last_seen'])
  for (const rows of Object.values(panels)) {
    if (!rows) continue
    for (const row of rows) {
      for (const [key, value] of Object.entries(row)) {
        if (textual.has(key)) continue
        if (typeof value === 'string' && value !== '' && Number.isFinite(Number(value))) {
          row[key] = num(value)
        }
      }
    }
  }

  return {
    generatedAt: new Date().toISOString(),
    windowDays: WINDOW_DAYS,
    minBucket: MIN_BUCKET,
    excluded: EXCLUDE_AUTOMATED,
    panels,
    failed,
    errors,
  }
}

/** Recompute and store. Returns the blob so a cold request can serve it. */
async function refresh(env: Env): Promise<string> {
  const blob = JSON.stringify(await aggregate(env))
  await env.SOLVER_STATS.put(KEY, blob)
  return blob
}

/**
 * How long a *degraded* aggregate is held before retrying.
 *
 * A blob whose panels all failed is not data, it is an outage — a rejected
 * credential, most likely. Holding it for the full TTL means that after the
 * cause is fixed the page keeps reporting the failure for another quarter of an
 * hour, which is exactly when someone is watching and wondering whether their
 * fix worked. So it is retried a minute later instead.
 */
const DEGRADED_TTL_MS = 60 * 1000

/** Whether a stored blob is old enough to warrant recomputing. */
function isStale(blob: string): boolean {
  try {
    const parsed = JSON.parse(blob) as Aggregate
    const age = Date.now() - Date.parse(parsed.generatedAt)
    const degraded = (parsed.failed?.length ?? 0) > 0
    return age > (degraded ? DEGRADED_TTL_MS : TTL_MS)
  } catch {
    // Unparseable is worse than stale — replace it.
    return true
  }
}

function json(body: string, cacheSeconds: number): Response {
  return new Response(body, {
    headers: {
      'Content-Type': 'application/json; charset=utf-8',
      'Cache-Control': cacheSeconds > 0 ? `public, max-age=${cacheSeconds}` : 'no-store',
    },
  })
}

export const onRequestGet: PagesFunction<Env> = async ({ env, waitUntil }) => {
  const cached = await env.SOLVER_STATS.get(KEY, 'text')

  // Without the credential there is nothing to compute, so serve whatever is
  // stored and otherwise say plainly that there is nothing yet. This is also
  // the path taken by any deploy that has not been given the secret.
  if (!env.AE_TOKEN || !env.CF_ACCOUNT_ID) {
    if (cached !== null) return json(cached, 60)
    return Response.json(
      { status: 'pending', detail: 'No aggregate has been written yet.' },
      { status: 503, headers: { 'Cache-Control': 'no-store' } }
    )
  }

  // Cold: nothing stored, so this request pays for the first aggregation.
  if (cached === null) {
    try {
      return json(await refresh(env), TTL_MS / 1000)
    } catch (error) {
      return Response.json(
        { status: 'error', detail: String(error).slice(0, 300) },
        { status: 503, headers: { 'Cache-Control': 'no-store' } }
      )
    }
  }

  // Warm but past its TTL: serve immediately and rebuild behind the response,
  // so nobody waits on the SQL API. Two overlapping refreshes would both simply
  // write a correct blob, so the race is not worth a lock at this volume.
  if (isStale(cached)) {
    waitUntil(refresh(env).catch(() => {}))
  }

  return json(cached, TTL_MS / 1000)
}
