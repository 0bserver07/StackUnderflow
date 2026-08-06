// Meta-agent client — streams from ``POST /api/meta-agent/chat`` and
// surfaces typed events to React.
//
// The route returns an NDJSON stream (one event per line); we read it
// progressively and dispatch each parsed event via the ``onEvent``
// callback. Aborting the controller cancels the upstream fetch — the
// backend's Ollama stream is closed too because httpx's connection
// pool ties the upstream lifetime to the response context.
//
// The matching backend module is ``stackunderflow/routes/meta_agent.py``.

import type {
  MetaAgentEvent,
  MetaAgentToolsResponse,
} from '../types/metaAgent'

const BASE = '/api/meta-agent'

export interface MetaAgentChatRequest {
  messages: Array<{ role: string; content: string }>
  model: string
  tools_enabled?: boolean
  project_slug?: string | null
}

export const metaAgentApi = {
  async chat(
    request: MetaAgentChatRequest,
    onEvent: (event: MetaAgentEvent) => void,
    signal?: AbortSignal,
  ): Promise<void> {
    const response = await fetch(`${BASE}/chat`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(request),
      signal,
    })

    if (!response.ok) {
      const text = await response.text().catch(() => '')
      throw new Error(
        `Meta-agent chat failed: ${response.status} ${response.statusText}${text ? ` - ${text}` : ''}`,
      )
    }

    const reader = response.body?.getReader()
    if (!reader) throw new Error('No response body')

    const decoder = new TextDecoder()
    let buffer = ''

    try {
      while (true) {
        const { done, value } = await reader.read()
        if (done) break

        buffer += decoder.decode(value, { stream: true })
        const lines = buffer.split('\n')
        buffer = lines.pop() || ''

        for (const line of lines) {
          if (!line.trim()) continue
          try {
            const event = JSON.parse(line) as MetaAgentEvent
            onEvent(event)
          } catch {
            // Skip parse errors — the stream is best-effort.
          }
        }
      }

      // Flush any trailing partial line.
      if (buffer.trim()) {
        try {
          const event = JSON.parse(buffer) as MetaAgentEvent
          onEvent(event)
        } catch {
          // Ignore.
        }
      }
    } finally {
      reader.releaseLock()
    }
  },

  async listTools(): Promise<MetaAgentToolsResponse | null> {
    try {
      const res = await fetch(`${BASE}/tools`)
      if (!res.ok) return null
      return (await res.json()) as MetaAgentToolsResponse
    } catch {
      return null
    }
  },
}

// Heuristic: which locally-pulled Ollama models honour the ``tools``
// array on ``/api/chat``. The frontend uses this as a fallback when
// the Ollama tags endpoint doesn't surface the ``capabilities`` field
// (older Ollama versions). The list is intentionally short — names the
// docs recommend — and substring-matched case-insensitively. If a model
// the user has isn't on the list, we still send the tools array and let
// Ollama / the model decide; the worst case is the model ignores it.
const KNOWN_TOOL_MODEL_FRAGMENTS = [
  'qwen2.5-coder',
  'qwen2.5',
  'llama3.2',
  'llama3.1',
  'firefunction',
  'command-r',
  'mistral-nemo',
  'mistral-large',
  'mixtral',
  'deepseek',
]

export function modelLikelySupportsTools(name: string | undefined | null): boolean {
  if (!name) return false
  const lower = name.toLowerCase()
  return KNOWN_TOOL_MODEL_FRAGMENTS.some((frag) => lower.includes(frag))
}
