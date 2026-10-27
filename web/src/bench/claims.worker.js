// The claim benchmark's engine thread.
//
// Claims are generated and checked here, off the main thread, and each result
// is posted back as it lands so the page can show progress. Only the check is
// timed: generating a claim runs one or more double-dummy solves to decide what
// to claim, and that is not the claim checker's work.
//
// Stopping a run is the page terminating this worker; there is nothing to
// cancel cooperatively, and a fresh worker is started for the next run.

import init, { ClaimBenchCase, claim_bench_presets } from '../wasm/bridge_solver_wasm.js'

const ready = init()

self.onmessage = async ({ data }) => {
  try {
    await ready
    if (data.op === 'catalogue') {
      self.postMessage({ op: 'catalogue', catalogue: JSON.parse(claim_bench_presets()) })
      return
    }
    if (data.op === 'run') {
      const { rows, count, budget } = data
      for (let row = 0; row < rows.length; row++) {
        const { workload, size } = rows[row]
        for (let index = 0; index < count; index++) {
          const claim = new ClaimBenchCase(workload, size, index)
          // The timer's resolution varies by browser (a millisecond, coarsened
          // further in some), so a single fast claim's time is approximate;
          // the per-row and whole-run sums are what to read.
          const started = performance.now()
          const answer = claim.check(budget)
          const ms = performance.now() - started
          claim.free()
          const [letter, nodes] = answer.split(' ')
          self.postMessage({ op: 'claim', row, index, letter, nodes: Number(nodes), ms })
        }
      }
      self.postMessage({ op: 'done' })
    }
  } catch (error) {
    self.postMessage({ op: 'error', message: String(error?.message || error) })
  }
}
