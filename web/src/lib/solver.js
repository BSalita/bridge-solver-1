// Client for the double-dummy engine.
//
// This is the wasm replacement for Bridge-Classroom's `utils/ddsClient.js`,
// keeping its three operations and their exact request and response shapes so
// the same components and decoding code work against either. What changes is
// that there is no `fetch` and no `SOLVER_URL`: the engine is compiled into the
// page, which is what lets the site claim nothing is uploaded and back it with
// `connect-src 'self'`.
//
// The classroom client's other discipline is kept too: every call resolves to
// `null` on any failure rather than throwing. Double-dummy here is an overlay
// on a hand you can already read, so a failed solve should cost you the
// annotation and nothing else.

import { dealStringFrom } from './deal.js'
import { probeDeal, scoreFrom } from './benchmark.js'
import { record, recordSolve, timeSegment } from './perf.js'

/** Column order of a decoded DD table row. */
export const DD_STRAINS = ['C', 'D', 'H', 'S', 'NT']

/** Row order of a decoded DD table. */
export const DD_SEATS = ['N', 'E', 'S', 'W']

let worker = null
let nextId = 1
const pending = new Map()

/**
 * Where the engine binary lives.
 *
 * `new URL(..., import.meta.url)` is the form Vite rewrites to the hashed asset
 * URL, which is also what makes the immutable far-future cache header safe to
 * set on it.
 */
function wasmUrl() {
  return new URL('../wasm/bridge_solver_wasm_bg.wasm', import.meta.url)
}

let modulePromise = null

/**
 * Fetch and compile the engine once, on the main thread.
 *
 * Deliberately not `compileStreaming`, which would fuse the two segments this
 * exists to separate: they have different fixes — fetch wants compression and
 * caching, compile wants a smaller binary — and a single number cannot say
 * which one to chase. Compiling from a buffer costs a little against streaming
 * and buys both the breakdown and a `WebAssembly.Module` that can be handed to
 * more than one worker without any of them re-fetching or re-compiling.
 *
 * Resolves to `null` if the module cannot be produced, which leaves the worker
 * to load it the ordinary way rather than failing the page.
 */
export function preloadEngine() {
  if (modulePromise) return modulePromise
  modulePromise = (async () => {
    const bytes = await timeSegment('wasmFetch', async () => {
      const response = await fetch(wasmUrl())
      if (!response.ok) throw new Error(`fetching the engine failed: ${response.status}`)
      return response.arrayBuffer()
    })
    return timeSegment('wasmCompile', () => WebAssembly.compile(bytes))
  })().catch((error) => {
    console.warn(`[solver] preload fell back to in-worker load: ${error?.message || error}`)
    return null
  })
  return modulePromise
}

function rawCall(target, op, payload) {
  const id = nextId++
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject })
    target.postMessage({ id, op, payload })
  })
}

function spawnWorker() {
  const w = new Worker(new URL('./solver.worker.js', import.meta.url), { type: 'module' })
  w.onmessage = (event) => {
    const { id, ok, result, error } = event.data || {}
    const entry = pending.get(id)
    if (!entry) return
    pending.delete(id)
    ok ? entry.resolve(result) : entry.reject(new Error(error))
  }
  w.onerror = (event) => {
    // A worker-level failure (the module failing to load, say) never resolves
    // the in-flight calls otherwise.
    const message = event.message || 'the solver worker failed to start'
    for (const [, entry] of pending) entry.reject(new Error(message))
    pending.clear()
  }
  return w
}

let workerReady = null

/**
 * The worker, instantiated and ready for work.
 *
 * The compiled module is handed over before any operation is sent, so the
 * worker never races the main thread to fetch the same bytes. Instantiation is
 * timed inside the worker, because that is where it happens and because it is
 * the one cold-start segment a worker pool would pay more than once.
 */
function ensureWorker() {
  if (workerReady) return workerReady
  worker = spawnWorker()
  workerReady = preloadEngine()
    .then((module) => rawCall(worker, 'init', { module }))
    .then((result) => {
      if (result && typeof result.instantiateMs === 'number') {
        record('wasmInstantiate', result.instantiateMs)
      }
      return worker
    })
  return workerReady
}

function call(op, payload) {
  return ensureWorker().then((target) => rawCall(target, op, payload))
}

/**
 * How long the last measured run of work took, in milliseconds.
 *
 * Wall clock from the main thread, so it includes the worker hop and the JSON at
 * both ends — which is the honest number, because that is what the page waited
 * for. Measured here rather than in Rust because `std::time::Instant` has no
 * wasm32 implementation and traps at runtime.
 */
let lastElapsedMs = 0

export function elapsedMs() {
  return lastElapsedMs
}

/** Run `work`, recording how long it took whether or not it succeeds. */
export async function timed(work) {
  const started = performance.now()
  try {
    return await work()
  } finally {
    lastElapsedMs = performance.now() - started
    // Recorded in call order, so the first solve — which also pays for the
    // engine tiering up — can be told apart from the steady state.
    recordSolve(lastElapsedMs)
  }
}

/**
 * Swallow a failed analysis into `null`, but say why on the console.
 *
 * The `null` is deliberate — an overlay that cannot be computed should cost the
 * annotation and nothing else. Discarding the reason as well is not: it turns
 * "the trace silently did not appear" into something with no thread to pull.
 */
function optional(op, promise) {
  return promise.catch((error) => {
    console.warn(`[solver] ${op} failed: ${error?.message || error}`)
    return null
  })
}

/**
 * The 20-cell double-dummy table.
 *
 * Resolves to `{ tricks, total }` with `tricks` as rows in [`DD_SEATS`] order
 * and columns in [`DD_STRAINS`] order, or `null` if the deal could not be
 * solved.
 */
export function fetchDoubleDummy(hands) {
  return optional('dd table', call('ddTable', { dealstr: dealStringFrom(hands) }))
}

/**
 * Build the request body the two play operations share.
 *
 * `trump` takes the engine's spelling: a suit letter, or `NT`. Callers holding
 * a contract should pass `trumpFromContract(contract) || 'NT'`, since that
 * returns `null` for notrump.
 *
 * Everything here must be plain, structured-cloneable data: this crosses into a
 * worker, and `postMessage` cannot clone a Proxy. A caller passing Vue reactive
 * state — which is exactly what the app does — hands over proxied arrays, so
 * `plays` is copied into a plain one rather than forwarded. The failure mode if
 * this is missed is a `could not be cloned` DataCloneError, not a wrong answer.
 */
export function playRequest({ hands, trump, declarer, leader, plays }) {
  return {
    dealstr: dealStringFrom(hands),
    trump: trump || 'NT',
    declarer: String(declarer),
    leader: String(leader),
    plays: Array.from(plays, String),
  }
}

/**
 * The running trace: every card played, with what it cost.
 *
 * Resolves to `{ contract_tricks, trace: [{ index, seat, card, cost }], cached }`
 * or `null`. A `cost` above zero is a card that gave away that many tricks, and
 * it is comparable across seats — declarer's errors and the defence's are
 * counted the same way.
 */
export function fetchDdPlay(request) {
  return optional(
    'dd play',
    call('ddPlay', { request }).then((result) => {
      if (!Array.isArray(result?.trace)) throw new Error('response carried no trace')
      return result
    })
  )
}

/**
 * One decision point's alternatives.
 *
 * `node` indexes into `request.plays`. Resolves to
 * `{ index, seat, card, cost, alternatives: [{ card, tricks, cost }] }` or
 * `null`.
 */
export function fetchDdPlayNode(request, node) {
  return optional(
    'dd play node',
    call('ddPlayNode', { request, node }).then((result) => {
      if (!Array.isArray(result?.alternatives)) {
        throw new Error('response carried no alternatives')
      }
      return result
    })
  )
}

/**
 * A double-dummy-perfect continuation from one point in the hand.
 *
 * Resolves to `{ from, cards, seats, declaring_tricks }` or `null`. Started at the
 * first costed error, this is the correction for it: what should have happened.
 */
export function fetchOptimalLine(request, from) {
  return optional(
    'optimal line',
    call('ddOptimalLine', { request, from }).then((result) => {
      if (!Array.isArray(result?.cards)) throw new Error('response carried no line')
      return result
    })
  )
}

/**
 * Check a claim, or find the largest one, at the end of `request.plays`.
 *
 * `request` is a {@link playRequest} plus `claimer` (a seat) and, to check a
 * particular claim, `total_tricks` (the claiming side's total for the hand).
 * Without `total_tricks` it asks for the most the claimer is sure of instead.
 * Resolves to the engine's answer, `{ verdict | guaranteed, undecided,
 * tricks_won, tricks_left, nodes }`, and rejects with its message: like
 * parsing, this is what the page is for, so a failure has to be shown.
 */
export function checkClaim(request) {
  return call('checkClaim', { request })
}

/**
 * Parse a LIN string or a BBO handviewer URL.
 *
 * Resolves to the parsed board, or rejects with the engine's message — unlike
 * the analysis calls, this one *is* load-bearing: if the input cannot be read
 * there is nothing to show, and the user needs to be told why.
 */
export function parseLin(input) {
  return call('parseLin', { input })
}

/**
 * Parse a multi-board LIN file.
 *
 * Resolves to an array of `{ ok }` / `{ error }` entries, one per line, so a
 * single unreadable board does not cost the rest of the file.
 */
export function parseLinFile(content) {
  return call('parseLinFile', { content })
}

/**
 * Time one small fixed solve, to find out how fast this device is.
 *
 * Run once, before the first real analysis, because a warning about a slow
 * device is only useful before the wait rather than during it. Resolves to
 * `{ ms, score, ok }` — `ok` false meaning the device produced the wrong table,
 * which is a wasm correctness bug on that platform and worth more than the
 * timing beside it.
 *
 * Resolves to `null` rather than throwing: a probe that fails should cost the
 * warning, not the analysis.
 */
export function runBenchmark() {
  const { deal, ddtricks } = probeDeal()
  const started = performance.now()
  return optional(
    'benchmark',
    call('ddTable', { dealstr: deal }).then((result) => {
      const ms = performance.now() - started
      if (!result?.tricks) throw new Error('the probe returned no table')
      return { ms, score: scoreFrom(ms), ok: encodeDdTricks(result.tricks) === ddtricks }
    })
  )
}

/** How many positions the session cache is holding. */
export function cacheSize() {
  return optional('cache size', call('cacheSize', {})).then((n) => n ?? 0)
}

/**
 * Encode a decoded table back into the 20-character `ddtricks` string.
 *
 * Seat-major over `N, S, E, W` and strain over `NT, S, H, D, C` — note both
 * orders differ from the decoded table's. This is the interchange format the
 * classroom app and the older BSOL service both use, and keeping an encoder
 * here means a table from this engine can be compared against either.
 */
export function encodeDdTricks(tricks) {
  const seatRow = { N: 0, E: 1, S: 2, W: 3 }
  const strainCol = { NT: 4, S: 3, H: 2, D: 1, C: 0 }
  let out = ''
  for (const seat of ['N', 'S', 'E', 'W']) {
    for (const strain of ['NT', 'S', 'H', 'D', 'C']) {
      const n = tricks[seatRow[seat]][strainCol[strain]]
      out += n < 10 ? String(n) : String.fromCharCode(97 + n - 10)
    }
  }
  return out
}
