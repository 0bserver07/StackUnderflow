// Run with: node --test tests/services/playback-fs.test.ts
// (Node 22+ strips TypeScript types automatically.)
//
// Locks the Playback v2 surface: the FS-snapshot API client + the pure
// helpers (humanizeBytes / formatSnapshotTs / groupFilesByDirectory /
// debounce) that the side panel composes. We also exercise the panel's
// behavioural seams (file selection driving the content pane) via the
// helper module rather than a DOM runner — this project doesn't ship one.
//
// Spec: stackunderflow/services/playback_fs.py.

import { test } from 'node:test'
import assert from 'node:assert/strict'

import {
  getPlaybackFsSnapshot,
  PlaybackFsBadTimestampError,
} from '../../src/services/api.ts'
import type {
  PlaybackFsFileEntry,
  PlaybackFsSnapshotResponse,
} from '../../src/types/api.ts'
import {
  debounce,
  formatSnapshotTs,
  groupFilesByDirectory,
  humanizeBytes,
} from '../../src/components/dashboard/playbackFs.ts'

// ---------------------------------------------------------------------------
// fetch stub (matches the playback / etl-status test pattern).
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
    statusText:
      status === 200
        ? 'OK'
        : status === 404
          ? 'Not Found'
          : status === 422
            ? 'Unprocessable Entity'
            : 'Error',
    json: async () => body,
    text: async () => (typeof body === 'string' ? body : JSON.stringify(body)),
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
// Sample payloads.
// ---------------------------------------------------------------------------

const sampleFile: PlaybackFsFileEntry = {
  content: 'print("hello")\n',
  byte_count: 15,
  last_modified_ts: '2026-05-13T11:55:00Z',
  operations_applied: ['Read#0', 'Edit#0'],
  reconstruction_complete: true,
}

const sampleSnapshot: PlaybackFsSnapshotResponse = {
  session_id: 'sess-1',
  snapshot_ts: '2026-05-13T12:00:00Z',
  files: {
    'src/main.py': sampleFile,
  },
  warnings: [],
}

// ---------------------------------------------------------------------------
// API client — getPlaybackFsSnapshot
// ---------------------------------------------------------------------------

test('getPlaybackFsSnapshot: serialises `at` and uses /fs path', async () => {
  let captured: string | null = null
  const restore = withFetch(async (url) => {
    captured = url
    return mockResponse(sampleSnapshot)
  })
  try {
    const data = await getPlaybackFsSnapshot('sess-1', { at: '2026-05-13T12:00:00Z' })
    const u = new URL(`http://x${captured}`)
    assert.equal(u.pathname, '/api/playback/sess-1/fs')
    assert.equal(u.searchParams.get('at'), '2026-05-13T12:00:00Z')
    assert.equal(u.searchParams.get('paths'), null)
    assert.equal(u.searchParams.get('include_content'), null)
    assert.equal(data.session_id, 'sess-1')
    assert.equal(Object.keys(data.files).length, 1)
  } finally {
    restore()
  }
})

test('getPlaybackFsSnapshot: forwards include_content=false (metadata-only scrub)', async () => {
  let captured: string | null = null
  const restore = withFetch(async (url) => {
    captured = url
    return mockResponse({ ...sampleSnapshot, files: {} })
  })
  try {
    await getPlaybackFsSnapshot('sess-1', {
      at: '2026-05-13T12:00:00Z',
      includeContent: false,
    })
    const u = new URL(`http://x${captured}`)
    assert.equal(u.searchParams.get('include_content'), 'false')
  } finally {
    restore()
  }
})

test('getPlaybackFsSnapshot: forwards include_content=true and paths', async () => {
  let captured: string | null = null
  const restore = withFetch(async (url) => {
    captured = url
    return mockResponse(sampleSnapshot)
  })
  try {
    await getPlaybackFsSnapshot('sess-1', {
      at: '2026-05-13T12:00:00Z',
      paths: ['src/main.py', 'src/util.py'],
      includeContent: true,
    })
    const u = new URL(`http://x${captured}`)
    assert.equal(u.searchParams.get('include_content'), 'true')
    assert.equal(u.searchParams.get('paths'), 'src/main.py,src/util.py')
  } finally {
    restore()
  }
})

test('getPlaybackFsSnapshot: empty paths array is dropped (no `paths` param)', async () => {
  let captured: string | null = null
  const restore = withFetch(async (url) => {
    captured = url
    return mockResponse(sampleSnapshot)
  })
  try {
    await getPlaybackFsSnapshot('sess-1', { at: '2026-05-13T12:00:00Z', paths: [] })
    const u = new URL(`http://x${captured}`)
    assert.equal(u.searchParams.get('paths'), null)
  } finally {
    restore()
  }
})

test('getPlaybackFsSnapshot: URL-encodes the session id', async () => {
  let captured: string | null = null
  const restore = withFetch(async (url) => {
    captured = url
    return mockResponse(sampleSnapshot)
  })
  try {
    await getPlaybackFsSnapshot('a/b c', { at: '2026-05-13T12:00:00Z' })
    assert.match(captured!, /^\/api\/playback\/a%2Fb%20c\/fs\?/)
  } finally {
    restore()
  }
})

test('getPlaybackFsSnapshot: 200 + files:{} for a session with no FS calls', async () => {
  const restore = withFetch(async () =>
    mockResponse({
      session_id: 'empty',
      snapshot_ts: '2026-05-13T12:00:00Z',
      files: {},
      warnings: [],
    }),
  )
  try {
    const data = await getPlaybackFsSnapshot('empty', { at: '2026-05-13T12:00:00Z' })
    assert.deepEqual(data.files, {})
    assert.deepEqual(data.warnings, [])
  } finally {
    restore()
  }
})

test('getPlaybackFsSnapshot: surfaces 404 as a generic Error', async () => {
  const restore = withFetch(async () => mockResponse({ detail: 'not found' }, 404))
  try {
    await assert.rejects(
      () => getPlaybackFsSnapshot('nope', { at: '2026-05-13T12:00:00Z' }),
      /404/,
    )
  } finally {
    restore()
  }
})

test('getPlaybackFsSnapshot: 422 surfaces as PlaybackFsBadTimestampError', async () => {
  const restore = withFetch(async () =>
    mockResponse({ detail: 'unparseable' }, 422),
  )
  try {
    await assert.rejects(
      () => getPlaybackFsSnapshot('sess-1', { at: 'not-a-date' }),
      (err: unknown) => err instanceof PlaybackFsBadTimestampError,
    )
  } finally {
    restore()
  }
})

test('getPlaybackFsSnapshot: warnings propagate through the response', async () => {
  const restore = withFetch(async () =>
    mockResponse({
      session_id: 'sess-1',
      snapshot_ts: '2026-05-13T12:00:00Z',
      files: { 'src/foo.py': sampleFile },
      warnings: [
        'src/foo.py: Edit#3 old_string did not match — substitution skipped',
      ],
    }),
  )
  try {
    const data = await getPlaybackFsSnapshot('sess-1', { at: '2026-05-13T12:00:00Z' })
    assert.equal(data.warnings.length, 1)
    assert.match(data.warnings[0]!, /substitution skipped/)
  } finally {
    restore()
  }
})

// ---------------------------------------------------------------------------
// Side panel helpers — humanizeBytes
// ---------------------------------------------------------------------------

test('humanizeBytes: null / undefined / NaN → em dash', () => {
  assert.equal(humanizeBytes(null), '—')
  assert.equal(humanizeBytes(undefined), '—')
  assert.equal(humanizeBytes(Number.NaN), '—')
})

test('humanizeBytes: sub-KB → bytes', () => {
  assert.equal(humanizeBytes(0), '0 B')
  assert.equal(humanizeBytes(512), '512 B')
  assert.equal(humanizeBytes(1023), '1023 B')
})

test('humanizeBytes: KB / MB scaling rounds to 1 decimal', () => {
  assert.equal(humanizeBytes(1024), '1.0 KB')
  assert.equal(humanizeBytes(2048), '2.0 KB')
  assert.equal(humanizeBytes(1024 * 1024), '1.0 MB')
  assert.equal(humanizeBytes(5 * 1024 * 1024 + 500_000), '5.5 MB')
})

// ---------------------------------------------------------------------------
// Side panel helpers — formatSnapshotTs
// ---------------------------------------------------------------------------

test('formatSnapshotTs: null → em dash', () => {
  assert.equal(formatSnapshotTs(null), '—')
})

test('formatSnapshotTs: malformed string falls back to itself', () => {
  assert.equal(formatSnapshotTs('not-a-date'), 'not-a-date')
})

test('formatSnapshotTs: valid ISO renders a non-empty short label', () => {
  const out = formatSnapshotTs('2026-05-13T12:00:00Z')
  assert.notEqual(out, '—')
  assert.notEqual(out, '2026-05-13T12:00:00Z')
  assert.ok(out.length > 0)
})

// ---------------------------------------------------------------------------
// Side panel helpers — groupFilesByDirectory
// ---------------------------------------------------------------------------

const fileEntry = (op: string, complete = true): PlaybackFsFileEntry => ({
  byte_count: 100,
  last_modified_ts: '2026-05-13T12:00:00Z',
  operations_applied: [op],
  reconstruction_complete: complete,
})

test('groupFilesByDirectory: empty map → empty array', () => {
  assert.deepEqual(groupFilesByDirectory({}), [])
})

test('groupFilesByDirectory: groups by parent dir, sorted root-first then alpha', () => {
  const groups = groupFilesByDirectory({
    'README.md': fileEntry('Read#0'),
    'src/main.py': fileEntry('Edit#0'),
    'src/util.py': fileEntry('Edit#1'),
    'tests/test_main.py': fileEntry('Read#1'),
  })
  assert.equal(groups.length, 3)
  assert.equal(groups[0]!.dir, '') // root first
  assert.equal(groups[0]!.files[0]!.basename, 'README.md')
  assert.equal(groups[1]!.dir, 'src')
  assert.equal(groups[1]!.files.length, 2)
  assert.equal(groups[1]!.files[0]!.basename, 'main.py') // alpha within
  assert.equal(groups[1]!.files[1]!.basename, 'util.py')
  assert.equal(groups[2]!.dir, 'tests')
})

test('groupFilesByDirectory: preserves the original entry on each node', () => {
  const groups = groupFilesByDirectory({
    'src/main.py': fileEntry('Edit#0', false),
  })
  assert.equal(groups[0]!.files[0]!.entry.reconstruction_complete, false)
  assert.deepEqual(groups[0]!.files[0]!.entry.operations_applied, ['Edit#0'])
})

test('groupFilesByDirectory: deep paths use the deepest parent as the group', () => {
  const groups = groupFilesByDirectory({
    'a/b/c/d.py': fileEntry('Read#0'),
    'a/b/c/e.py': fileEntry('Read#1'),
    'a/b/f.py': fileEntry('Read#2'),
  })
  const dirs = groups.map((g) => g.dir)
  assert.deepEqual(dirs, ['a/b', 'a/b/c'])
})

// ---------------------------------------------------------------------------
// Side panel helpers — debounce (drives the 250ms scrub throttle)
// ---------------------------------------------------------------------------

test('debounce: only fires once per quiet window', (_, done) => {
  let calls = 0
  const fn = debounce(() => {
    calls += 1
  }, 30)
  fn()
  fn()
  fn()
  setTimeout(() => {
    assert.equal(calls, 1)
    done()
  }, 80)
})

test('debounce: cancel() drops the pending invocation', (_, done) => {
  let calls = 0
  const fn = debounce(() => {
    calls += 1
  }, 30)
  fn()
  fn.cancel()
  setTimeout(() => {
    assert.equal(calls, 0)
    done()
  }, 80)
})

test('debounce: latest args win after a burst', (_, done) => {
  const captured: number[] = []
  const fn = debounce((n: number) => {
    captured.push(n)
  }, 30)
  fn(1)
  fn(2)
  fn(3)
  setTimeout(() => {
    assert.deepEqual(captured, [3])
    done()
  }, 80)
})

// ---------------------------------------------------------------------------
// Side panel composition seam — "file selection updates content pane"
// We can't render the React component without a DOM runner, but the
// selection-driven contract is: pick a path → call getPlaybackFsSnapshot
// with paths=[that_path] and include_content=true. Verify the API client
// honours that contract end-to-end.
// ---------------------------------------------------------------------------

test('Panel selection contract: clicking a file → /fs?paths=…&include_content=true', async () => {
  let captured: string | null = null
  const restore = withFetch(async (url) => {
    captured = url
    return mockResponse({
      session_id: 'sess-1',
      snapshot_ts: '2026-05-13T12:00:00Z',
      files: { 'src/main.py': sampleFile },
      warnings: [],
    })
  })
  try {
    const data = await getPlaybackFsSnapshot('sess-1', {
      at: '2026-05-13T12:00:00Z',
      paths: ['src/main.py'],
      includeContent: true,
    })
    const u = new URL(`http://x${captured}`)
    assert.equal(u.searchParams.get('paths'), 'src/main.py')
    assert.equal(u.searchParams.get('include_content'), 'true')
    assert.equal(data.files['src/main.py']?.content, 'print("hello")\n')
  } finally {
    restore()
  }
})

test('Panel empty-state contract: 200 + files:{} returns the empty map', async () => {
  const restore = withFetch(async () =>
    mockResponse({
      session_id: 'sess-1',
      snapshot_ts: '2026-05-13T12:00:00Z',
      files: {},
      warnings: [],
    }),
  )
  try {
    const data = await getPlaybackFsSnapshot('sess-1', { at: '2026-05-13T12:00:00Z' })
    assert.equal(Object.keys(data.files).length, 0)
    // Caller renders "No file operations in this session before [ts]" when empty.
  } finally {
    restore()
  }
})

test('Panel warnings contract: warnings array is rendered above the body', async () => {
  const restore = withFetch(async () =>
    mockResponse({
      session_id: 'sess-1',
      snapshot_ts: '2026-05-13T12:00:00Z',
      files: { 'src/main.py': sampleFile },
      warnings: [
        'src/main.py: Edit#3 old_string did not match — substitution skipped',
      ],
    }),
  )
  try {
    const data = await getPlaybackFsSnapshot('sess-1', { at: '2026-05-13T12:00:00Z' })
    assert.equal(data.warnings.length, 1)
    // Caller renders this in the warnings banner.
  } finally {
    restore()
  }
})
