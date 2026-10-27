// The public statistics dashboard.
//
// A static page that fetches one pre-aggregated blob from `/api/stats` and
// draws it. No build step and no dependencies: this file is copied verbatim out
// of `web/public/` by Vite, so it must run in the browser exactly as written.
// That is also why the charts are hand-built SVG — the page's own
// Content-Security-Policy is `default-src 'none'`, which forbids fetching a
// charting library even if we wanted one.
//
// The bias throughout is toward saying how much data a number rests on. With
// little traffic almost every figure here is one or two events, and a chart
// that renders a single point with the same confidence as a thousand is the
// main way a page like this misleads.

/** Chart palette. Two categorical slots, validated against this page's white
 *  chart surface — see the note in stats.html before changing either. */
const SERIES_1 = '#2a78d6'
const SERIES_2 = '#eb6834'
const AXIS = '#c3c2b7'
const GRID = '#e1e0d9'
const MUTED = '#898781'
const SURFACE = '#ffffff'

/**
 * What the pre-solve warning predicts, on the reference machine.
 *
 * Mirrors `expectedTotalMs` in web/src/lib/benchmark.js: a device scoring `s`
 * is told to expect `950 * 100 / s` milliseconds. Kept here as a constant
 * rather than imported because this file is not bundled — **if the estimate
 * changes there, change it here too**, or the accuracy panel silently starts
 * grading against a formula the app no longer uses.
 */
const PREDICTED_MS_AT_100 = 950

const root = document.getElementById('root')
const metaLine = document.getElementById('meta')
const tip = document.getElementById('tip')

/* ------------------------------------------------------------------ format */

/**
 * Milliseconds, at the precision a reader can actually act on.
 *
 * Zero is spelled `0` rather than `<1 ms`: it is an axis origin and a genuinely
 * absent measurement, not a very small one. Trailing zeros come off the seconds
 * form so an axis tick reads `1 s` rather than `1.00 s`.
 */
function ms(value) {
  if (value === null || value === undefined || !Number.isFinite(value)) return '—'
  if (value === 0) return '0'
  if (value < 1) return '<1 ms'
  if (value < 1000) return `${Math.round(value)} ms`
  return `${parseFloat((value / 1000).toFixed(value < 10000 ? 2 : 1))} s`
}

/** Counts, thousands-separated. */
function count(value) {
  return Number(value || 0).toLocaleString('en-GB')
}

/** A UTC offset in minutes, as the label a person recognises. */
function utcOffset(minutes) {
  const sign = minutes < 0 ? '-' : '+'
  const abs = Math.abs(minutes)
  const h = Math.floor(abs / 60)
  const m = abs % 60
  return `UTC${sign}${h}${m ? `:${String(m).padStart(2, '0')}` : ''}`
}

function el(tag, attrs = {}, text) {
  const node = document.createElementNS('http://www.w3.org/2000/svg', tag)
  for (const [k, v] of Object.entries(attrs)) node.setAttribute(k, String(v))
  if (text !== undefined) node.textContent = text
  return node
}

function html(tag, className, text) {
  const node = document.createElement(tag)
  if (className) node.className = className
  if (text !== undefined) node.textContent = text
  return node
}

/* ----------------------------------------------------------------- tooltip */

// One tooltip element, moved to whichever mark is under the pointer. Delegated
// rather than per-mark listeners so a chart with 90 bars still costs one
// handler. Marks opt in by carrying `data-tip`.
document.addEventListener('pointermove', (event) => {
  const mark = event.target.closest?.('[data-tip]')
  if (!mark) {
    tip.style.opacity = '0'
    return
  }
  tip.textContent = mark.getAttribute('data-tip')
  tip.style.opacity = '1'
  // Flipped to the left near the right edge so it never leaves the viewport.
  const overflowing = event.clientX + 200 > window.innerWidth
  tip.style.left = `${overflowing ? event.clientX - tip.offsetWidth - 12 : event.clientX + 12}px`
  tip.style.top = `${event.clientY + 16}px`
})

/* ------------------------------------------------------------------- charts */

/**
 * The chart's coordinate width, kept close to the width it is actually painted
 * at.
 *
 * An SVG with a fixed viewBox scaled down to a phone scales its *text* down
 * too: a 720-unit box rendered into 330 CSS pixels turns an 11px axis label
 * into a 5px one, which is not readable by anyone. Matching the viewBox to the
 * rendered width keeps a unit worth roughly a pixel, so the labels stay the
 * size they claim to be at every viewport.
 */
let W = 720
const H = 250
const PAD = { top: 12, right: 16, bottom: 36, left: 52 }
const PLOT_H = H - PAD.top - PAD.bottom

/** Current plot width. A function, not a constant — `W` moves with the viewport. */
function plotW() {
  return W - PAD.left - PAD.right
}

/** Re-measure against the column the charts are drawn into. */
function measure() {
  W = Math.max(320, Math.min(720, (root.clientWidth || 720) - 40))
}

/** Round an axis maximum up to something with no spurious precision. */
function niceMax(value) {
  if (!value || value <= 0) return 1
  const magnitude = 10 ** Math.floor(Math.log10(value))
  for (const step of [1, 1.5, 2, 2.5, 3, 4, 5, 7.5, 10]) {
    if (value <= step * magnitude) return step * magnitude
  }
  return 10 * magnitude
}

/**
 * A bar with its data-end rounded and its baseline square.
 *
 * Drawn as a path rather than `rect rx`, which would round all four corners and
 * detach the bar from the baseline it is measured from.
 */
function barPath(x, y, w, h, r = 4) {
  const radius = Math.min(r, w / 2, h)
  return [
    `M${x},${y + h}`,
    `V${y + radius}`,
    `Q${x},${y} ${x + radius},${y}`,
    `H${x + w - radius}`,
    `Q${x + w},${y} ${x + w},${y + radius}`,
    `V${y + h}`,
    'Z',
  ].join(' ')
}

/**
 * Pick the axis maximum and how many gridlines to divide it into.
 *
 * `integer` matters more than it looks: a count axis divided into quarters
 * produces ticks like 0.75 and 2.25, and a chart whose axis offers three
 * quarters of an event is quietly telling the reader it does not know what it
 * is counting. For those the step is rounded up to a whole number first and the
 * maximum follows from it.
 */
function axisScale(rawMax, integer) {
  if (!integer) return { max: niceMax(rawMax), ticks: 4 }
  const max = Math.max(1, Math.ceil(rawMax))
  if (max <= 5) return { max, ticks: max }
  const step = Math.ceil(niceMax(max) / 4)
  return { max: step * 4, ticks: 4 }
}

/** The y-axis: hairline gridlines and their labels. */
function drawAxis(svg, max, formatValue, ticks = 4) {
  for (let i = 0; i <= ticks; i += 1) {
    const value = (max / ticks) * i
    const y = PAD.top + PLOT_H - (value / max) * PLOT_H
    svg.append(
      el('line', {
        x1: PAD.left,
        x2: PAD.left + plotW(),
        y1: y,
        y2: y,
        stroke: i === 0 ? AXIS : GRID,
        'stroke-width': 1,
      })
    )
    svg.append(
      el(
        'text',
        {
          x: PAD.left - 8,
          y: y + 4,
          'text-anchor': 'end',
          fill: MUTED,
          'font-size': 11,
          'font-variant-numeric': 'tabular-nums',
        },
        formatValue(value)
      )
    )
  }
}

/**
 * A column chart of one or two series.
 *
 * `series` is `[{ key, color, label }]`. Two is the ceiling by construction —
 * beyond that the bars get too thin to read and the honest form is a table,
 * which every panel here already has underneath it.
 */
function columns(rows, { series, labelOf, formatValue = count, tipOf, integer = true }) {
  const svg = el('svg', { viewBox: `0 0 ${W} ${H}`, role: 'img' })
  const { max, ticks } = axisScale(
    Math.max(...rows.flatMap((r) => series.map((s) => Number(r[s.key]) || 0))),
    integer
  )
  drawAxis(svg, max, formatValue, ticks)

  const band = plotW() / rows.length
  // ≤24px per the mark spec, and never filling the band — the leftover is air.
  const barW = Math.max(2, Math.min(24, (band - 8) / series.length - (series.length > 1 ? 2 : 0)))
  const groupW = barW * series.length + (series.length - 1) * 2

  rows.forEach((row, i) => {
    const centre = PAD.left + band * i + band / 2

    series.forEach((s, j) => {
      const value = Number(row[s.key]) || 0
      const h = max > 0 ? (value / max) * PLOT_H : 0
      const x = centre - groupW / 2 + j * (barW + 2)
      const y = PAD.top + PLOT_H - h
      if (h > 0) {
        svg.append(
          el('path', {
            d: barPath(x, y, barW, h),
            fill: s.color,
            'data-tip': tipOf(row, s),
          })
        )
      }
    })

    // Thin out x labels rather than let them collide — measured off the band
    // width against an 11px face.
    const every = Math.ceil((rows.length * 46) / plotW())
    if (i % every === 0) {
      svg.append(
        el(
          'text',
          {
            x: centre,
            y: PAD.top + PLOT_H + 18,
            'text-anchor': 'middle',
            fill: MUTED,
            'font-size': 11,
          },
          labelOf(row, i)
        )
      )
    }
  })

  return svg
}

/**
 * A line chart of one or two series over an ordered axis.
 *
 * End-dots carry a 2px ring in the surface colour so they stay legible where
 * the two series cross.
 */
function lines(rows, { series, labelOf, formatValue = count, tipOf, integer = false }) {
  const svg = el('svg', { viewBox: `0 0 ${W} ${H}`, role: 'img' })
  const { max, ticks } = axisScale(
    Math.max(...rows.flatMap((r) => series.map((s) => Number(r[s.key]) || 0))),
    integer
  )
  drawAxis(svg, max, formatValue, ticks)

  // A single point has no line to draw, so it is plotted as a dot alone.
  const step = rows.length > 1 ? plotW() / (rows.length - 1) : 0
  const xAt = (i) => (rows.length > 1 ? PAD.left + step * i : PAD.left + plotW() / 2)
  const yAt = (v) => PAD.top + PLOT_H - (max > 0 ? ((Number(v) || 0) / max) * PLOT_H : 0)

  series.forEach((s) => {
    if (rows.length > 1) {
      svg.append(
        el('polyline', {
          points: rows.map((r, i) => `${xAt(i)},${yAt(r[s.key])}`).join(' '),
          fill: 'none',
          stroke: s.color,
          'stroke-width': 2,
          'stroke-linejoin': 'round',
          'stroke-linecap': 'round',
        })
      )
    }
    rows.forEach((row, i) => {
      svg.append(
        el('circle', {
          cx: xAt(i),
          cy: yAt(row[s.key]),
          r: 4,
          fill: s.color,
          stroke: SURFACE,
          'stroke-width': 2,
          'data-tip': tipOf(row, s),
        })
      )
    })
  })

  const every = Math.ceil((rows.length * 62) / plotW())
  rows.forEach((row, i) => {
    if (i % every !== 0 && i !== rows.length - 1) return
    svg.append(
      el(
        'text',
        { x: xAt(i), y: PAD.top + PLOT_H + 18, 'text-anchor': 'middle', fill: MUTED, 'font-size': 11 },
        labelOf(row, i)
      )
    )
  })

  return svg
}

/* -------------------------------------------------------------- assembling */

/** A legend. Present whenever there are two series — never colour alone. */
function legend(series) {
  const box = html('div', 'legend')
  for (const s of series) {
    const item = document.createElement('span')
    const key = html('i', 'key')
    key.style.background = s.color
    item.append(key, document.createTextNode(s.label))
    box.append(item)
  }
  return box
}

/** The table that repeats a chart's numbers, for anyone the chart fails. */
function table(rows, columnsSpec) {
  const t = document.createElement('table')
  const head = document.createElement('tr')
  for (const c of columnsSpec) {
    const th = html('th', c.numeric ? 'n' : '', c.label)
    head.append(th)
  }
  // Note `append` returns undefined rather than the node, so this cannot be
  // chained — build the row into the section, then the section into the table.
  const thead = document.createElement('thead')
  thead.append(head)
  t.append(thead)

  const body = document.createElement('tbody')
  for (const row of rows) {
    const tr = document.createElement('tr')
    for (const c of columnsSpec) tr.append(html('td', c.numeric ? 'n' : '', c.value(row)))
    body.append(tr)
  }
  t.append(body)
  return t
}

/**
 * One panel: a heading, an explanation of what it is for, the chart, and the
 * same numbers as a table underneath.
 *
 * `rows` empty renders a stated absence rather than an empty axis — a blank
 * chart reads as zero, which is a factual claim we have no basis for.
 */
function panel({ title, sub, rows, series, chart, columns: columnsSpec, note }) {
  const card = html('section', 'card')
  card.append(html('h2', null, title))
  if (sub) card.append(html('p', 'sub', sub))

  if (!rows || rows.length === 0) {
    card.append(html('p', 'empty', 'No data in this window yet.'))
    return card
  }

  if (series && series.length > 1) card.append(legend(series))
  if (note) card.append(html('p', 'sub', note))
  if (chart) card.append(chart)

  if (columnsSpec) {
    const details = document.createElement('details')
    details.append(html('summary', null, 'Data table'))
    details.append(table(rows, columnsSpec))
    card.append(details)
  }

  return card
}

/** A plain table panel, for the dimensions where a chart adds nothing. */
function tablePanel({ title, sub, rows, columns: columnsSpec, suppressed }) {
  const card = html('section', 'card')
  card.append(html('h2', null, title))
  if (sub) card.append(html('p', 'sub', sub))
  if (!rows || rows.length === 0) {
    card.append(
      html(
        'p',
        'empty',
        suppressed
          ? `Nothing above the ${suppressed}-event reporting threshold yet.`
          : 'No data in this window yet.'
      )
    )
    return card
  }
  card.append(table(rows, columnsSpec))
  return card
}

function tile(label, value) {
  const box = html('div', 'tile')
  box.append(html('div', 'tile-label', label))
  box.append(html('div', 'tile-value', value))
  return box
}

/* ---------------------------------------------------------------- the page */

function render(data) {
  const p = data.panels || {}
  const totals = p.totals?.[0] || {}
  const percentiles = p.solvePercentiles?.[0] || {}
  const perCard = p.msPerCard?.[0] || {}
  const cold = p.coldStart?.[0] || {}

  root.textContent = ''

  const solves = Number(totals.solves) || 0
  const loads = Number(totals.loads) || 0

  // Sample size is the caveat that matters at this volume, and it has to be
  // stated rather than left to be worked out from the bucket counts.
  if (solves < 50) {
    const caveat = html('div', 'caveat')
    caveat.append(html('strong', null, 'Read these numbers with care. '))
    caveat.append(
      document.createTextNode(
        `The whole window holds ${count(solves)} ${solves === 1 ? 'analysis' : 'analyses'} and ` +
          `${count(loads)} page ${loads === 1 ? 'load' : 'loads'}, which is too few for any ` +
          'percentile here to be stable, and some of it is still the author testing. ' +
          'Bucket counts are shown throughout so you can see what each rests on.'
      )
    )
    root.append(caveat)
  }

  // Exactly one hero figure, and it is a performance number rather than a
  // usage one: what this page is for is knowing how the tool behaves on real
  // hardware.
  const hero = html('div', 'hero')
  hero.append(html('div', 'hero-value', ms(Number(percentiles.p50))))
  hero.append(
    html(
      'div',
      'hero-label',
      `median analysis, across ${count(percentiles.n)} ${
        Number(percentiles.n) === 1 ? 'measurement' : 'measurements'
      } in the last ${data.windowDays} days`
    )
  )
  root.append(hero)

  const tiles = html('div', 'tiles')
  tiles.append(tile('Analyses', count(solves)))
  tiles.append(tile('Page loads', count(loads)))
  tiles.append(
    tile('Analyses per load', loads > 0 ? (solves / loads).toFixed(2) : '—')
  )
  tiles.append(tile('90th percentile', ms(Number(percentiles.p90))))
  tiles.append(tile('Slowest seen', ms(Number(percentiles.hi))))
  tiles.append(tile('Per card (median)', ms(Number(perCard.p50))))
  tiles.append(tile('Countries', count(totals.countries)))
  tiles.append(tile('Cancelled', count(totals.cancelled)))
  root.append(tiles)

  /* --- the accuracy panel, which is the one that earns its place ---------- */

  const bench = (p.byBench || []).map((row) => ({
    ...row,
    predicted: Number(row.score) > 0 ? (PREDICTED_MS_AT_100 * 100) / Number(row.score) : 0,
  }))

  const accuracySeries = [
    { key: 'predicted', color: SERIES_2, label: 'Predicted before starting' },
    { key: 'p50', color: SERIES_1, label: 'Actually measured (median)' },
  ]

  // The single number this panel exists to produce: how far out the warning is.
  const ratios = bench.filter((r) => Number(r.p50) > 0).map((r) => r.predicted / Number(r.p50))
  const overBy = ratios.length
    ? ratios.slice().sort((a, b) => a - b)[Math.floor(ratios.length / 2)]
    : null

  root.append(
    panel({
      title: 'Is the pre-solve warning honest?',
      sub:
        'Before any work starts, a quick probe times one small deal and the page predicts how ' +
        'long the analysis will take. This compares that prediction against what then actually ' +
        'happened, grouped by the device-speed score the probe produced.',
      note:
        overBy === null
          ? undefined
          : `Median over-prediction across the bands below: ${overBy.toFixed(1)}×. ` +
            (overBy > 1.5
              ? 'The warning is telling people to expect a wait substantially longer than they get.'
              : 'Prediction and reality are within a reasonable factor of each other.'),
      rows: bench,
      series: accuracySeries,
      chart: bench.length
        ? columns(bench, {
            series: accuracySeries,
            formatValue: ms,
            // Milliseconds, so the axis is free to use fractional steps.
            integer: false,
            labelOf: (row) => `${row.band}`,
            tipOf: (row, s) =>
              `Score ${row.band}–${Number(row.band) + 24} · ${s.label}: ${ms(Number(row[s.key]))} · ${count(
                row.n
              )} ${Number(row.n) === 1 ? 'event' : 'events'}`,
          })
        : null,
      columns: [
        { label: 'Speed score', value: (r) => `${r.band}–${Number(r.band) + 24}` },
        { label: 'Predicted', numeric: true, value: (r) => ms(r.predicted) },
        { label: 'Actual p50', numeric: true, value: (r) => ms(Number(r.p50)) },
        { label: 'Actual p90', numeric: true, value: (r) => ms(Number(r.p90)) },
        { label: 'Events', numeric: true, value: (r) => count(r.n) },
      ],
    })
  )

  /* --- distribution ------------------------------------------------------ */

  const hist = p.solveHistogram || []
  const histSeries = [{ key: 'n', color: SERIES_1, label: 'Analyses' }]
  root.append(
    panel({
      title: 'How long an analysis takes',
      sub:
        'The distribution rather than the average, because the average of a long-tailed ' +
        'measurement describes nobody. Each bar is a 250 ms band.',
      rows: hist,
      series: histSeries,
      chart: hist.length
        ? columns(hist, {
            series: histSeries,
            labelOf: (row) => ms(Number(row.bucket)),
            tipOf: (row) =>
              `${ms(Number(row.bucket))}–${ms(Number(row.bucket) + 250)}: ${count(row.n)} ${
                Number(row.n) === 1 ? 'analysis' : 'analyses'
              }`,
          })
        : null,
      columns: [
        { label: 'Band', value: (r) => `${ms(Number(r.bucket))} – ${ms(Number(r.bucket) + 250)}` },
        { label: 'Analyses', numeric: true, value: (r) => count(r.n) },
      ],
    })
  )

  /* --- variation over time ----------------------------------------------- */

  const daily = p.dailyPerf || []
  const dailySeries = [
    { key: 'p50', color: SERIES_1, label: 'Median' },
    { key: 'p90', color: SERIES_2, label: '90th percentile' },
  ]
  root.append(
    panel({
      title: 'Analysis time, day by day',
      sub: 'Whether the tool is getting faster or slower, and how wide the spread is on any given day.',
      rows: daily,
      series: dailySeries,
      chart: daily.length
        ? lines(daily, {
            series: dailySeries,
            formatValue: ms,
            labelOf: (row) => String(row.d).slice(5),
            tipOf: (row, s) => `${row.d} · ${s.label}: ${ms(Number(row[s.key]))} · ${count(row.n)} events`,
          })
        : null,
      columns: [
        { label: 'Day', value: (r) => String(r.d) },
        { label: 'Median', numeric: true, value: (r) => ms(Number(r.p50)) },
        { label: 'p90', numeric: true, value: (r) => ms(Number(r.p90)) },
        { label: 'Analyses', numeric: true, value: (r) => count(r.n) },
      ],
    })
  )

  const volume = p.daily || []
  const volumeSeries = [
    { key: 'loads', color: SERIES_1, label: 'Page loads' },
    { key: 'solves', color: SERIES_2, label: 'Analyses' },
  ]
  root.append(
    panel({
      title: 'Use, day by day',
      sub:
        'Loads against analyses. A lot of loads with few analyses would mean people arrive and ' +
        'leave without running anything.',
      rows: volume,
      series: volumeSeries,
      chart: volume.length
        ? columns(volume, {
            series: volumeSeries,
            labelOf: (row) => String(row.d).slice(5),
            tipOf: (row, s) => `${row.d} · ${s.label}: ${count(row[s.key])}`,
          })
        : null,
      columns: [
        { label: 'Day', value: (r) => String(r.d) },
        { label: 'Loads', numeric: true, value: (r) => count(r.loads) },
        { label: 'Analyses', numeric: true, value: (r) => count(r.solves) },
      ],
    })
  )

  /* --- the device fleet --------------------------------------------------- */

  const fleet = p.benchDist || []
  const fleetSeries = [{ key: 'n', color: SERIES_1, label: 'Events' }]
  root.append(
    panel({
      title: 'How fast the devices are',
      sub:
        'The speed score each device measured for itself, where the reference machine — a Mac ' +
        'mini M4 Pro — scores 100. This is the performance envelope the tool actually has to run in.',
      rows: fleet,
      series: fleetSeries,
      chart: fleet.length
        ? columns(fleet, {
            series: fleetSeries,
            labelOf: (row) => `${row.band}`,
            tipOf: (row) =>
              `Score ${row.band}–${Number(row.band) + 24}: ${count(row.n)} ${
                Number(row.n) === 1 ? 'event' : 'events'
              }`,
          })
        : null,
      columns: [
        { label: 'Speed score', value: (r) => `${r.band}–${Number(r.band) + 24}` },
        { label: 'Events', numeric: true, value: (r) => count(r.n) },
      ],
    })
  )

  root.append(
    tablePanel({
      title: 'By browser and operating system',
      sub: 'Where the slow platforms are. The engine is the same everywhere; the runtime is not.',
      rows: p.byPlatform || [],
      columns: [
        { label: 'Browser', value: (r) => String(r.browser || 'other') },
        { label: 'System', value: (r) => String(r.os || 'other') },
        { label: 'Median', numeric: true, value: (r) => ms(Number(r.p50)) },
        { label: 'p90', numeric: true, value: (r) => ms(Number(r.p90)) },
        { label: 'Analyses', numeric: true, value: (r) => count(r.n) },
      ],
    })
  )

  root.append(
    tablePanel({
      title: 'Claim checker benchmark',
      sub:
        'Runs of the claim benchmark page (claim-bench.html): the same fixed claims on every ' +
        'device, so the time is the device and browser, not the workload. "Wrong" counts runs ' +
        'whose verdicts differed from the native tool, which would be a bug in the browser build.',
      rows: p.claimBench || [],
      columns: [
        { label: 'Preset', value: (r) => String(r.preset || '?') },
        { label: 'Browser', value: (r) => String(r.browser || 'other') },
        { label: 'System', value: (r) => String(r.os || 'other') },
        { label: 'Median', numeric: true, value: (r) => ms(Number(r.p50)) },
        { label: 'Best', numeric: true, value: (r) => ms(Number(r.best)) },
        { label: 'Runs', numeric: true, value: (r) => count(r.n) },
        { label: 'Wrong', numeric: true, value: (r) => count(r.mismatched) },
      ],
    })
  )

  /* --- delivery, and the cache ------------------------------------------- */

  const coldTiles = html('section', 'card')
  coldTiles.append(html('h2', null, 'Getting the engine to the device'))
  coldTiles.append(
    html(
      'p',
      'sub',
      'Downloading and compiling the WebAssembly engine, once per visit. Measured on real ' +
        'connections rather than a local build — the claim being checked is that this is a small ' +
        'part of the wait compared with the analysis itself.'
    )
  )
  if (Number(cold.n) > 0) {
    const grid = html('div', 'tiles')
    grid.append(tile('Download (median)', ms(Number(cold.fetch_p50))))
    grid.append(tile('Download (p90)', ms(Number(cold.fetch_p90))))
    grid.append(tile('Compile (median)', ms(Number(cold.compile_p50))))
    grid.append(tile('Compile (p90)', ms(Number(cold.compile_p90))))
    grid.append(tile('Measurements', count(cold.n)))
    grid.style.marginBottom = '0'
    coldTiles.append(grid)
  } else {
    coldTiles.append(html('p', 'empty', 'No data in this window yet.'))
  }
  root.append(coldTiles)

  root.append(
    tablePanel({
      title: 'First analysis against later ones',
      sub:
        'The engine keeps a cache of positions it has already worked out, so the first analysis ' +
        'of a session costs more than the ones after it. This is that difference, as measured.',
      rows: p.coldVsWarm || [],
      columns: [
        { label: 'Run', value: (r) => (Number(r.cold) === 1 ? 'First of the session' : 'Later') },
        { label: 'Median', numeric: true, value: (r) => ms(Number(r.p50)) },
        { label: 'Analyses', numeric: true, value: (r) => count(r.n) },
      ],
    })
  )

  /* --- shape of the work and the audience --------------------------------- */

  const cards = p.cards || []
  const cardsSeries = [{ key: 'n', color: SERIES_1, label: 'Analyses' }]
  root.append(
    panel({
      title: 'How much of a hand gets analysed',
      sub: 'Cards in the deal being worked through. A full deal is 52; a partly played one is fewer.',
      rows: cards,
      series: cardsSeries,
      chart: cards.length
        ? columns(cards, {
            series: cardsSeries,
            labelOf: (row) => `${row.cards}`,
            tipOf: (row) => `${row.cards} cards: ${count(row.n)} analyses`,
          })
        : null,
      columns: [
        { label: 'Cards', numeric: true, value: (r) => count(r.cards) },
        { label: 'Analyses', numeric: true, value: (r) => count(r.n) },
      ],
    })
  )

  const tz = p.tz || []
  const tzSeries = [{ key: 'n', color: SERIES_1, label: 'Events' }]
  root.append(
    panel({
      title: 'Where in the world, by clock',
      sub:
        "Each visitor's UTC offset. A coarser signal than the country list below, and the one " +
        'that says what time of day the tool gets used.',
      rows: tz,
      series: tzSeries,
      chart: tz.length
        ? columns(tz, {
            series: tzSeries,
            labelOf: (row) => utcOffset(Number(row.tz)),
            tipOf: (row) => `${utcOffset(Number(row.tz))}: ${count(row.n)} events`,
          })
        : null,
      columns: [
        { label: 'Offset', value: (r) => utcOffset(Number(r.tz)) },
        { label: 'Events', numeric: true, value: (r) => count(r.n) },
      ],
    })
  )

  root.append(
    tablePanel({
      title: 'Countries',
      sub: `Fewer than ${data.minBucket} events from a country and it is left out, so that a lone visitor is never singled out.`,
      rows: p.countries || [],
      suppressed: data.minBucket,
      columns: [
        { label: 'Country', value: (r) => String(r.country) },
        { label: 'Events', numeric: true, value: (r) => count(r.n) },
      ],
    })
  )

  root.append(
    tablePanel({
      title: 'Sites embedding this page',
      sub: `Where the tool is used inside someone else's page. Same ${data.minBucket}-event threshold.`,
      rows: p.embeds || [],
      suppressed: data.minBucket,
      columns: [
        { label: 'Site', value: (r) => String(r.origin) },
        { label: 'Events', numeric: true, value: (r) => count(r.n) },
      ],
    })
  )

  root.append(
    tablePanel({
      title: 'Versions in use',
      sub: 'Which build produced each record. A performance change is otherwise indistinguishable from a slow device.',
      rows: p.versions || [],
      columns: [
        { label: 'Version', value: (r) => String(r.v) },
        { label: 'Events', numeric: true, value: (r) => count(r.n) },
      ],
    })
  )

  /* --- provenance --------------------------------------------------------- */

  const generated = data.generatedAt ? new Date(data.generatedAt) : null
  metaLine.textContent =
    `Covering the last ${data.windowDays} days. ` +
    (generated ? `Figures recalculated ${generated.toUTCString()}. ` : '') +
    (data.failed?.length ? `Panels unavailable: ${data.failed.join(', ')}. ` : '') +
    'Records are deleted automatically after three months.'
}

/** Nothing to show, and why. */
function renderPending(detail) {
  root.textContent = ''
  const card = html('section', 'card')
  card.append(html('h2', null, 'Not collecting yet'))
  card.append(
    html(
      'p',
      'sub',
      detail ||
        'No figures have been calculated yet. This page will fill in once the site has been running for a while.'
    )
  )
  root.append(card)
}

// Fetching and drawing are caught separately, on purpose. Wrapping both in one
// `catch` once meant a bug in the drawing code reported itself as "the
// statistics could not be loaded" — a wrong and very misleading message, since
// the data had arrived perfectly well. A failure to draw is ours, and it should
// say so and leave a real error in the console to work from.
fetch('./api/stats', { headers: { Accept: 'application/json' } })
  .then(async (response) => {
    const body = await response.json().catch(() => null)
    if (!response.ok || !body || body.status) {
      renderPending(body?.detail)
      return null
    }
    return body
  })
  .catch(() => {
    renderPending('The statistics could not be loaded. This page is served only from the main site.')
    return null
  })
  .then((body) => {
    if (!body) return

    const draw = () => {
      measure()
      try {
        render(body)
      } catch (error) {
        renderPending('The statistics loaded but could not be displayed. This is a bug in this page.')
        throw error
      }
    }

    draw()

    // Redraw when the width changes enough to matter — turning a phone, or
    // dragging a desktop window narrow. Charts are sized in coordinates chosen
    // from the measured width, so they cannot simply reflow with CSS. Debounced,
    // and skipped unless the width actually moved: a resize event also fires on
    // mobile when the address bar hides, which changes only the height.
    let lastWidth = window.innerWidth
    let pending
    window.addEventListener('resize', () => {
      if (window.innerWidth === lastWidth) return
      lastWidth = window.innerWidth
      clearTimeout(pending)
      pending = setTimeout(draw, 150)
    })
  })
