// The analysis benchmark's engine thread.
//
// Each deal's full double-dummy table is solved here, off the main thread, and
// each result is posted back as it lands so the page can show progress. The
// solve alone is timed. Stopping a run is the page terminating this worker.

import init, { dd_bench_presets, dd_bench_table } from '../wasm/bridge_solver_wasm.js'

const ready = init()

self.onmessage = async ({ data }) => {
  try {
    await ready
    if (data.op === 'catalogue') {
      self.postMessage({ op: 'catalogue', catalogue: JSON.parse(dd_bench_presets()) })
      return
    }
    if (data.op === 'run') {
      for (let index = 0; index < data.deals; index++) {
        const started = performance.now()
        const answer = dd_bench_table(index)
        const ms = performance.now() - started
        const [tricks, nodes] = answer.split(' ')
        self.postMessage({ op: 'deal', index, tricks, nodes: Number(nodes), ms })
      }
      self.postMessage({ op: 'done' })
    }
  } catch (error) {
    self.postMessage({ op: 'error', message: String(error?.message || error) })
  }
}
