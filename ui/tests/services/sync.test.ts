// Run with: node --test tests/services/sync.test.ts
// (Node 22+ strips TypeScript types automatically; matches the runner used by
// tests/services/format.test.ts and tests/services/etl-status.test.ts.)
//
// Coverage for the multi-device sync client (#100 Phase 2). The DevicesTab
// component isn't render-tested (no DOM runner in this project); here we lock
// the endpoint URLs, the scope query param, and the data-shape contract of both
// `/api/sync/status` and `/api/sync/overview` (stub vs. merged discriminant).

import { test } from 'node:test'
import assert from 'node:assert/strict'

import { getSyncOverview, getSyncStatus } from '../../src/services/sync.ts'
import type { SyncOverview, SyncStatus } from '../../src/types/api.ts'

// ---------------------------------------------------------------------------
// Minimal `fetch` stub — swap globalThis.fetch per test, restore afterwards.
// Same pattern as tests/services/etl-status.test.ts (no msw / fetch-mock dep).
// ---------------------------------------------------------------------------

interface MockResponse {
  ok: boolean
  status: number
  statusText: string
  json: () => Promise<unknown>
  text: () => Promise<string>
}

function mockResponse(body: unknown, status = 200): MockResponse {
  return {
    ok: status >= 200 && status < 300,
    status,
    statusText: status === 200 ? 'OK' : status === 404 ? 'Not Found' : 'Error',
    json: async () => body,
    text: async () => (typeof body === 'string' ? body : JSON.stringify(body)),
  }
}

function withFetch(impl: (input: string, init?: RequestInit) => Promise<MockResponse>): () => void {
  const original = (globalThis as { fetch?: unknown }).fetch
  ;(globalThis as { fetch: unknown }).fetch = impl as unknown as typeof fetch
  return () => {
    ;(globalThis as { fetch: unknown }).fetch = original as typeof fetch
  }
}

// ---------------------------------------------------------------------------
// Sample payloads mirroring stackunderflow/routes/sync.py.
// ---------------------------------------------------------------------------

const configuredStatus: SyncStatus = {
  enabled: true,
  device_uuid: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
  fingerprint: 'SHA256:abcd',
  bucket_url: 's3://my-bucket',
  endpoint_url: null,
  shard_count: 4,
  pending: ['daily/2026-07'],
  pending_count: 1,
  last_push_ts: '2026-07-02T10:00:00+00:00',
  peers: [
    {
      remote_device_uuid: 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb',
      alias: 'laptop',
      key_fingerprint: 'SHA256:abcd',
      first_seen: '2026-07-01T09:00:00+00:00',
      last_seen: '2026-07-02T09:30:00+00:00',
      last_generation: 3,
    },
  ],
  peer_count: 1,
  remote_rows: 42,
  all_devices_available: true,
  scanned_at: '2026-07-02T12:00:00+00:00',
}

const disabledStatus: SyncStatus = {
  enabled: false,
  device_uuid: null,
  fingerprint: null,
  bucket_url: null,
  endpoint_url: null,
  shard_count: 0,
  pending: [],
  pending_count: 0,
  last_push_ts: null,
  peers: [],
  peer_count: 0,
  remote_rows: 0,
  all_devices_available: false,
  scanned_at: '2026-07-02T12:00:00+00:00',
}

const stubOverview: SyncOverview = {
  scope: 'this-device',
  merged: false,
  sync_enabled: true,
  hint: 'pass ?scope=all-devices to union pulled peers',
}

const mergedOverview: SyncOverview = {
  scope: 'all-devices',
  merged: true,
  sync_enabled: true,
  totals: {
    cost_usd: 123.45,
    input_tokens: 1000,
    output_tokens: 2000,
    cache_read: 500,
    cache_create: 250,
    message_count: 88,
    session_count: 12,
  },
  by_day: [
    { day: '2026-07-01', cost_usd: 60.0, input_tokens: 400, output_tokens: 800, message_count: 40 },
    { day: '2026-07-02', cost_usd: 63.45, input_tokens: 600, output_tokens: 1200, message_count: 48 },
  ],
  by_project: [
    {
      provider: 'claude',
      slug: 'my-app',
      display_name: 'My App',
      first_ts: '2026-06-01T00:00:00+00:00',
      last_ts: '2026-07-02T00:00:00+00:00',
      total_messages: 88,
      total_sessions: 12,
      total_input_tokens: 1000,
      total_output_tokens: 2000,
      total_cache_read: 500,
      total_cache_create: 250,
      total_cost_usd: 123.45,
    },
  ],
  by_provider_day: [
    { day: '2026-07-02', provider: 'claude', cost_usd: 63.45, message_count: 48, session_count: 6, project_count: 1 },
  ],
  devices: [
    { device_uuid: '(local)', alias: null, is_local: true, projects: 1, cost_usd: 80.0 },
    { device_uuid: 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', alias: 'laptop', is_local: false, projects: 1, cost_usd: 43.45 },
  ],
  merge_warnings: 2,
  currency: { code: 'USD', symbol: '$', rate_from_usd: 1.0, warning: null },
  generated_at: '2026-07-02T12:00:01+00:00',
}

// ---------------------------------------------------------------------------
// getSyncStatus
// ---------------------------------------------------------------------------

test('getSyncStatus GETs /api/sync/status and parses the configured shape', async () => {
  const restore = withFetch(async (url) => {
    assert.equal(url, '/api/sync/status')
    return mockResponse(configuredStatus)
  })
  try {
    const data = await getSyncStatus()
    assert.equal(data.enabled, true)
    assert.equal(data.all_devices_available, true)
    assert.equal(data.peer_count, 1)
    assert.equal(data.remote_rows, 42)
    assert.equal(data.peers[0]!.alias, 'laptop')
    assert.equal(data.peers[0]!.last_seen, '2026-07-02T09:30:00+00:00')
  } finally {
    restore()
  }
})

test('getSyncStatus round-trips the sync-off shape (nulls + empties)', async () => {
  const restore = withFetch(async () => mockResponse(disabledStatus))
  try {
    const data = await getSyncStatus()
    assert.equal(data.enabled, false)
    assert.equal(data.all_devices_available, false)
    assert.equal(data.device_uuid, null)
    assert.deepEqual(data.peers, [])
  } finally {
    restore()
  }
})

test('getSyncStatus throws a status-bearing Error on a non-ok response', async () => {
  const restore = withFetch(async () => mockResponse('boom', 500))
  try {
    await assert.rejects(getSyncStatus, (err) => {
      assert.ok(err instanceof Error)
      assert.match(err.message, /500/)
      return true
    })
  } finally {
    restore()
  }
})

// ---------------------------------------------------------------------------
// getSyncOverview — scope query param + the merged/stub discriminant.
// ---------------------------------------------------------------------------

test('getSyncOverview defaults to scope=this-device and parses the stub', async () => {
  const restore = withFetch(async (url) => {
    assert.equal(url, '/api/sync/overview?scope=this-device')
    return mockResponse(stubOverview)
  })
  try {
    const data = await getSyncOverview()
    assert.equal(data.merged, false)
    assert.equal(data.scope, 'this-device')
    // Narrowing on the discriminant: the stub has no `totals`.
    if (!data.merged) {
      assert.equal(data.hint, 'pass ?scope=all-devices to union pulled peers')
    } else {
      assert.fail('stub should narrow to merged: false')
    }
  } finally {
    restore()
  }
})

test('getSyncOverview passes scope=all-devices and parses the merged roll-up', async () => {
  const restore = withFetch(async (url) => {
    assert.equal(url, '/api/sync/overview?scope=all-devices')
    return mockResponse(mergedOverview)
  })
  try {
    const data = await getSyncOverview('all-devices')
    assert.equal(data.merged, true)
    // Narrowing on the discriminant exposes the merged fields.
    if (data.merged) {
      assert.equal(data.totals.cost_usd, 123.45)
      assert.equal(data.totals.session_count, 12)
      assert.equal(data.by_day.length, 2)
      assert.equal(data.by_project[0]!.display_name, 'My App')
      assert.equal(data.devices[0]!.is_local, true)
      assert.equal(data.merge_warnings, 2)
      assert.equal(data.currency.symbol, '$')
    } else {
      assert.fail('merged payload should narrow to merged: true')
    }
  } finally {
    restore()
  }
})

test('getSyncOverview surfaces a non-ok response as an Error', async () => {
  const restore = withFetch(async () => mockResponse('nope', 503))
  try {
    await assert.rejects(() => getSyncOverview('all-devices'), (err) => {
      assert.ok(err instanceof Error)
      assert.match(err.message, /503/)
      return true
    })
  } finally {
    restore()
  }
})
