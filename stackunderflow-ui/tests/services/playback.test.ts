// Run with: node --test tests/services/playback.test.ts
// Locks the Playback tab's API client + URL-state contract.
//
// Spec: .notes/specs/10-playback-timeline.md

import { test } from 'node:test'
import assert from 'node:assert/strict'

import {
  getPlayback,
  getProjectTimeline,
  readPlaybackSelection,
  writePlaybackSelection,
} from '../../src/services/api.ts'
import type { PlaybackResponse, ProjectTimelineResponse } from '../../src/types/api.ts'

// ---------------------------------------------------------------------------
// fetch stub (matches the agent-teams / etl-status test pattern).
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
    statusText: status === 200 ? 'OK' : 'Error',
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
// Sample payloads.
// ---------------------------------------------------------------------------

const sampleEvent = {
  seq: 0,
  ts: '2026-05-01T00:00:01Z',
  message_id: 42,
  tool_name: 'Edit',
  summary: 'Edit routes/cost.py',
  target_path: 'routes/cost.py',
  byte_count: 128,
  success: false,
  duration_ms: 1500,
  payload_excerpt: "- 'foo'\n+ 'baz'",
  session_id: 'sess-1',
}

const sampleSessionBody: PlaybackResponse = {
  session_id: 'sess-1',
  events: [sampleEvent],
  total: 1,
  truncated: false,
}

const sampleProjectBody: ProjectTimelineResponse = {
  project_slug: 'demo',
  events: [{ ...sampleEvent, payload_excerpt: '' }],
  total: 1,
  truncated: true,
}

// ---------------------------------------------------------------------------
// API client — getPlayback
// ---------------------------------------------------------------------------

test('getPlayback hits /api/playback/{id} with no params by default', async () => {
  let captured: string | null = null
  const restore = withFetch(async (url) => {
    captured = url
    return mockResponse(sampleSessionBody)
  })
  try {
    const data = await getPlayback('sess-1')
    assert.equal(captured, '/api/playback/sess-1')
    assert.equal(data.session_id, 'sess-1')
    assert.equal(data.events.length, 1)
    assert.equal(data.events[0]!.tool_name, 'Edit')
    assert.equal(data.events[0]!.success, false)
    assert.equal(data.truncated, false)
  } finally {
    restore()
  }
})

test('getPlayback serialises tool_filter, limit and include_payload', async () => {
  let captured: string | null = null
  const restore = withFetch(async (url) => {
    captured = url
    return mockResponse(sampleSessionBody)
  })
  try {
    await getPlayback('sess-1', { toolFilter: ['Edit', 'Write'], limit: 250, includePayload: false })
    const u = new URL(`http://x${captured}`)
    assert.equal(u.pathname, '/api/playback/sess-1')
    assert.equal(u.searchParams.get('tool_filter'), 'Edit,Write')
    assert.equal(u.searchParams.get('limit'), '250')
    assert.equal(u.searchParams.get('include_payload'), '0')
  } finally {
    restore()
  }
})

test('getPlayback omits include_payload when not specified, sets 1 when true', async () => {
  let captured: string | null = null
  const restore = withFetch(async (url) => {
    captured = url
    return mockResponse(sampleSessionBody)
  })
  try {
    await getPlayback('s', {})
    assert.equal(captured, '/api/playback/s')
    await getPlayback('s', { includePayload: true })
    assert.equal(new URL(`http://x${captured}`).searchParams.get('include_payload'), '1')
  } finally {
    restore()
  }
})

test('getPlayback URL-encodes the session id', async () => {
  let captured: string | null = null
  const restore = withFetch(async (url) => {
    captured = url
    return mockResponse(sampleSessionBody)
  })
  try {
    await getPlayback('a/b c')
    assert.equal(captured, '/api/playback/a%2Fb%20c')
  } finally {
    restore()
  }
})

test('getPlayback surfaces 404', async () => {
  const restore = withFetch(async () => mockResponse({ detail: 'not found' }, 404))
  try {
    await assert.rejects(() => getPlayback('nope'), /404/)
  } finally {
    restore()
  }
})

test('getPlayback handles a tool-call-free session (empty events, 200)', async () => {
  const restore = withFetch(async () => mockResponse({ session_id: 'empty', events: [], total: 0, truncated: false }))
  try {
    const data = await getPlayback('empty')
    assert.deepEqual(data.events, [])
    assert.equal(data.total, 0)
  } finally {
    restore()
  }
})

// ---------------------------------------------------------------------------
// API client — getProjectTimeline
// ---------------------------------------------------------------------------

test('getProjectTimeline hits /api/playback/project/{slug} and forwards since', async () => {
  let captured: string | null = null
  const restore = withFetch(async (url) => {
    captured = url
    return mockResponse(sampleProjectBody)
  })
  try {
    const data = await getProjectTimeline('demo', { since: '7d', toolFilter: ['Edit'], limit: 100 })
    const u = new URL(`http://x${captured}`)
    assert.equal(u.pathname, '/api/playback/project/demo')
    assert.equal(u.searchParams.get('since'), '7d')
    assert.equal(u.searchParams.get('tool_filter'), 'Edit')
    assert.equal(u.searchParams.get('limit'), '100')
    assert.equal(data.project_slug, 'demo')
    assert.equal(data.truncated, true)
  } finally {
    restore()
  }
})

test('getProjectTimeline with no opts has a bare URL', async () => {
  let captured: string | null = null
  const restore = withFetch(async (url) => {
    captured = url
    return mockResponse(sampleProjectBody)
  })
  try {
    await getProjectTimeline('my proj')
    assert.equal(captured, '/api/playback/project/my%20proj')
  } finally {
    restore()
  }
})

test('getProjectTimeline surfaces 404 for an unknown slug', async () => {
  const restore = withFetch(async () => mockResponse({ detail: 'nope' }, 404))
  try {
    await assert.rejects(() => getProjectTimeline('nope'), /404/)
  } finally {
    restore()
  }
})

// ---------------------------------------------------------------------------
// URL state — readPlaybackSelection / writePlaybackSelection round-trip.
// ---------------------------------------------------------------------------

test('readPlaybackSelection: empty search → null pair', () => {
  assert.deepEqual(readPlaybackSelection(''), { session: null, seq: null })
  assert.deepEqual(readPlaybackSelection('?'), { session: null, seq: null })
})

test('readPlaybackSelection: parses session and seq', () => {
  const sel = readPlaybackSelection('?session=S1&seq=42')
  assert.equal(sel.session, 'S1')
  assert.equal(sel.seq, 42)
})

test('readPlaybackSelection: seq must be a non-negative integer', () => {
  assert.equal(readPlaybackSelection('?session=S&seq=-1').seq, null)
  assert.equal(readPlaybackSelection('?session=S&seq=1.5').seq, null)
  assert.equal(readPlaybackSelection('?session=S&seq=abc').seq, null)
  assert.equal(readPlaybackSelection('?session=S&seq=').seq, null)
  assert.equal(readPlaybackSelection('?session=S&seq=0').seq, 0)
})

test('readPlaybackSelection: ignores empty session value and other params', () => {
  assert.equal(readPlaybackSelection('?session=&seq=3').session, null)
  const sel = readPlaybackSelection('?tab=playback&session=S1&q=hi&seq=7')
  assert.equal(sel.session, 'S1')
  assert.equal(sel.seq, 7)
})

test('writePlaybackSelection: serialises both keys', () => {
  assert.equal(writePlaybackSelection('', { session: 'S1', seq: 42 }), '?session=S1&seq=42')
})

test('writePlaybackSelection: clears keys when null / invalid seq', () => {
  assert.equal(writePlaybackSelection('?session=S1&seq=42', { session: null, seq: null }), '')
  // seq=0 is valid and must be kept.
  assert.equal(writePlaybackSelection('', { session: 'S', seq: 0 }), '?session=S&seq=0')
})

test('writePlaybackSelection: preserves unrelated params', () => {
  const out = writePlaybackSelection('?tab=playback&q=hi', { session: 'S1', seq: 5 })
  const params = new URLSearchParams(out)
  assert.equal(params.get('tab'), 'playback')
  assert.equal(params.get('q'), 'hi')
  assert.equal(params.get('session'), 'S1')
  assert.equal(params.get('seq'), '5')
})

test('writePlaybackSelection: round-trips through read', () => {
  const search = writePlaybackSelection('?tab=playback', { session: 'sess-xyz', seq: 13 })
  const sel = readPlaybackSelection(search)
  assert.equal(sel.session, 'sess-xyz')
  assert.equal(sel.seq, 13)
})

test('writePlaybackSelection: changing seq does not duplicate the key', () => {
  let search = writePlaybackSelection('', { session: 'S', seq: 1 })
  search = writePlaybackSelection(search, { session: 'S', seq: 9 })
  const params = new URLSearchParams(search)
  assert.equal(params.getAll('seq').length, 1)
  assert.equal(params.get('seq'), '9')
})
