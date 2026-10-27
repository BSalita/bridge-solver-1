// The solver, off the main thread.
//
// A full per-card analysis of one deal is ~130 ms and a 20-cell table ~250 ms,
// so a single board would be tolerable inline — but a twelve-board file is
// several seconds of solid compute, which would freeze the page. Everything
// goes through here instead.
//
// One `Analyzer` lives for the life of the worker, which is the whole point:
// it holds the prefix-keyed position cache, so stepping through a hand pays for
// each new position once and hits the cache for everything before it.

import init, {
  initSync,
  Analyzer,
  check_claim,
  parse_lin,
  parse_lin_file,
} from '../wasm/bridge_solver_wasm.js'

let analyzer = null
let ready = null

/** How long binding the compiled module to its imports took, in milliseconds. */
let instantiateMs = null

/**
 * Bring the engine up, preferring a module the main thread already compiled.
 *
 * With a module in hand this is `new WebAssembly.Instance` and nothing else —
 * no fetch, no compile — which is the whole reason the main thread compiles.
 * Without one (an older browser, a blocked fetch) it falls back to the ordinary
 * load, so a failed preload costs a little time rather than the page.
 */
function ensureReady(module) {
  if (!ready) {
    const started = performance.now()
    ready = (module ? Promise.resolve().then(() => initSync({ module })) : init()).then(() => {
      instantiateMs = performance.now() - started
      analyzer = new Analyzer()
    })
  }
  return ready
}

/** Operations the main thread can ask for, by name. */
const handlers = {
  /**
   * Take delivery of the compiled module and report what instantiation cost.
   *
   * Sent before any real work, so every later operation finds the engine up.
   */
  init: () => ({ instantiateMs }),

  ddTable: ({ dealstr }) => JSON.parse(analyzer.dd_table(dealstr)),

  ddPlay: ({ request }) => JSON.parse(analyzer.dd_play(JSON.stringify(request))),

  ddPlayNode: ({ request, node }) =>
    JSON.parse(analyzer.dd_play_node(JSON.stringify(request), node)),

  ddOptimalLine: ({ request, from }) =>
    JSON.parse(analyzer.dd_optimal_line(JSON.stringify(request), from)),

  checkClaim: ({ request }) => JSON.parse(check_claim(JSON.stringify(request))),

  parseLin: ({ input }) => JSON.parse(parse_lin(input)),

  parseLinFile: ({ content }) => JSON.parse(parse_lin_file(content)),

  cacheSize: () => analyzer.cached_positions,

  clearCache: () => {
    analyzer.clear_cache()
    return true
  },
}

self.onmessage = async (event) => {
  const { id, op, payload } = event.data || {}
  try {
    await ensureReady(payload?.module)
    const handler = handlers[op]
    if (!handler) throw new Error(`unknown operation "${op}"`)
    self.postMessage({ id, ok: true, result: handler(payload || {}) })
  } catch (error) {
    // The engine reports failures as thrown `JsError`s. Send the message across
    // rather than the error object, which does not structured-clone usefully.
    self.postMessage({ id, ok: false, error: String(error?.message || error) })
  }
}
