// Run with: node --test tests/services/meta-agent.test.ts
//
// Locks the meta-agent client + the tool-call surface helpers:
//   * NDJSON line splitter dispatches one parsed event per ``\n``
//   * malformed lines are skipped without aborting the stream
//   * the ``listTools()`` endpoint returns the catalogue payload as-is
//   * model-name heuristic recognises the recommended families
//   * sidebar initial-state resolver picks the right state per viewport
//   * tool-call surface summary line surfaces the right field per tool
//
// Spec: stackunderflow/services/meta_agent.py + the route
// stackunderflow/routes/meta_agent.py.

import { test } from 'node:test'
import assert from 'node:assert/strict'

import { metaAgentApi, modelLikelySupportsTools } from '../../src/services/metaAgent.ts'
import type { MetaAgentEvent, MetaAgentToolInvocation } from '../../src/types/metaAgent.ts'
import {
  buildToolStatusLabel,
  buildToolSummary,
} from '../../src/components/discussion/toolCallSurfaceHelpers.ts'
import { _resolveInitialState } from '../../src/components/layout/metaAgentSidebarHelpers.ts'

// ---------------------------------------------------------------------------
// fetch stub — same pattern as playback-fs tests
// ---------------------------------------------------------------------------

interface MockResponse {
  ok: boolean
  status: number
  statusText: string
  body: ReadableStream<Uint8Array> | null
  text: () => Promise<string>
  json: () => Promise<unknown>
}

function streamFromLines(lines: string[]): ReadableStream<Uint8Array> {
  const encoder = new TextEncoder()
  return new ReadableStream({
    start(controller) {
      for (const line of lines) {
        controller.enqueue(encoder.encode(line))
      }
      controller.close()
    },
  })
}

function mockNdjson(lines: string[], status = 200): MockResponse {
  return {
    ok: status >= 200 && status < 300,
    status,
    statusText: status === 200 ? 'OK' : 'Error',
    body: streamFromLines(lines),
    text: async () => lines.join(''),
    json: async () => ({}),
  }
}

function mockJson(body: unknown, status = 200): MockResponse {
  return {
    ok: status >= 200 && status < 300,
    status,
    statusText: status === 200 ? 'OK' : 'Error',
    body: null,
    text: async () => JSON.stringify(body),
    json: async () => body,
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
// metaAgentApi.chat — NDJSON streaming
// ---------------------------------------------------------------------------

test('chat: dispatches token / tool_call / tool_result / done in order', async () => {
  const events: MetaAgentEvent[] = []
  const restore = withFetch(async () =>
    mockNdjson([
      '{"type":"tool_call","id":"c1","name":"list_recent_sessions","args":{},"ts":"t1"}\n',
      '{"type":"tool_result","id":"c1","name":"list_recent_sessions","ok":true,"data":{"count":3},"duration_ms":12,"ts":"t2"}\n',
      '{"type":"token","delta":"you have ","ts":"t3"}\n',
      '{"type":"token","delta":"3 sessions","ts":"t4"}\n',
      '{"type":"done","hops":2,"ts":"t5"}\n',
    ]),
  )
  try {
    await metaAgentApi.chat(
      { messages: [{ role: 'user', content: 'q' }], model: 'qwen2.5-coder' },
      (ev) => events.push(ev),
    )
  } finally {
    restore()
  }
  const types = events.map((e) => e.type)
  assert.deepEqual(types, ['tool_call', 'tool_result', 'token', 'token', 'done'])
  assert.equal((events[0] as { name: string }).name, 'list_recent_sessions')
  assert.equal((events[1] as { ok: boolean }).ok, true)
})

test('chat: split lines across chunks are reassembled correctly', async () => {
  const events: MetaAgentEvent[] = []
  // Pre-split one event across two chunks to exercise the buffer logic.
  const restore = withFetch(async () =>
    mockNdjson([
      '{"type":"tok',
      'en","delta":"hi","ts":"t"}\n',
      '{"type":"done","hops":1,"ts":"t"}\n',
    ]),
  )
  try {
    await metaAgentApi.chat(
      { messages: [{ role: 'user', content: 'q' }], model: 'x' },
      (ev) => events.push(ev),
    )
  } finally {
    restore()
  }
  assert.deepEqual(events.map((e) => e.type), ['token', 'done'])
})

test('chat: malformed JSON lines are skipped silently', async () => {
  const events: MetaAgentEvent[] = []
  const restore = withFetch(async () =>
    mockNdjson([
      'not-json\n',
      '{"type":"token","delta":"hi","ts":"t"}\n',
      '{broken\n',
      '{"type":"done","hops":1,"ts":"t"}\n',
    ]),
  )
  try {
    await metaAgentApi.chat(
      { messages: [{ role: 'user', content: 'q' }], model: 'x' },
      (ev) => events.push(ev),
    )
  } finally {
    restore()
  }
  assert.deepEqual(events.map((e) => e.type), ['token', 'done'])
})

test('chat: surfaces an HTTP error via thrown Error', async () => {
  const restore = withFetch(async () => mockNdjson([], 400))
  try {
    await metaAgentApi.chat(
      { messages: [{ role: 'user', content: 'q' }], model: 'x' },
      () => {},
    )
    assert.fail('expected chat() to throw on 400')
  } catch (err) {
    assert.ok(err instanceof Error)
    assert.match(err.message, /Meta-agent chat failed/)
  } finally {
    restore()
  }
})

test('chat: posts request body with messages + model + project_slug', async () => {
  let captured: { url: string; body: unknown } | null = null
  const restore = withFetch(async (url, init) => {
    captured = { url, body: init?.body ? JSON.parse(String(init.body)) : null }
    return mockNdjson(['{"type":"done","hops":1,"ts":"t"}\n'])
  })
  try {
    await metaAgentApi.chat(
      {
        messages: [{ role: 'user', content: 'hi' }],
        model: 'qwen2.5-coder',
        project_slug: 'alpha',
        tools_enabled: true,
      },
      () => {},
    )
  } finally {
    restore()
  }
  assert.ok(captured)
  assert.equal(captured!.url, '/api/meta-agent/chat')
  const body = captured!.body as Record<string, unknown>
  assert.equal(body.model, 'qwen2.5-coder')
  assert.equal(body.project_slug, 'alpha')
  assert.equal(body.tools_enabled, true)
  assert.equal((body.messages as Array<{ content: string }>)[0].content, 'hi')
})

// ---------------------------------------------------------------------------
// metaAgentApi.listTools
// ---------------------------------------------------------------------------

test('listTools: returns the catalogue payload', async () => {
  const restore = withFetch(async () =>
    mockJson({
      tools: [{ type: 'function', function: { name: 'x', description: '', parameters: { type: 'object', properties: {} } } }],
      names: ['x'],
      max_hops: 5,
    }),
  )
  try {
    const tools = await metaAgentApi.listTools()
    assert.ok(tools)
    assert.deepEqual(tools!.names, ['x'])
    assert.equal(tools!.max_hops, 5)
  } finally {
    restore()
  }
})

test('listTools: returns null on transport failure', async () => {
  const restore = withFetch(async () => {
    throw new Error('network down')
  })
  try {
    const tools = await metaAgentApi.listTools()
    assert.equal(tools, null)
  } finally {
    restore()
  }
})

test('listTools: returns null on non-2xx', async () => {
  const restore = withFetch(async () => mockJson({}, 500))
  try {
    const tools = await metaAgentApi.listTools()
    assert.equal(tools, null)
  } finally {
    restore()
  }
})

// ---------------------------------------------------------------------------
// modelLikelySupportsTools — heuristic
// ---------------------------------------------------------------------------

test('modelLikelySupportsTools: recognises recommended families', () => {
  assert.equal(modelLikelySupportsTools('qwen2.5-coder:7b'), true)
  assert.equal(modelLikelySupportsTools('llama3.2:latest'), true)
  assert.equal(modelLikelySupportsTools('llama3.1:8b-instruct'), true)
  assert.equal(modelLikelySupportsTools('firefunction-v2'), true)
  assert.equal(modelLikelySupportsTools('mixtral:8x7b'), true)
})

test('modelLikelySupportsTools: rejects unknown models + nullish input', () => {
  assert.equal(modelLikelySupportsTools(''), false)
  assert.equal(modelLikelySupportsTools(undefined), false)
  assert.equal(modelLikelySupportsTools(null), false)
  assert.equal(modelLikelySupportsTools('gemma:2b'), false)
  assert.equal(modelLikelySupportsTools('phi3:mini'), false)
})

// ---------------------------------------------------------------------------
// ToolCallSurface helpers (rendering branches)
// ---------------------------------------------------------------------------

const okInv = (data: Record<string, unknown>): MetaAgentToolInvocation => ({
  id: 'i1',
  name: 'search_past_decisions',
  args: { query: 'x' },
  result: { ok: true, data, duration_ms: 12 },
})

test('buildToolStatusLabel: running vs ok vs error', () => {
  assert.equal(
    buildToolStatusLabel({ id: 'i', name: 'n', args: {} }),
    'running…',
  )
  assert.equal(buildToolStatusLabel(okInv({ count: 3 })), 'ok · 12ms')
  assert.equal(
    buildToolStatusLabel({
      id: 'i',
      name: 'n',
      args: {},
      result: { ok: false, data: { error: 'boom' }, duration_ms: 4 },
    }),
    'error · 4ms',
  )
})

test('buildToolSummary: surfaces count / file_count / total_cost / sessions / error', () => {
  assert.equal(buildToolSummary(okInv({ count: 7 })), '7 matches')
  assert.equal(buildToolSummary(okInv({ file_count: 4 })), '4 files touched')
  assert.equal(
    buildToolSummary(okInv({ total_cost_usd: 12.34, top_projects: [] })),
    '$12.34 total',
  )
  assert.equal(
    buildToolSummary(okInv({ sessions: 3, cost_usd: 0.5 })),
    '3 sessions · $0.50',
  )
  assert.equal(
    buildToolSummary({
      id: 'i',
      name: 'n',
      args: {},
      result: { ok: false, data: { error: 'no such tool' }, duration_ms: 1 },
    }),
    'no such tool',
  )
})

test('buildToolSummary: returns empty when result is pending or unmappable', () => {
  assert.equal(buildToolSummary({ id: 'i', name: 'n', args: {} }), '')
  assert.equal(buildToolSummary(okInv({ irrelevant: true })), '')
})

// ---------------------------------------------------------------------------
// MetaAgentSidebar — _resolveInitialState
// ---------------------------------------------------------------------------

test('sidebar state: large viewport defaults to expanded', () => {
  assert.equal(_resolveInitialState(null, 1440), 'expanded')
})

test('sidebar state: medium viewport defaults to collapsed', () => {
  assert.equal(_resolveInitialState(null, 1024), 'collapsed')
})

test('sidebar state: narrow viewport always hides regardless of persistence', () => {
  assert.equal(_resolveInitialState('expanded', 600), 'hidden')
  assert.equal(_resolveInitialState('collapsed', 600), 'hidden')
})

test('sidebar state: persisted value wins on non-narrow viewports', () => {
  assert.equal(_resolveInitialState('collapsed', 1440), 'collapsed')
  assert.equal(_resolveInitialState('expanded', 1024), 'expanded')
})
