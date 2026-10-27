<script setup>
/**
 * The claim checker page: load a hand, go to any point in the play, and ask
 * whether a claim there holds however the unseen cards lie.
 *
 * The hand comes in the same ways as on the analysis page (paste, file, a URL
 * parameter) and is parsed by the same code. From there the page keeps a line
 * of play and a cursor into it. The line starts as the record; clicking a card
 * the player on turn may legally play extends it from the cursor, which is how
 * a bare deal with no play gets to a position worth claiming in.
 *
 * Every check runs in the engine's worker on this device. The hand never
 * leaves the browser; see telemetry.js for the anonymous timing record.
 */
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import BridgeTable from '../components/BridgeTable.vue'
import InputPanel from '../components/InputPanel.vue'
import { parseInput } from '../lib/input.js'
import { trumpFromContract } from '../lib/cardplay.js'
import { seatAtIndex, sideOf } from '../lib/cards.js'
import { checkClaim, playRequest } from '../lib/solver.js'
import { readUrl } from '../lib/session.js'
import {
  claimersFor,
  describePosition,
  positionAt,
  seatName,
  suggestedTricks,
  trickText,
  verdictText,
} from '../lib/claims.js'
import { reportClaim } from '../lib/telemetry.js'

const STRAINS = [
  { value: 'C', label: '♣ Clubs' },
  { value: 'D', label: '♦ Diamonds' },
  { value: 'H', label: '♥ Hearts' },
  { value: 'S', label: '♠ Spades' },
  { value: 'NT', label: 'Notrump' },
]
const SEATS = ['N', 'E', 'S', 'W']

const input = ref('')
const error = ref('')
const busy = ref(false)
const boards = ref([])
const boardIndex = ref(0)

/** The contract being played, editable because a PBN deal may carry none. */
const strain = ref('NT')
const declarer = ref('S')

/** The line of play, and how many of its cards have been played. */
const plays = ref([])
const at = ref(0)

const claimer = ref('S')
const tricks = ref(13)
const checking = ref(false)
const result = ref(null)
const maxima = ref(null)

const board = computed(() => boards.value[boardIndex.value] || null)
const trump = computed(() => (strain.value === 'NT' ? null : strain.value))
const leader = computed(() => seatAtIndex(declarer.value, 1))
/** The record's own plays, when the contract on screen is the record's. */
const record = computed(() => {
  const b = board.value
  if (!b?.plays?.length) return []
  const sameContract =
    b.declarer === declarer.value && (trumpFromContract(b.contract) || 'NT') === strain.value
  return sameContract ? b.plays : []
})
const offRecord = computed(
  () => plays.value.slice(0, at.value).some((card, i) => record.value[i] !== card)
)

const position = computed(() =>
  board.value
    ? positionAt({ hands: board.value.hands, leader: leader.value }, plays.value, at.value, trump.value)
    : null
)
const won = computed(() => {
  if (!position.value) return 0
  return position.value.taken[sideOf(claimer.value)]
})
const left = computed(() => 13 - Math.floor(at.value / 4))
const claimers = computed(() => claimersFor(declarer.value))

/** The legal cards for the player on turn, as badges BridgeTable can show. */
const badges = computed(() => {
  const p = position.value
  if (!p || p.over) return null
  // The same highlight as the analysis page's free play.
  const marks = {}
  for (const code of p.legal) marks[code] = { current: true }
  return { [p.nextSeat]: marks }
})

async function load(text) {
  input.value = text
  error.value = ''
  if (!text.trim()) {
    boards.value = []
    return
  }
  busy.value = true
  try {
    const parsed = await parseInput(text)
    boards.value = parsed.boards
    boardIndex.value = 0
    openBoard()
  } catch (e) {
    boards.value = []
    error.value = e?.message || String(e)
  } finally {
    busy.value = false
  }
}

/** Show the current board at the end of its record, ready to claim. */
function openBoard(atCard = null) {
  const b = board.value
  if (!b) return
  declarer.value = b.declarer || 'S'
  strain.value = trumpFromContract(b.contract) || 'NT'
  plays.value = [...(b.plays || [])]
  at.value = atCard == null ? plays.value.length : Math.max(0, Math.min(atCard, plays.value.length))
  claimer.value = declarer.value
  suggest()
}

/** Offer the record's claim, or every remaining trick, for the claimer. */
function suggest() {
  tricks.value = suggestedTricks({
    claim: at.value === record.value.length ? board.value?.claim : null,
    declarer: declarer.value,
    claimer: claimer.value,
    won: won.value,
    left: left.value,
  })
  result.value = null
}

function changeContract() {
  plays.value = [...record.value]
  at.value = plays.value.length
  if (!claimers.value.includes(claimer.value)) claimer.value = declarer.value
  maxima.value = null
  suggest()
}

function moveTo(n) {
  at.value = Math.max(0, Math.min(n, plays.value.length))
}

function backToRecord() {
  plays.value = [...record.value]
  at.value = Math.min(at.value, plays.value.length)
}

/** Play a card from the position on screen, discarding any line beyond it. */
function onCardClick({ seat, code }) {
  const p = position.value
  if (!p || seat !== p.nextSeat || !p.legal.has(code)) return
  plays.value = [...plays.value.slice(0, at.value), code]
  at.value += 1
}

watch([at, plays], () => {
  result.value = null
  maxima.value = null
  suggest()
})
watch(claimer, suggest)
watch(tricks, () => {
  result.value = null
})

function request(extra) {
  return {
    ...playRequest({
      hands: board.value.hands,
      trump: strain.value,
      declarer: declarer.value,
      leader: leader.value,
      plays: plays.value.slice(0, at.value),
    }),
    ...extra,
  }
}

async function check() {
  checking.value = true
  error.value = ''
  const started = performance.now()
  try {
    const answer = await checkClaim(request({ claimer: claimer.value, total_tricks: tricks.value }))
    reportClaim({ kind: 'check', ms: performance.now() - started, cards: at.value })
    result.value = {
      ...answer,
      ms: performance.now() - started,
      ...verdictText({
        result: answer,
        claimer: claimer.value,
        declarer: declarer.value,
        total: tricks.value,
      }),
    }
  } catch (e) {
    error.value = e?.message || String(e)
  } finally {
    checking.value = false
  }
}

/** The most each possible claimer is sure of, one after another. */
async function findMaxima() {
  checking.value = true
  error.value = ''
  const rows = []
  try {
    for (const seat of claimers.value) {
      const started = performance.now()
      const answer = await checkClaim(request({ claimer: seat }))
      const ms = performance.now() - started
      reportClaim({ kind: 'max', ms, cards: at.value })
      rows.push({ seat, ...answer, ms })
      maxima.value = [...rows]
    }
  } catch (e) {
    error.value = e?.message || String(e)
  } finally {
    checking.value = false
  }
}

function onKeydown(event) {
  if (event.metaKey || event.ctrlKey || event.altKey) return
  const tag = event.target?.tagName
  if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || event.target?.isContentEditable) {
    return
  }
  const step = { ArrowRight: 1, ArrowLeft: -1, ArrowDown: 4, ArrowUp: -4 }[event.key]
  if (step === undefined || !board.value) return
  event.preventDefault()
  moveTo(at.value + step)
}

onMounted(() => {
  window.addEventListener('keydown', onKeydown)
  // A hand handed over in the URL, from the analysis page's "Check a claim"
  // link or anywhere else; `at` is the number of cards played.
  const url = readUrl()
  if (url.hand) {
    const hash = location.hash.includes('?') ? location.hash.slice(location.hash.indexOf('?') + 1) : ''
    const params = new URLSearchParams(location.search)
    const read = (key) => params.get(key) ?? new URLSearchParams(hash).get(key)
    const atCard = Number.parseInt(read('at') ?? '', 10)
    const which = Number.parseInt(read('b') ?? '', 10)
    load(url.hand).then(() => {
      if (which > 0 && which < boards.value.length) boardIndex.value = which
      openBoard(Number.isFinite(atCard) ? atCard : null)
    })
  }
})
onUnmounted(() => window.removeEventListener('keydown', onKeydown))

const plural = (n, word) => `${n} ${word}${n === 1 ? '' : 's'}`
</script>

<template>
  <InputPanel :busy="busy" :error="error && !board ? error : ''" :hand="input" @analyse="load" />

  <p v-if="!board" class="hint">
    Paste a PBN board, a LIN record or a BBO handviewer URL anywhere on the page, or drop a file.
    Or press <strong>Try an example</strong> for a real BBO hand where declarer claimed after 41 cards.
  </p>

  <template v-if="board">
    <section class="card setup">
      <label v-if="boards.length > 1">
        Board
        <select v-model.number="boardIndex" @change="openBoard()">
          <option v-for="(b, i) in boards" :key="i" :value="i">{{ b.label || `Board ${i + 1}` }}</option>
        </select>
      </label>
      <label>
        Strain
        <select v-model="strain" @change="changeContract">
          <option v-for="s in STRAINS" :key="s.value" :value="s.value">{{ s.label }}</option>
        </select>
      </label>
      <label>
        Declarer
        <select v-model="declarer" @change="changeContract">
          <option v-for="s in SEATS" :key="s" :value="s">{{ seatName(s) }}</option>
        </select>
      </label>
      <span v-if="board.contract" class="chip">Played in {{ board.contract }} by {{ seatName(board.declarer) }}</span>
      <span v-if="board.claim != null" class="chip">
        BBO records a claim after {{ plural(board.plays.length, 'card') }}: declarer ends with
        {{ board.claim }}
      </span>
    </section>

    <section class="card position">
      <div class="stepper">
        <button type="button" :disabled="at === 0" title="Start of the hand" @click="moveTo(0)">⏮</button>
        <button type="button" :disabled="at === 0" title="Back a trick (↑)" @click="moveTo(at - 4)">◀◀</button>
        <button type="button" :disabled="at === 0" title="Back a card (←)" @click="moveTo(at - 1)">◀</button>
        <input
          v-model.number="at"
          type="range"
          min="0"
          :max="plays.length"
          :disabled="!plays.length"
          aria-label="Cards played"
        />
        <button type="button" :disabled="at >= plays.length" title="Forward a card (→)" @click="moveTo(at + 1)">▶</button>
        <button type="button" :disabled="at >= plays.length" title="Forward a trick (↓)" @click="moveTo(at + 4)">▶▶</button>
        <button type="button" :disabled="at >= plays.length" title="End of the line" @click="moveTo(plays.length)">⏭</button>
      </div>
      <p class="where">
        <strong>{{ describePosition(at) }}</strong>
        <template v-if="position">
          <template v-if="position.trick"> · on the table: {{ trickText(position.trick) }}</template>
          · North-South {{ position.taken.NS }}, East-West {{ position.taken.EW }}
          <template v-if="!position.over"> · {{ seatName(position.nextSeat) }} to play</template>
        </template>
        <template v-if="offRecord">
          · your own line
          <button type="button" class="link" @click="backToRecord">back to the record</button>
        </template>
      </p>
      <p class="sub">
        Click a highlighted card to play it and go on from here; the arrow keys step through the
        line.
      </p>
    </section>

    <div class="layout">
      <BridgeTable
        class="table"
        :hands="position.hands"
        :names="board.names"
        :trick="position.trick"
        :tricks-taken="position.taken"
        :active-seat="position.nextSeat"
        :declarer="declarer"
        :south-seat="declarer"
        :card-badges="badges"
        :inspectable="true"
        @card-click="onCardClick"
      />

      <section class="card claim" aria-labelledby="claim-heading">
        <h2 id="claim-heading">Claim</h2>
        <p v-if="position.over" class="sub">The hand is over; go back to claim.</p>
        <template v-else>
          <div class="form">
            <label>
              Claimer
              <select v-model="claimer">
                <option v-for="s in claimers" :key="s" :value="s">
                  {{ seatName(s) }}{{ s === declarer ? ' (declarer)' : '' }}
                </option>
              </select>
            </label>
            <label>
              Tricks for their side, in all
              <input v-model.number="tricks" type="number" :min="won" :max="won + left" />
            </label>
            <button type="button" class="primary" :disabled="checking" @click="check">
              {{ checking ? 'Checking…' : 'Check the claim' }}
            </button>
          </div>
          <p class="sub">
            {{ seatName(claimer) }}'s side has {{ plural(won, 'trick') }} and {{ left }} still to play,
            so the claim is for {{ plural(Math.max(0, tricks - won), 'more trick') }}.
          </p>

          <div v-if="result" class="verdict" :class="result.tone" role="status">
            <h3>{{ result.title }}</h3>
            <p>{{ result.text }}</p>
            <p class="sub">
              {{ result.nodes.toLocaleString() }} positions searched in {{ Math.round(result.ms) }} ms.
            </p>
          </div>

          <h3>The most each player could claim</h3>
          <button type="button" :disabled="checking" @click="findMaxima">Work it out</button>
          <table v-if="maxima" class="maxima">
            <thead>
              <tr>
                <th>Claimer</th>
                <th class="n">Sure of</th>
                <th class="n">ms</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="m in maxima" :key="m.seat">
                <td>{{ seatName(m.seat) }}{{ m.seat === declarer ? ' (declarer)' : '' }}</td>
                <td class="n">
                  {{ m.guaranteed }}{{ m.undecided ? ' or more' : '' }}
                </td>
                <td class="n">{{ Math.round(m.ms) }}</td>
              </tr>
            </tbody>
          </table>
          <p v-if="maxima" class="sub">
            Totals for the claimer's side over the whole hand. "Or more" means the search ran out of
            budget before ruling out one more trick.
          </p>
        </template>
        <p v-if="error && board" class="bad">{{ error }}</p>
      </section>
    </div>
  </template>
</template>

<style scoped>
.hint {
  color: var(--text-secondary);
  margin: 16px 0;
}

.card {
  background: var(--bg-white);
  border: 1px solid var(--border);
  border-radius: var(--radius-card);
  padding: 12px 16px;
  margin: 12px 0;
}

.setup {
  display: flex;
  flex-wrap: wrap;
  gap: 8px 16px;
  align-items: center;
}

.chip {
  background: var(--surface-alt);
  border-radius: 999px;
  padding: 2px 10px;
  font-size: 0.92em;
}

.stepper {
  display: flex;
  gap: 6px;
  align-items: center;
}

.stepper input[type='range'] {
  flex: 1;
  min-width: 0;
}

.where {
  margin: 8px 0 0;
}

.layout {
  display: grid;
  grid-template-columns: minmax(0, 1fr) minmax(280px, 360px);
  gap: 16px;
  align-items: start;
}

@media (max-width: 860px) {
  .layout {
    grid-template-columns: minmax(0, 1fr);
  }
}

.claim h2 {
  margin: 0 0 8px;
  font-size: 1.15em;
}

.claim h3 {
  margin: 16px 0 6px;
  font-size: 1em;
}

.form {
  display: flex;
  flex-direction: column;
  gap: 8px;
}

label {
  display: flex;
  flex-direction: column;
  gap: 2px;
  font-weight: 600;
  font-size: 0.92em;
}

.setup label {
  flex-direction: row;
  align-items: center;
  gap: 6px;
}

select,
input[type='number'],
button {
  font: inherit;
  font-weight: normal;
}

select,
input[type='number'] {
  padding: 4px 6px;
  border: 1px solid var(--border);
  border-radius: var(--radius-button);
  background: var(--bg-white);
}

button {
  padding: 6px 12px;
  border-radius: var(--radius-button);
  border: 1px solid var(--border);
  background: var(--bg-white);
  cursor: pointer;
}

button:disabled {
  opacity: 0.5;
  cursor: default;
}

button.primary {
  background: var(--green);
  border-color: var(--green);
  color: #fff;
}

button.primary:hover:not(:disabled) {
  background: var(--green-hover);
}

button.link {
  border: 0;
  background: none;
  padding: 0;
  color: var(--focus-blue-strong);
  text-decoration: underline;
}

.verdict {
  margin-top: 12px;
  padding: 10px 12px;
  border-radius: var(--radius-button);
  border: 1px solid var(--border);
}

.verdict h3 {
  margin: 0 0 4px;
}

.verdict p {
  margin: 4px 0;
}

.verdict.good {
  background: var(--green-pale);
  border-color: var(--green);
}

.verdict.bad {
  background: var(--cost-mild);
  border-color: var(--cost-severe);
}

.verdict.warn {
  background: var(--surface-alt);
}

.maxima {
  border-collapse: collapse;
  width: 100%;
  margin-top: 8px;
}

.maxima th,
.maxima td {
  padding: 3px 6px;
  border-bottom: 1px solid var(--rule);
  text-align: left;
}

.maxima .n {
  text-align: right;
  font-variant-numeric: tabular-nums;
}

.sub {
  color: var(--text-secondary);
  font-size: 0.9em;
  margin: 6px 0 0;
}

.bad {
  color: var(--cost-ink);
}
</style>
