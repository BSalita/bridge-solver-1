// The claim benchmark page.
//
// Runs a preset of fixed claims through the claim checker in a worker, fills
// the table as results arrive, checks every answer against the native tool's,
// and offers the result as text to paste. See ./worker.js for the engine side
// and ../lib/claimBench.js for the bookkeeping.

import '../assets/tokens.css'
import './style.css'
import { expected, overall, platformName, resultsText, rowsOf, tally } from '../lib/claimBench.js'
import { reportClaimBench } from '../lib/telemetry.js'

const $ = (id) => document.getElementById(id)
const presetChoice = $('presets')
const startButton = $('start')
const stopButton = $('stop')
const copyButton = $('copy')
const status = $('status')
const progress = $('progress')
const tbody = $('rows')
const summary = $('summary')

let worker = null
let catalogue = null
let run = null

function spawn() {
  worker = new Worker(new URL('./worker.js', import.meta.url), { type: 'module' })
  worker.onmessage = ({ data }) => handlers[data.op]?.(data)
  worker.onerror = (event) => fail(event.message || 'the engine failed to start')
}

function fail(message) {
  status.textContent = `Something went wrong: ${message}`
  run = null
  setRunning(false)
}

function setRunning(running) {
  startButton.disabled = running || !catalogue
  stopButton.disabled = !running
  presetChoice.disabled = running
  copyButton.disabled = running || !run?.finished
}

const handlers = {
  catalogue({ catalogue: c }) {
    catalogue = c
    // Radio buttons rather than a <select>: with two choices both stay in
    // view, and there is no native popup to misplace.
    presetChoice.querySelector('.status')?.remove()
    c.presets.forEach((p, i) => {
      const claims = rowsOf(p).length * p.count
      const label = document.createElement('label')
      const input = document.createElement('input')
      input.type = 'radio'
      input.name = 'preset'
      input.value = p.name
      input.checked = i === 0
      const text =
        p.name === 'quick'
          ? `Quick: ${claims} claims, a few seconds`
          : `Full: ${claims} claims, under a minute on a laptop`
      label.append(input, ` ${text}`)
      presetChoice.append(label)
    })
    status.textContent = `Ready. Engine version ${c.version}.`
    setRunning(false)
  },

  claim({ row, index, letter, nodes, ms }) {
    const r = run.rows[row]
    r.results.push({
      letter,
      nodes,
      ms,
      expected: expected(r.workload, r.size, index, catalogue.budget),
    })
    run.done += 1
    progress.value = run.done
    status.textContent = `Checking ${r.workload}, ${r.size} cards per hand: claim ${index + 1} of ${run.count}`
    drawRow(row)
  },

  done() {
    run.finished = true
    const total = overall(run.rows.map((r) => tally(r.results)))
    status.textContent = 'Finished.'
    drawSummary(total)
    reportClaimBench({
      preset: run.preset,
      ms: total.ms,
      claims: total.claims,
      nodes: total.nodes,
      undecided: total.undecided,
      mismatch: total.matches === false,
    })
    setRunning(false)
  },

  error({ message }) {
    fail(message)
  },
}

/** Draw, or redraw, one row of the results table. */
function drawRow(i) {
  const r = run.rows[i]
  const t = tally(r.results)
  const cells = [
    r.workload,
    r.size,
    t.valid,
    t.invalid,
    t.undecided,
    t.nodes.toLocaleString(),
    t.ms.toFixed(1),
    t.claims ? (t.ms / t.claims).toFixed(2) : '',
    t.verdicts,
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
    `${total.claims} claims checked in ${(total.ms / 1000).toFixed(2)} s ` +
      `(${(total.claims / Math.max(total.ms / 1000, 1e-6)).toFixed(0)} claims per second), ` +
      `${total.undecided} undecided.`
  )
  if (total.matches === true) {
    line('Every verdict and node count matched the native tool.', 'good')
  } else if (total.matches === false) {
    line(
      'Some verdicts or node counts differ from the native tool. That is a bug in the ' +
        'browser build, not a slow device. Please report it, with the copied results.',
      'bad'
    )
  }
}

startButton.addEventListener('click', () => {
  const chosen = presetChoice.querySelector('input[name="preset"]:checked')?.value
  const preset = catalogue.presets.find((p) => p.name === chosen) ?? catalogue.presets[0]
  const rows = rowsOf(preset)
  run = {
    preset: preset.name,
    count: preset.count,
    rows: rows.map((r) => ({ ...r, results: [] })),
    done: 0,
    finished: false,
  }
  tbody.textContent = ''
  summary.textContent = ''
  progress.max = rows.length * preset.count
  progress.value = 0
  status.textContent = 'Starting…'
  setRunning(true)
  worker.postMessage({ op: 'run', rows, count: preset.count, budget: catalogue.budget })
})

stopButton.addEventListener('click', () => {
  worker.terminate()
  run = null
  status.textContent = 'Stopped. Nothing was recorded.'
  // A fresh engine for the next run. The presets are already in hand, so it
  // is not asked for them again.
  spawn()
  setRunning(false)
})

copyButton.addEventListener('click', async () => {
  const text = resultsText({
    preset: run.preset,
    budget: catalogue.budget,
    version: catalogue.version,
    platform: platformName(),
    rows: run.rows,
    tallies: run.rows.map((r) => tally(r.results)),
  })
  try {
    await navigator.clipboard.writeText(text)
    copyButton.textContent = 'Copied'
  } catch {
    // Clipboard access can be refused; show the text so it can be copied by hand.
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
