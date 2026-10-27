// The benchmark page's analysis suite.
//
// Solves a preset of fixed deals' full double-dummy tables in a worker, fills
// the table ten deals to a row as results arrive, checks every table and node
// count against the native tool's, and offers the result as text to paste.
// See ./analysis.worker.js for the engine side and ../lib/ddBench.js for the
// bookkeeping.

import { lock } from './lock.js'
import { DEALS_PER_ROW, expected, resultsText, rowsFor, tally } from '../lib/ddBench.js'
import { platformName } from '../lib/claimBench.js'
import { reportDdBench } from '../lib/telemetry.js'

const $ = (id) => document.getElementById(id)
const presetChoice = $('dd-presets')
const startButton = $('dd-start')
const stopButton = $('dd-stop')
const copyButton = $('dd-copy')
const status = $('dd-status')
const progress = $('dd-progress')
const tbody = $('dd-rows')
const summary = $('dd-summary')

let worker = null
let catalogue = null
let run = null
let running = false

function spawn() {
  worker = new Worker(new URL('./analysis.worker.js', import.meta.url), { type: 'module' })
  worker.onmessage = ({ data }) => handlers[data.op]?.(data)
  worker.onerror = (event) => fail(event.message || 'the engine failed to start')
}

function fail(message) {
  status.textContent = `Something went wrong: ${message}`
  run = null
  setRunning(false)
}

function setRunning(value) {
  running = value
  if (running) lock.take('analysis')
  else lock.release('analysis')
  refresh()
}

/** Enable what can be used now; the other suite running blocks a start. */
function refresh() {
  startButton.disabled = running || !catalogue || lock.holder !== null
  stopButton.disabled = !running
  presetChoice.disabled = running
  copyButton.disabled = running || !run?.finished
}

lock.subscribe(refresh)

const handlers = {
  catalogue({ catalogue: c }) {
    catalogue = c
    presetChoice.querySelector('.status')?.remove()
    c.presets.forEach((p, i) => {
      const label = document.createElement('label')
      const input = document.createElement('input')
      input.type = 'radio'
      input.name = 'dd-preset'
      input.value = p.name
      input.checked = i === 0
      const text =
        p.name === 'quick'
          ? `Quick: ${p.deals} deals, a few seconds`
          : `Full: all ${p.deals} deals, a minute or so on a laptop`
      label.append(input, ` ${text}`)
      presetChoice.append(label)
    })
    status.textContent = `Ready. Engine version ${c.version}.`
    refresh()
  },

  deal({ index, tricks, nodes, ms }) {
    run.results.push({ tricks, nodes, ms, expected: expected(index) })
    progress.value = run.results.length
    status.textContent = `Solving deal ${index + 1} of ${run.deals}`
    drawRow(Math.floor(index / DEALS_PER_ROW))
  },

  done() {
    run.finished = true
    const total = tally(run.results)
    status.textContent = 'Finished.'
    drawSummary(total)
    reportDdBench({
      preset: run.preset,
      ms: total.ms,
      deals: total.deals,
      nodes: total.nodes,
      mismatch: total.matches === false,
    })
    setRunning(false)
  },

  error({ message }) {
    fail(message)
  },
}

/** Draw, or redraw, one row of ten deals. */
function drawRow(i) {
  const { first, last } = rowsFor(run.deals)[i]
  const t = tally(run.results.slice(first, last + 1))
  const cells = [
    `${first + 1}–${last + 1}`,
    t.nodes.toLocaleString(),
    t.ms.toFixed(1),
    t.deals ? (t.ms / t.deals).toFixed(1) : '',
    t.matches === null ? '' : t.matches ? '✓' : '✗',
  ]
  let tr = tbody.children[i]
  if (!tr) {
    tr = document.createElement('tr')
    for (let c = 0; c < cells.length; c++) tr.append(document.createElement('td'))
    tbody.append(tr)
  }
  cells.forEach((value, c) => {
    tr.children[c].textContent = String(value)
  })
  tr.className = t.matches === false ? 'mismatch' : ''
}

function drawSummary(total) {
  summary.textContent = ''
  const line = (text, className) => {
    const p = document.createElement('p')
    p.textContent = text
    if (className) p.className = className
    summary.append(p)
  }
  line(
    `${total.deals} tables solved in ${(total.ms / 1000).toFixed(2)} s ` +
      `(${(total.ms / total.deals).toFixed(0)} ms a deal, ` +
      `${(total.nodes / Math.max(total.ms, 1e-6) / 1000).toFixed(2)} million positions a second).`
  )
  if (total.matches === true) {
    line('Every table and node count matched the native tool.', 'good')
  } else if (total.matches === false) {
    line(
      'Some tables or node counts differ from the native tool. That is a bug in the browser ' +
        'build, not a slow device. Please report it, with the copied results.',
      'bad'
    )
  }
}

startButton.addEventListener('click', () => {
  const chosen = presetChoice.querySelector('input[name="dd-preset"]:checked')?.value
  const preset = catalogue.presets.find((p) => p.name === chosen) ?? catalogue.presets[0]
  run = { preset: preset.name, deals: preset.deals, results: [], finished: false }
  tbody.textContent = ''
  summary.textContent = ''
  progress.max = preset.deals
  progress.value = 0
  status.textContent = 'Starting…'
  setRunning(true)
  worker.postMessage({ op: 'run', deals: preset.deals })
})

stopButton.addEventListener('click', () => {
  worker.terminate()
  run = null
  status.textContent = 'Stopped. Nothing was recorded.'
  spawn()
  setRunning(false)
})

copyButton.addEventListener('click', async () => {
  const text = resultsText({
    preset: run.preset,
    version: catalogue.version,
    platform: platformName(),
    results: run.results,
  })
  try {
    await navigator.clipboard.writeText(text)
    copyButton.textContent = 'Copied'
  } catch {
    const pre = document.createElement('pre')
    pre.textContent = text
    summary.append(pre)
    copyButton.textContent = 'Copy it from below'
  }
  setTimeout(() => {
    copyButton.textContent = 'Copy results'
  }, 2000)
})

spawn()
worker.postMessage({ op: 'catalogue' })
