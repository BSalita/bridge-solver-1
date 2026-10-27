import { readFileSync } from 'node:fs'
import { fileURLToPath, URL } from 'node:url'
import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

const entry = (name) => fileURLToPath(new URL(name, import.meta.url))

// Read rather than imported: a JSON import needs an assertion whose syntax has
// moved twice, and this config is the one place that already does file IO.
const pkg = JSON.parse(readFileSync(new URL('./package.json', import.meta.url), 'utf8'))

export default defineConfig({
  plugins: [vue()],

  // The build's own version, so a telemetry record can say which build produced
  // it — otherwise a regression looks identical to a slow device. Inlined at
  // build time, so nothing reads package.json at runtime.
  define: {
    __APP_VERSION__: JSON.stringify(pkg.version),
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
    // Three pages: the app, a gallery that embeds it at each viewport, and the
    // claim checker benchmark. The gallery imports the shared example module and
    // the benchmark the engine, so each has to be an entry rather than a static
    // file copied past the bundler.
    rollupOptions: {
      input: {
        main: entry('index.html'),
        gallery: entry('gallery.html'),
        claimBench: entry('claim-bench.html'),
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
