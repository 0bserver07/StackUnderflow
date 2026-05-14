// Run with: node --test tests/services/agent-teams.test.ts
// Locks the API client + URL state contract for the Agents tab.
//
// Spec: docs/specs/agent-teams.md

import { test } from 'node:test'
import assert from 'node:assert/strict'

import {
  listAgentTeams,
  getAgentTeam,
  getAgentTeamTranscript,
  readAgentTeamSelection,
  writeAgentTeamSelection,
} from '../../src/services/api.ts'
import type {
  AgentTeamListResponse,
  AgentTeamGraph,
  AgentTeamTranscriptResponse,
} from '../../src/types/api.ts'

// ---------------------------------------------------------------------------
// fetch stub (matches the etl-status / format / filters test pattern).
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
// Sample payloads — every field populated.
// ---------------------------------------------------------------------------

const sampleListBody: AgentTeamListResponse = {
  teams: [
    {
      session_id: 'lead-001',
      project_slug: 'stackunderflow',
      project_display_name: 'StackUnderflow',
      team_name: 'wave-12',
      first_ts: '2026-04-01T00:00:00Z',
      last_ts: '2026-04-01T01:00:00Z',
      agent_count: 3,
      sub_agent_message_count: 412,
      lead_message_count: 51,
    },
  ],
}

const sampleGraphBody: AgentTeamGraph = {
  session_id: 'lead-001',
  team_name: 'wave-12',
  project_slug: 'stackunderflow',
  project_display_name: 'StackUnderflow',
  lead: {
    session_id: 'lead-001',
    agent_id: null,
    agent_name: 'team-lead',
    is_lead: true,
    parent_session_id: null,
    message_count: 51,
    first_ts: '2026-04-01T00:00:00Z',
    last_ts: '2026-04-01T01:00:00Z',
    first_user_prompt: 'Boot the team',
    model: 'claude-opus-4-7',
    cost_usd: 12.5,
  },
  agents: [
    {
      session_id: 'sub-a',
      agent_id: 'a-worker',
      agent_name: 'a-worker',
      is_lead: false,
      parent_session_id: 'lead-001',
      message_count: 87,
      first_ts: '2026-04-01T00:10:00Z',
      last_ts: '2026-04-01T00:50:00Z',
      first_user_prompt: 'do thing A',
      model: 'claude-sonnet-4-7',
      cost_usd: 3.21,
    },
    {
      session_id: 'sub-b',
      agent_id: 'b-worker',
      agent_name: 'b-worker',
      is_lead: false,
      parent_session_id: 'lead-001',
      message_count: 99,
      first_ts: '2026-04-01T00:20:00Z',
      last_ts: '2026-04-01T01:00:00Z',
      first_user_prompt: 'do thing B',
      model: 'claude-sonnet-4-7',
      cost_usd: 4.56,
    },
  ],
}

const sampleTranscriptBody: AgentTeamTranscriptResponse = {
  session_id: 'lead-001',
  agent_session_id: 'sub-a',
  message_count: 2,
  messages: [
    {
      id: 1,
      seq: 0,
      timestamp: '2026-04-01T00:10:00Z',
      role: 'user',
      model: null,
      content_text: 'do thing A',
      is_sidechain: true,
      uuid: 'u-1',
      parent_uuid: null,
    },
    {
      id: 2,
      seq: 1,
      timestamp: '2026-04-01T00:11:00Z',
      role: 'assistant',
      model: 'claude-sonnet-4-7',
      content_text: 'done',
      is_sidechain: true,
      uuid: 'u-2',
      parent_uuid: 'u-1',
    },
  ],
}

// ---------------------------------------------------------------------------
// API client — listAgentTeams
// ---------------------------------------------------------------------------

test('listAgentTeams calls /api/agent-teams with default limit', async () => {
  let captured: string | null = null
  const restore = withFetch(async (url) => {
    captured = url
    return mockResponse(sampleListBody)
  })
  try {
    const data = await listAgentTeams()
    assert.equal(captured, '/api/agent-teams?limit=50')
    assert.equal(data.teams.length, 1)
    assert.equal(data.teams[0]!.session_id, 'lead-001')
    assert.equal(data.teams[0]!.team_name, 'wave-12')
    assert.equal(data.teams[0]!.agent_count, 3)
  } finally {
    restore()
  }
})

test('listAgentTeams forwards a custom limit', async () => {
  let captured: string | null = null
  const restore = withFetch(async (url) => {
    captured = url
    return mockResponse({ teams: [] })
  })
  try {
    const data = await listAgentTeams(7)
    assert.equal(captured, '/api/agent-teams?limit=7')
    assert.deepEqual(data, { teams: [] })
  } finally {
    restore()
  }
})

test('listAgentTeams handles empty store cleanly', async () => {
  const restore = withFetch(async () => mockResponse({ teams: [] }))
  try {
    const data = await listAgentTeams()
    assert.deepEqual(data, { teams: [] })
  } finally {
    restore()
  }
})

test('listAgentTeams surfaces non-200 errors', async () => {
  const restore = withFetch(async () => mockResponse('boom', 500))
  try {
    await assert.rejects(listAgentTeams, /500/)
  } finally {
    restore()
  }
})

// ---------------------------------------------------------------------------
// API client — getAgentTeam
// ---------------------------------------------------------------------------

test('getAgentTeam URL-encodes the session id', async () => {
  let captured: string | null = null
  const restore = withFetch(async (url) => {
    captured = url
    return mockResponse(sampleGraphBody)
  })
  try {
    await getAgentTeam('lead/with spaces')
    assert.equal(captured, '/api/agent-teams/lead%2Fwith%20spaces')
  } finally {
    restore()
  }
})

test('getAgentTeam returns the parsed graph', async () => {
  const restore = withFetch(async () => mockResponse(sampleGraphBody))
  try {
    const g = await getAgentTeam('lead-001')
    assert.equal(g.session_id, 'lead-001')
    assert.equal(g.lead.is_lead, true)
    assert.equal(g.agents.length, 2)
    assert.equal(g.agents[0]!.session_id, 'sub-a')
    assert.equal(g.agents[1]!.session_id, 'sub-b')
  } finally {
    restore()
  }
})

test('getAgentTeam surfaces 404 errors', async () => {
  const restore = withFetch(async () => mockResponse({ detail: 'not found' }, 404))
  try {
    await assert.rejects(() => getAgentTeam('nope'), /404/)
  } finally {
    restore()
  }
})

// ---------------------------------------------------------------------------
// API client — getAgentTeamTranscript
// ---------------------------------------------------------------------------

test('getAgentTeamTranscript URL-encodes both ids and parses messages', async () => {
  let captured: string | null = null
  const restore = withFetch(async (url) => {
    captured = url
    return mockResponse(sampleTranscriptBody)
  })
  try {
    const tx = await getAgentTeamTranscript('lead-001', 'sub-a')
    assert.equal(captured, '/api/agent-teams/lead-001/agent/sub-a')
    assert.equal(tx.message_count, 2)
    assert.equal(tx.messages[0]!.content_text, 'do thing A')
    assert.equal(tx.messages[1]!.is_sidechain, true)
  } finally {
    restore()
  }
})

// ---------------------------------------------------------------------------
// URL state — readAgentTeamSelection / writeAgentTeamSelection round-trip.
// ---------------------------------------------------------------------------

test('readAgentTeamSelection: empty search → null pair', () => {
  assert.deepEqual(readAgentTeamSelection(''), { session: null, agent: null })
  assert.deepEqual(readAgentTeamSelection('?'), { session: null, agent: null })
})

test('readAgentTeamSelection: parses both params', () => {
  const sel = readAgentTeamSelection('?session=L1&agent=A2')
  assert.equal(sel.session, 'L1')
  assert.equal(sel.agent, 'A2')
})

test('readAgentTeamSelection: ignores empty values', () => {
  const sel = readAgentTeamSelection('?session=&agent=')
  assert.equal(sel.session, null)
  assert.equal(sel.agent, null)
})

test('readAgentTeamSelection: ignores other params', () => {
  const sel = readAgentTeamSelection('?tab=agents&session=L1&q=hello')
  assert.equal(sel.session, 'L1')
  assert.equal(sel.agent, null)
})

test('writeAgentTeamSelection: serialises both keys', () => {
  const out = writeAgentTeamSelection('', { session: 'L1', agent: 'A2' })
  assert.equal(out, '?session=L1&agent=A2')
})

test('writeAgentTeamSelection: clears keys when null', () => {
  const out = writeAgentTeamSelection('?session=L1&agent=A2', {
    session: null,
    agent: null,
  })
  assert.equal(out, '')
})

test('writeAgentTeamSelection: preserves unrelated params', () => {
  const out = writeAgentTeamSelection('?tab=agents&q=hello', {
    session: 'L1',
    agent: null,
  })
  // tab + q preserved, session added, agent absent
  const params = new URLSearchParams(out)
  assert.equal(params.get('tab'), 'agents')
  assert.equal(params.get('q'), 'hello')
  assert.equal(params.get('session'), 'L1')
  assert.equal(params.get('agent'), null)
})

test('writeAgentTeamSelection: round-trips through read', () => {
  const search = writeAgentTeamSelection('?tab=agents', {
    session: 'lead-xyz',
    agent: 'sub-abc',
  })
  const sel = readAgentTeamSelection(search)
  assert.equal(sel.session, 'lead-xyz')
  assert.equal(sel.agent, 'sub-abc')
})

test('writeAgentTeamSelection: switching agent does not duplicate keys', () => {
  let search = writeAgentTeamSelection('', { session: 'L', agent: 'A' })
  search = writeAgentTeamSelection(search, { session: 'L', agent: 'B' })
  const params = new URLSearchParams(search)
  // URLSearchParams.getAll() returns ALL values for a key — it should be
  // exactly one for each.
  assert.equal(params.getAll('session').length, 1)
  assert.equal(params.getAll('agent').length, 1)
  assert.equal(params.get('agent'), 'B')
})
