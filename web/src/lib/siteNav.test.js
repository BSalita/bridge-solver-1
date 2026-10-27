// Every page repeats the navigation bar's markup, because two of them are
// copied into the build verbatim and cannot share a component. This keeps the
// copies saying the same thing: the same links in the same order, back to
// Bridge Craftwork, with the page itself marked as current.

import { readFileSync } from 'node:fs'
import { describe, expect, it } from 'vitest'

const web = new URL('../../', import.meta.url)
const read = (path) => readFileSync(new URL(path, web), 'utf8')

const LINKS = ['./', './claim.html', './bench.html', './get.html', './stats.html']

// Page file, and the link that should be marked current on it (null: none).
const PAGES = [
  ['src/App.vue', './'],
  ['claim.html', './claim.html'],
  ['bench.html', './bench.html'],
  ['public/get.html', './get.html'],
  ['public/stats.html', './stats.html'],
  ['public/privacy.html', null],
]

function navOf(text) {
  const match = text.match(/<nav\b[^>]*class="site-nav"[\s\S]*?<\/nav>/)
  return match ? match[0] : null
}

describe('the shared navigation bar', () => {
  for (const [path, current] of PAGES) {
    it(`is the same on ${path}`, () => {
      const nav = navOf(read(path))
      expect(nav, 'no <nav class="site-nav">').not.toBeNull()
      expect(nav).toContain('href="https://bridge-craftwork.com/"')
      const items = [...nav.matchAll(/<li><a href="([^"]+)"/g)].map((m) => m[1])
      expect(items).toEqual(LINKS)
      const marked = [...nav.matchAll(/<a href="([^"]+)" aria-current="page"/g)].map((m) => m[1])
      expect(marked).toEqual(current ? [current] : [])
    })
  }

  it('is hidden when the app is embedded in another site', () => {
    expect(navOf(read('src/App.vue'))).toMatch(/^<nav v-if="!embed"/)
  })
})
