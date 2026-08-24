// Run with: node --test tests/services/projects.test.ts
// (Node 22+ strips TypeScript types automatically; matches the runner used by
// tests/services/format.test.ts and tests/services/etl-status.test.ts.)
//
// Coverage for the project-list fetchers. `getProjects` walks ONE server page;
// `getAllProjects` walks every page. The distinction is load-bearing: the
// Overview filter box matches against whatever list it is handed, so a
// fetcher that quietly stops at the first page turns "your project is on page
// 3" into "no such project" with no error and no empty-state explanation.
// These tests lock the paging contract that keeps the filter complete.

import { test } from 'node:test'
import assert from 'node:assert/strict'

import { PROJECTS_MAX_LIMIT, getAllProjects, getProjects } from '../../src/services/api.ts'
import type { Project, ProjectsResponse } from '../../src/types/api.ts'

// ---------------------------------------------------------------------------
// Helpers — minimal `fetch` stub, same pattern as etl-status.test.ts.
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

function project(name: string): Project {
  return {
    dir_name: name,
    log_path: `/logs/${name}`,
    file_count: 1,
    total_size_mb: 0.5,
    last_modified: 1_700_000_000,
    first_seen: 1_600_000_000,
    display_name: name,
    in_cache: false,
    url_slug: name,
    stats: null,
  } as Project
}

/**
 * A stub server holding `total` projects and honouring `limit`/`offset` the
 * way python-legacy: routes/projects.py does — including the clamp of `limit`
 * to `PROJECTS_MAX_LIMIT`, which is what makes a single unbounded request an
 * unsafe way to "get everything".
 */
function pagedServer(total: number, urls: string[]) {
  return async (url: string): Promise<MockResponse> => {
    urls.push(url)
    const params = new URLSearchParams(url.split('?')[1] ?? '')
    const asked = Number(params.get('limit') ?? total)
    const limit = Math.max(1, Math.min(asked, PROJECTS_MAX_LIMIT))
    const offset = Math.max(0, Number(params.get('offset') ?? 0))
    const all = Array.from({ length: total }, (_, i) => project(`proj-${i}`))
    const body: ProjectsResponse = {
      projects: all.slice(offset, offset + limit),
      total_count: total,
      limit,
      offset,
      has_more: offset + limit < total,
      cache_status: { cached_count: 0, total_projects: total },
    }
    return mockResponse(body)
  }
}

// ---------------------------------------------------------------------------
// getProjects — one page, params encoded as the route expects.
// ---------------------------------------------------------------------------

test('getProjects encodes include_stats + limit/offset', async () => {
  const urls: string[] = []
  const restore = withFetch(pagedServer(303, urls))
  try {
    const page = await getProjects(true, undefined, { limit: 100, offset: 100 })
    assert.equal(urls.length, 1)
    assert.match(urls[0]!, /include_stats=true/)
    assert.match(urls[0]!, /limit=100/)
    assert.match(urls[0]!, /offset=100/)
    assert.equal(page.projects.length, 100)
    assert.equal(page.projects[0]!.dir_name, 'proj-100')
    assert.equal(page.total_count, 303)
    assert.equal(page.has_more, true)
  } finally {
    restore()
  }
})

test('getProjects omits limit/offset when no page is requested', async () => {
  const urls: string[] = []
  const restore = withFetch(pagedServer(5, urls))
  try {
    await getProjects(false)
    assert.equal(urls.length, 1)
    assert.ok(!urls[0]!.includes('limit='), urls[0])
    assert.ok(!urls[0]!.includes('offset='), urls[0])
  } finally {
    restore()
  }
})

// ---------------------------------------------------------------------------
// getAllProjects — completeness is the whole point.
// ---------------------------------------------------------------------------

test('getAllProjects returns every project from a single-page store', async () => {
  const urls: string[] = []
  const restore = withFetch(pagedServer(303, urls))
  try {
    const all = await getAllProjects(true)
    assert.equal(urls.length, 1, 'one request covers a store under the clamp')
    assert.equal(all.projects.length, 303)
    assert.equal(all.total_count, 303)
    assert.equal(all.has_more, false)
    assert.equal(all.offset, 0)
  } finally {
    restore()
  }
})

test('getAllProjects walks past the server limit clamp', async () => {
  // 2400 projects: the server clamps any `limit` to 1000, so the ONLY way to
  // see the last 400 is to keep paging. A fetcher that trusted one request
  // would return 1000 rows that look like the whole store.
  const urls: string[] = []
  const restore = withFetch(pagedServer(2400, urls))
  try {
    const all = await getAllProjects(true)
    assert.equal(urls.length, 3, '2400 / 1000 → 3 pages')
    assert.equal(all.projects.length, 2400)
    assert.equal(all.projects[0]!.dir_name, 'proj-0')
    assert.equal(all.projects[2399]!.dir_name, 'proj-2399')
    assert.equal(all.has_more, false)
    // No duplicates — offsets advance, they don't restart.
    assert.equal(new Set(all.projects.map((p) => p.dir_name)).size, 2400)
  } finally {
    restore()
  }
})

test('getAllProjects honours an explicit smaller page size', async () => {
  const urls: string[] = []
  const restore = withFetch(pagedServer(303, urls))
  try {
    const all = await getAllProjects(true, undefined, 100)
    assert.equal(urls.length, 4, '303 / 100 → 4 pages')
    assert.equal(all.projects.length, 303)
    assert.deepEqual(
      urls.map((u) => new URLSearchParams(u.split('?')[1]).get('offset')),
      ['0', '100', '200', '300'],
    )
  } finally {
    restore()
  }
})

test('getAllProjects forwards provider/model filters on every page', async () => {
  const urls: string[] = []
  const restore = withFetch(pagedServer(250, urls))
  try {
    await getAllProjects(true, { providers: ['Codex'] }, 100)
    assert.equal(urls.length, 3)
    for (const url of urls) {
      assert.match(url, /provider=codex/, 'filter must not be dropped mid-walk')
    }
  } finally {
    restore()
  }
})

test('getAllProjects stops on an empty page instead of spinning', async () => {
  // A server that keeps claiming `has_more` while handing back nothing must
  // terminate the walk, not hang the dashboard.
  let calls = 0
  const restore = withFetch(async () => {
    calls += 1
    const body: ProjectsResponse = {
      projects: calls === 1 ? [project('only')] : [],
      total_count: 999,
      limit: 1000,
      offset: 0,
      has_more: true,
      cache_status: { cached_count: 0, total_projects: 999 },
    }
    return mockResponse(body)
  })
  try {
    const all = await getAllProjects(true)
    assert.equal(calls, 2)
    assert.equal(all.projects.length, 1)
    assert.equal(all.has_more, false)
  } finally {
    restore()
  }
})

test('getAllProjects surfaces a failed page as an error', async () => {
  const restore = withFetch(async () => mockResponse('boom', 500))
  try {
    await assert.rejects(() => getAllProjects(true), /500/)
  } finally {
    restore()
  }
})

// ---------------------------------------------------------------------------
// The filter predicate the Overview table applies to the list above. Mirrored
// here so the "name OR path matches, case-insensitively" contract is pinned:
// a project is findable by its display name and by its slug.
// ---------------------------------------------------------------------------

function matches(p: Project, query: string): boolean {
  const q = query.toLowerCase()
  return p.display_name.toLowerCase().includes(q) || p.dir_name.toLowerCase().includes(q)
}

test('project filter matches display name and slug, case-insensitively', () => {
  const p = { ...project('-Users-yad-dev-year25-GranolaRev'), display_name: 'GranolaRev' }
  assert.equal(matches(p, 'granola'), true, 'display name, lowercased query')
  assert.equal(matches(p, 'GRANOLA'), true, 'uppercase query')
  assert.equal(matches(p, 'year25'), true, 'slug-only substring')
  assert.equal(matches(p, 'xterm'), false)
})
