import { readFileSync } from 'node:fs'
import { fileURLToPath, URL } from 'node:url'
import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

const entry = (name) => fileURLToPath(new URL(name, import.meta.url))

// The solver's version, from the crate rather than web/package.json, whose
// version is never bumped (it said 1.0.0 through every release). The page and
// its wasm module are built from the same commit, so the crate's version is
// the one the page is running. Read with a pattern rather than a TOML parser:
// it is the first `version =` line, inside [package].
const crateToml = readFileSync(new URL('../Cargo.toml', import.meta.url), 'utf8')
const crateVersion = crateToml.match(/^\[package\][^[]*?^version\s*=\s*"([^"]+)"/m)?.[1]
if (!crateVersion) throw new Error('no [package] version in ../Cargo.toml')

export default defineConfig({
  plugins: [vue()],

  // The build's own version, shown in the page footer and sent with telemetry so
  // a record can say which build produced it — otherwise a regression looks
  // identical to a slow device. Inlined at build time.
  define: {
    __APP_VERSION__: JSON.stringify(crateVersion),
  },

  // Served from https://<org>.github.io/bridge-solver/, so assets must be
  // referenced relatively rather than from the domain root.
  base: './',

  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src', import.meta.url)),
    },
  },

  build: {
    outDir: 'dist',
    // Four pages: the app, a gallery that embeds it at each viewport, the claim
    // checker and the benchmarks. The gallery imports the shared example module
    // and the others the engine, so each has to be an entry rather than a static
    // file copied past the bundler.
    rollupOptions: {
      input: {
        main: entry('index.html'),
        gallery: entry('gallery.html'),
        bench: entry('bench.html'),
        claim: entry('claim.html'),
      },
    },
    // The wasm module is a megabyte-ish on its own; the default 500 kB warning
    // is noise here rather than a signal.
    chunkSizeWarningLimit: 2000,
    target: 'es2022',
  },

  worker: {
    format: 'es',
  },

  // `wasm-pack --target web` output loads the binary with
  // `new URL('..._bg.wasm', import.meta.url)`, which Vite rewrites to a hashed
  // asset. Excluding it from dep optimisation keeps that URL intact in dev.
  optimizeDeps: {
    exclude: ['@/wasm/bridge_solver_wasm.js'],
  },

  test: {
    environment: 'node',
    include: ['src/**/*.test.js'],
  },
})
