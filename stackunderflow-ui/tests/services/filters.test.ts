// Run with: node --test tests/services/filters.test.ts
// (Node 22+ strips TypeScript types automatically; matches the runner used
// by tests/services/format.test.ts so the project stays test-runner-free.)

import { test } from 'node:test'
import assert from 'node:assert/strict'
import {
  normalize,
  buildQueryString,
  readFromURL,
  writeToURL,
} from '../../src/services/filterUrl.ts'

// ---------------------------------------------------------------------------
// Minimal `window` stub so the URL-sync helpers run under node --test.
// We replace the real `window` with a hand-rolled object that exposes the
// `location.href`, `history.replaceState`, and `URLSearchParams` surface
// the implementation actually touches.
// ---------------------------------------------------------------------------

interface WindowStub {
  location: { href: string; pathname: string; search: string; hash: string }
  history: { replaceState: (state: unknown, t: string, url?: string) => void }
}

function installWindow(href: string): WindowStub {
  const url = new URL(href)
  const w: WindowStub = {
    location: {
      href: url.href,
      pathname: url.pathname,
      search: url.search,
      hash: url.hash,
    },
    history: {
      replaceState: (_state: unknown, _title: string, nextUrl?: string) => {
        if (typeof nextUrl !== 'string') return
        const u = new URL(nextUrl, url.origin)
        w.location.pathname = u.pathname
        w.location.search = u.search
        w.location.hash = u.hash
        w.location.href = u.href
      },
    },
  }
  ;(globalThis as unknown as { window: WindowStub }).window = w
  return w
}

function uninstallWindow(): void {
  delete (globalThis as unknown as { window?: WindowStub }).window
}

// ---------------------------------------------------------------------------
// normalize()
// ---------------------------------------------------------------------------

test('normalize lowercases, trims, and dedupes', () => {
  const out = normalize(['Cursor', 'cursor', '  CLINE ', '', 'cursor'])
  assert.deepEqual(out, ['cursor', 'cline'])
})

test('normalize ignores non-strings (defensive)', () => {
  const out = normalize(['claude', '', '   '])
  assert.deepEqual(out, ['claude'])
})

// ---------------------------------------------------------------------------
// buildQueryString()
// ---------------------------------------------------------------------------

test('buildQueryString empty filters → empty string', () => {
  assert.equal(buildQueryString({ providers: [], models: [] }), '')
})

test('buildQueryString providers only → leading-amp fragment', () => {
  const qs = buildQueryString({ providers: ['cursor', 'cline'], models: [] })
  assert.equal(qs, '&provider=cursor&provider=cline')
})

test('buildQueryString providers + models → both encoded', () => {
  const qs = buildQueryString({ providers: ['cursor'], models: ['opus-4-7', 'sonnet-4-6'] })
  assert.equal(qs, '&provider=cursor&model=opus-4-7&model=sonnet-4-6')
})

// ---------------------------------------------------------------------------
// readFromURL / writeToURL round-trip
// ---------------------------------------------------------------------------

test('readFromURL: empty when no params', () => {
  installWindow('https://example.com/project/foo')
  try {
    const out = readFromURL()
    assert.deepEqual(out, { providers: [], models: [] })
  } finally {
    uninstallWindow()
  }
})

test('readFromURL: parses repeated provider + model', () => {
  installWindow('https://example.com/project/foo?provider=cursor&provider=cline&model=opus-4-7')
  try {
    const out = readFromURL()
    assert.deepEqual(out.providers, ['cursor', 'cline'])
    assert.deepEqual(out.models, ['opus-4-7'])
  } finally {
    uninstallWindow()
  }
})

test('readFromURL is case-insensitive: ?provider=Cursor → ["cursor"]', () => {
  installWindow('https://example.com/project/foo?provider=Cursor&provider=CLINE')
  try {
    const out = readFromURL()
    assert.deepEqual(out.providers, ['cursor', 'cline'])
  } finally {
    uninstallWindow()
  }
})

test('readFromURL: SSR / no window → empty result', () => {
  // No installWindow — `hasWindow()` should short-circuit.
  uninstallWindow()
  const out = readFromURL()
  assert.deepEqual(out, { providers: [], models: [] })
})

test('writeToURL → readFromURL round-trip preserves the active set', () => {
  const w = installWindow('https://example.com/project/foo?tab=cost')
  try {
    writeToURL({ providers: ['cursor', 'cline'], models: ['opus-4-7'] })
    // Round-trip: reading the URL we just wrote should reproduce the input.
    const out = readFromURL()
    assert.deepEqual(out.providers, ['cursor', 'cline'])
    assert.deepEqual(out.models, ['opus-4-7'])
    // Other params (`tab=cost`) must survive a write.
    assert.ok(w.location.search.includes('tab=cost'))
  } finally {
    uninstallWindow()
  }
})

test('writeToURL clears stale provider/model params', () => {
  const w = installWindow('https://example.com/?provider=cursor&model=opus-4-7&tab=cost')
  try {
    writeToURL({ providers: [], models: [] })
    assert.equal(w.location.search.includes('provider'), false)
    assert.equal(w.location.search.includes('model'), false)
    // Unrelated params (tab) must remain.
    assert.ok(w.location.search.includes('tab=cost'))
  } finally {
    uninstallWindow()
  }
})

test('writeToURL is no-op when nothing changes (no extra history entry)', () => {
  const w = installWindow('https://example.com/?provider=cursor')
  try {
    let calls = 0
    const original = w.history.replaceState
    w.history.replaceState = (s, t, u) => {
      calls++
      original.call(w.history, s, t, u)
    }
    // Same set as the URL → writeToURL early-returns without calling replaceState.
    writeToURL({ providers: ['cursor'], models: [] })
    assert.equal(calls, 0)
  } finally {
    uninstallWindow()
  }
})
