// The claim benchmark page's bookkeeping: what to run, whether the answers
// match the native tool, and the text a person copies out.
//
// Kept apart from the page so it can be tested without a browser or the
// engine. The claims themselves come from the engine (`ClaimBenchCase`), which
// generates each from a fixed seed; the presets come from it too
// (`claim_bench_presets`), so this page and `bridge-claim bench` cannot drift
// apart on what "quick" means.

import fixture from './fixtures/claim-bench-v1.json'

/** Every row a preset runs, in report order. */
export function rowsOf(preset) {
  return preset.workloads.flatMap((workload) => preset.sizes.map((size) => ({ workload, size })))
}

/**
 * What the native tool found for one claim: its verdict letter and nodes.
 *
 * `null` when the fixture has no such claim, or was made with a different
 * budget, in which case nothing can be said either way.
 */
export function expected(workload, size, index, budget = fixture.budget) {
  if (budget !== fixture.budget) return null
  const row = fixture.rows[`${workload}/${size}`]
  if (!row || index >= row.verdicts.length) return null
  return { letter: row.verdicts[index], nodes: row.nodes[index] }
}

/**
 * One row's totals so far, from its claims' results.
 *
 * `matches` is `true` when every claim with a native result agreed with it,
 * `false` when any did not, and `null` when there was nothing to compare.
 * Verdicts and node counts are deterministic, so a disagreement is a bug in
 * the browser build, never a slow device.
 */
export function tally(results) {
  const count = (letter) => results.filter((r) => r.letter === letter).length
  const compared = results.filter((r) => r.expected)
  return {
    claims: results.length,
    valid: count('V'),
    invalid: count('X'),
    undecided: count('?'),
    nodes: results.reduce((sum, r) => sum + r.nodes, 0),
    ms: results.reduce((sum, r) => sum + r.ms, 0),
    verdicts: results.map((r) => r.letter).join(''),
    matches: compared.length
      ? compared.every((r) => r.letter === r.expected.letter && r.nodes === r.expected.nodes)
      : null,
  }
}

/** Whole-run totals over every row's tally. */
export function overall(tallies) {
  const sum = (key) => tallies.reduce((total, t) => total + t[key], 0)
  const compared = tallies.filter((t) => t.matches !== null)
  return {
    claims: sum('claims'),
    undecided: sum('undecided'),
    nodes: sum('nodes'),
    ms: sum('ms'),
    matches: compared.length ? compared.every((t) => t.matches) : null,
  }
}

const pad = (value, width) => String(value).padStart(width)

/**
 * The results as plain text, laid out like `bridge-claim bench` so the two can
 * be pasted side by side.
 */
export function resultsText({ preset, budget, version, platform, rows, tallies }) {
  const total = overall(tallies)
  const lines = [
    `bridge-claim ${version} in ${platform}, 1 thread (browser), budget ${budget} nodes, ` +
      `preset ${preset}`,
    `${'workload'.padEnd(10)} ${pad('size', 4)} ${pad('V', 4)} ${pad('X', 4)} ${pad('?', 4)} ` +
      `${pad('nodes', 12)} ${pad('ms', 10)} ${pad('ms/claim', 9)}  verdicts`,
  ]
  rows.forEach((row, i) => {
    const t = tallies[i]
    if (!t || t.claims === 0) return
    lines.push(
      `${row.workload.padEnd(10)} ${pad(row.size, 4)} ${pad(t.valid, 4)} ${pad(t.invalid, 4)} ` +
        `${pad(t.undecided, 4)} ${pad(t.nodes, 12)} ${pad(t.ms.toFixed(1), 10)} ` +
        `${pad((t.ms / t.claims).toFixed(2), 9)}  ${t.verdicts}`
    )
  })
  lines.push(
    `${total.claims} claims, ${total.nodes} nodes, ${(total.ms / 1000).toFixed(2)} s of checking`
  )
  if (total.matches === true) lines.push('verdicts and nodes match the native tool')
  if (total.matches === false) lines.push('MISMATCH: verdicts or nodes differ from the native tool')
  return lines.join('\n')
}

/**
 * A coarse browser and system name for the copied text, like the server
 * derives for the statistics. Local only: the copied text goes wherever the
 * person pastes it, and nowhere else.
 */
export function platformName(ua = typeof navigator === 'undefined' ? '' : navigator.userAgent) {
  const major = (re) => ua.match(re)?.[1] ?? ''
  let browser = 'a browser'
  if (/Edg\//.test(ua)) browser = `Edge ${major(/Edg\/(\d+)/)}`
  else if (/CriOS\//.test(ua)) browser = `Chrome ${major(/CriOS\/(\d+)/)}`
  else if (/FxiOS\//.test(ua)) browser = `Firefox ${major(/FxiOS\/(\d+)/)}`
  else if (/Firefox\//.test(ua)) browser = `Firefox ${major(/Firefox\/(\d+)/)}`
  else if (/Chrome\//.test(ua)) browser = `Chrome ${major(/Chrome\/(\d+)/)}`
  else if (/Safari\//.test(ua) && /Version\//.test(ua)) browser = `Safari ${major(/Version\/(\d+)/)}`
  let os = ''
  if (/iPhone|iPad|iPod/.test(ua)) os = 'iOS'
  else if (/Android/.test(ua)) os = 'Android'
  else if (/Mac OS X/.test(ua)) os = 'macOS'
  else if (/Windows/.test(ua)) os = 'Windows'
  else if (/Linux/.test(ua)) os = 'Linux'
  return os ? `${browser} on ${os}` : browser
}
