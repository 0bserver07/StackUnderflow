// Run with: node --test tests/services/live.test.ts
//
// Spec 13 — coverage for `services/live.ts`:
//
//   * `getLiveStats()` parses the snapshot shape and surfaces fetch errors.
//   * `connectLiveStream()` wires up the four typed event handlers and
//     dispatches one callback per SSE message.
//   * Malformed JSON in a payload is dropped silently rather than crashing.
//   * `close()` is idempotent and tears the EventSource down.
//
// Mirrors the test pattern in tests/services/etl-status.test.ts
// (fetch stub) and tests/services/meta-agent.test.ts (event-stream
// helper). No DOM runner — we stub `EventSource` with a hand-rolled
// emitter that mirrors the shape `connectLiveStream` consumes.

import { test } from 'node:test'
import assert from 'node:assert/strict'

import { connectLiveStream, getLiveStats } from '../../src/services/live.ts'
import type {
  LiveBurnSnapshot,
  LiveEventRowPayload,
  LiveStatsResponse,
  LiveToolCallRowPayload,
} from '../../src/types/api.ts'

// ---------------------------------------------------------------------------
// fetch stub
// ---------------------------------------------------------------------------

interface MockResponse {
  ok: boolean
  status: number
  statusText: string
  json: () => Promise<unknown>
  text: () => Promise<string>
}

function mockJson(body: unknown, status = 200): MockResponse {
  return {
    ok: status >= 200 && status < 300,
    status,
    statusText: status === 200 ? 'OK' : 'Error',
    json: async () => body,
    text: async () => JSON.stringify(body),
  }
}

function withFetch(
  impl: (input: string, init?: RequestInit) => Promise<MockResponse>,
): () => void {
  const original = (globalThis as { fetch?: unknown }).fetch
  ;(globalThis as { fetch: unknown }).fetch = impl as unknown as typeof fetch
  return () => {
    ;(globalThis as { fetch: unknown }).fetch = original as typeof fetch
  }
}

// ---------------------------------------------------------------------------
// EventSource stub. Hand-rolled so each test can synthesise the named SSE
// events the route emits. Only the surface `connectLiveStream` actually
// uses (`addEventListener`, `close`) is implemented; everything else
// remains undefined, which would throw if we accidentally relied on it.
// ---------------------------------------------------------------------------

class MockEventSource {
  static instances: MockEventSource[] = []
  url: string
  closed = false
  // listeners: keyed on event name, holds the handler the service registers.
  listeners: Map<string, Array<(ev: MessageEvent<string>) => void>> = new Map()

  constructor(url: string) {
    this.url = url
    MockEventSource.instances.push(this)
  }

  addEventListener(name: string, cb: (ev: MessageEvent<string>) => void): void {
    const list = this.listeners.get(name) ?? []
    list.push(cb)
    this.listeners.set(name, list)
  }

  close(): void {
    this.closed = true
  }

  /** Dispatch an SSE-formatted message to listeners on `name`. */
  emit(name: string, payload: unknown, ts: string = '2026-05-15T12:00:00Z'): void {
    const data = JSON.stringify({ type: name, ts, payload })
    const ev = { data } as MessageEvent<string>
    for (const cb of this.listeners.get(name) ?? []) cb(ev)
  }

  /** Dispatch a malformed-JSON payload — verifies the parser swallows it. */
  emitRaw(name: string, raw: string): void {
    const ev = { data: raw } as MessageEvent<string>
    for (const cb of this.listeners.get(name) ?? []) cb(ev)
  }
}

function withEventSource(): () => void {
  const original = (globalThis as { EventSource?: unknown }).EventSource
  ;(globalThis as { EventSource: unknown }).EventSource =
    MockEventSource as unknown as typeof EventSource
  MockEventSource.instances = []
  return () => {
    ;(globalThis as { EventSource: unknown }).EventSource = original as typeof EventSource
  }
}

// ---------------------------------------------------------------------------
// Sample payloads
// ---------------------------------------------------------------------------

const sampleBurn: LiveBurnSnapshot = {
  window_minutes: 5,
  window_cost: 0.42,
  per_minute: 0.084,
  per_hour: 5.04,
  today_cost: 12.34,
  month_to_date: 100.0,
  projected_month_end: 250.0,
  ts: '2026-05-15T12:00:00Z',
}

const sampleStats: LiveStatsResponse = {
  burn: sampleBurn,
  tool_latency: [
    { tool_name: 'Read', samples: 50, p50: 1.0, p95: 2.5, p99: 4.0 },
    { tool_name: 'Bash', samples: 20, p50: 0.5, p95: 1.0, p99: 1.5 },
  ],
  watermarks: { event_id: 1234, tool_call_id: 5678 },
  watcher: { running: true },
}

const sampleEventRow: LiveEventRowPayload = {
  id: 1235,
  ts: '2026-05-15T12:00:01Z',
  project_id: 1,
  session_id: 'sess-a',
  model: 'claude-sonnet-4-5',
  cost_usd: 0.05,
  input_tokens: 100,
  output_tokens: 50,
  cache_read_tokens: 0,
  cache_create_tokens: 0,
  cost_source: 'rate_card',
  project_slug: '-alpha',
  project_name: 'alpha',
}

const sampleToolCallRow: LiveToolCallRowPayload = {
  id: 5679,
  ts: '2026-05-15T12:00:02Z',
  project_id: 1,
  session_id: 'sess-a',
  tool_name: 'Read',
  file_path: '/tmp/foo.py',
  byte_count: 1024,
  call_index: 0,
  project_slug: '-alpha',
  project_name: 'alpha',
}

// ---------------------------------------------------------------------------
// getLiveStats
// ---------------------------------------------------------------------------

test('getLiveStats parses the snapshot shape', async () => {
  const restore = withFetch(async (url) => {
    assert.equal(url, '/api/live/stats')
    return mockJson(sampleStats)
  })
  try {
    const data = await getLiveStats()
    assert.equal(data.burn.window_cost, 0.42)
    assert.equal(data.tool_latency.length, 2)
    assert.equal(data.tool_latency[0]!.tool_name, 'Read')
    assert.equal(data.watermarks.event_id, 1234)
    assert.equal(data.watcher.running, true)
  } finally {
    restore()
  }
})

test('getLiveStats raises a generic Error on 500', async () => {
  const restore = withFetch(async () => mockJson('boom', 500))
  try {
    await assert.rejects(getLiveStats, (err) => {
      assert.ok(err instanceof Error)
      assert.match(err.message, /500/)
      return true
    })
  } finally {
    restore()
  }
})

// ---------------------------------------------------------------------------
// connectLiveStream — typed event dispatch
// ---------------------------------------------------------------------------

test('connectLiveStream dispatches onReady on the seed event', () => {
  const restore = withEventSource()
  try {
    let readyPayload: { burn_interval_seconds: number } | null = null
    const handle = connectLiveStream({
      onReady: (p) => {
        readyPayload = p as { burn_interval_seconds: number }
      },
    })
    const src = MockEventSource.instances[0]!
    src.emit('ready', {
      watermarks: { event_id: 0, tool_call_id: 0 },
      watcher: { running: 'unknown' },
      burn_interval_seconds: 5,
    })
    assert.ok(readyPayload, 'onReady should have fired')
    assert.equal((readyPayload as { burn_interval_seconds: number }).burn_interval_seconds, 5)
    handle.close()
  } finally {
    restore()
  }
})

test('connectLiveStream dispatches onEvent with the row payload + ts', () => {
  const restore = withEventSource()
  try {
    const captured: Array<{ row: LiveEventRowPayload; ts: string }> = []
    const handle = connectLiveStream({
      onEvent: (row, ts) => captured.push({ row, ts }),
    })
    const src = MockEventSource.instances[0]!
    src.emit('event', sampleEventRow, '2026-05-15T12:00:01Z')
    assert.equal(captured.length, 1)
    assert.equal(captured[0]!.row.cost_usd, 0.05)
    assert.equal(captured[0]!.ts, '2026-05-15T12:00:01Z')
    handle.close()
  } finally {
    restore()
  }
})

test('connectLiveStream dispatches onToolCall with the row payload + ts', () => {
  const restore = withEventSource()
  try {
    const captured: Array<{ row: LiveToolCallRowPayload; ts: string }> = []
    const handle = connectLiveStream({
      onToolCall: (row, ts) => captured.push({ row, ts }),
    })
    const src = MockEventSource.instances[0]!
    src.emit('tool_call', sampleToolCallRow)
    assert.equal(captured.length, 1)
    assert.equal(captured[0]!.row.tool_name, 'Read')
    assert.equal(captured[0]!.row.byte_count, 1024)
    handle.close()
  } finally {
    restore()
  }
})

test('connectLiveStream dispatches onBurnTick with the burn payload + ts', () => {
  const restore = withEventSource()
  try {
    let capturedBurn: LiveBurnSnapshot | null = null
    const handle = connectLiveStream({
      onBurnTick: (burn) => {
        capturedBurn = burn
      },
    })
    const src = MockEventSource.instances[0]!
    src.emit('burn_tick', sampleBurn)
    assert.ok(capturedBurn, 'onBurnTick should have fired')
    assert.equal((capturedBurn as LiveBurnSnapshot).window_cost, 0.42)
    assert.equal((capturedBurn as LiveBurnSnapshot).projected_month_end, 250.0)
    handle.close()
  } finally {
    restore()
  }
})

test('connectLiveStream silently drops a malformed JSON payload', () => {
  const restore = withEventSource()
  try {
    let calls = 0
    const handle = connectLiveStream({
      onBurnTick: () => {
        calls += 1
      },
    })
    const src = MockEventSource.instances[0]!
    // No listener should crash, no callback should fire.
    src.emitRaw('burn_tick', '{not json')
    assert.equal(calls, 0)
    handle.close()
  } finally {
    restore()
  }
})

test('connectLiveStream forwards onError to the EventSource error event', () => {
  const restore = withEventSource()
  try {
    let errorEvent: Event | null = null
    const handle = connectLiveStream({
      onError: (ev) => {
        errorEvent = ev
      },
    })
    const src = MockEventSource.instances[0]!
    // Emit a synthetic error via the same listener machinery — the
    // service registers `onError` on the 'error' event.
    const cb = src.listeners.get('error')?.[0]
    assert.ok(cb, 'onError should be registered')
    cb!(new Event('error') as unknown as MessageEvent<string>)
    assert.ok(errorEvent, 'onError should have fired')
    handle.close()
  } finally {
    restore()
  }
})

test('connectLiveStream close() is idempotent and shuts the source down', () => {
  const restore = withEventSource()
  try {
    const handle = connectLiveStream({})
    const src = MockEventSource.instances[0]!
    assert.equal(src.closed, false)
    handle.close()
    assert.equal(src.closed, true)
    // Second close should be a no-op (no errors / re-close).
    handle.close()
    assert.equal(src.closed, true)
  } finally {
    restore()
  }
})

test('connectLiveStream targets /api/live/stream by default', () => {
  const restore = withEventSource()
  try {
    const handle = connectLiveStream({})
    const src = MockEventSource.instances[0]!
    assert.equal(src.url, '/api/live/stream')
    handle.close()
  } finally {
    restore()
  }
})
