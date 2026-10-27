// Which suite is running, so the other waits: two suites timed at once would
// share the device and both report it slower than it is.

let holder = null
const listeners = new Set()

export const lock = {
  /** The running suite's name, or `null`. */
  get holder() {
    return holder
  },
  take(name) {
    holder = name
    listeners.forEach((f) => f())
  },
  release(name) {
    if (holder !== name) return
    holder = null
    listeners.forEach((f) => f())
  },
  /** Call `f` whenever the holder changes. */
  subscribe(f) {
    listeners.add(f)
  },
}
