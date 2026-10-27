// The benchmark page's analysis suite bookkeeping: whether the answers match
// the native tool, the table's rows, and the text a person copies out.
//
// Kept apart from the page so it can be tested without a browser or the
// engine. The deals and presets come from the engine (`dd_bench_table`,
// `dd_bench_presets`), so this page and `bridge-solver --bench` cannot drift
// apart on what "quick" means.

import fixture from './fixtures/dd-bench-v1.json'

/** Deals per row of the results table. */
export const DEALS_PER_ROW = 10

/** The rows a run of `deals` deals fills: `{ first, last }`, 0-based. */
export function rowsFor(deals) {
  const rows = []
  for (let first = 0; first < deals; first += DEALS_PER_ROW) {
    rows.push({ first, last: Math.min(deals, first + DEALS_PER_ROW) - 1 })
  }
  return rows
}

/**
 * What the native tool found for deal `index`: its table as hex digits and
 * its nodes, or `null` when the fixture has no such deal.
 */
export function expected(index) {
  if (index < 0 || index >= fixture.nodes.length) return null
  return { tricks: fixture.tricks[index], nodes: fixture.nodes[index] }
}

/**
 * Totals over some deals' results, each `{ tricks, nodes, ms, expected }`.
 *
 * `matches` is `true` when every deal with a native result agreed with it,
 * `false` when any did not, and `null` when there was nothing to compare.
 */
export function tally(results) {
  const compared = results.filter((r) => r.expected)
  return {
    deals: results.length,
    nodes: results.reduce((sum, r) => sum + r.nodes, 0),
    ms: results.reduce((sum, r) => sum + r.ms, 0),
    matches: compared.length
      ? compared.every((r) => r.tricks === r.expected.tricks && r.nodes === r.expected.nodes)
      : null,
  }
}

const pad = (value, width) => String(value).padStart(width)

/**
 * The results as plain text, laid out like `bridge-solver --bench` so the two
 * can be pasted side by side.
 */
export function resultsText({ preset, version, platform, results }) {
  const lines = [
    `bridge-solver ${version} in ${platform}, 1 thread (browser), preset ${preset} ` +
      `(${results.length} deals)`,
    `${pad('deals', 9)} ${pad('nodes', 14)} ${pad('ms', 10)} ${pad('ms/deal', 9)}`,
  ]
  for (const { first, last } of rowsFor(results.length)) {
    const t = tally(results.slice(first, last + 1))
    lines.push(
      `${pad(`${first + 1}-${last + 1}`, 9)} ${pad(t.nodes, 14)} ${pad(t.ms.toFixed(1), 10)} ` +
        `${pad((t.ms / t.deals).toFixed(1), 9)}`
    )
  }
  const total = tally(results)
  lines.push(
    `${total.deals} deals, ${total.nodes} nodes, ${(total.ms / 1000).toFixed(2)} s of solving, ` +
      `${(total.nodes / Math.max(total.ms, 1e-6) / 1000).toFixed(2)} M nodes/s`
  )
  if (total.matches === true) lines.push('tables and nodes match the native tool')
  if (total.matches === false) lines.push('MISMATCH: tables or nodes differ from the native tool')
  return lines.join('\n')
}
