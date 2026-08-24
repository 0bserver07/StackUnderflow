// Meta-agent wire types — mirror python-legacy: services/meta_agent.py
// and python-legacy: routes/meta_agent.py.
//
// The route streams one JSON object per line ("application/x-ndjson")
// where each line carries a ``type`` discriminator. Keep these shapes
// strictly aligned with the backend or the sidebar will silently render
// nothing.

export type MetaAgentEventType =
  | 'token'
  | 'tool_call'
  | 'tool_result'
  | 'error'
  | 'done'

export interface MetaAgentTokenEvent {
  type: 'token'
  delta: string
  ts: string
}

export interface MetaAgentToolCallEvent {
  type: 'tool_call'
  id: string
  name: string
  args: Record<string, unknown>
  ts: string
}

export interface MetaAgentToolResultEvent {
  type: 'tool_result'
  id: string
  name: string
  ok: boolean
  data: Record<string, unknown>
  duration_ms: number
  ts: string
}

export interface MetaAgentErrorEvent {
  type: 'error'
  message: string
  ts: string
}

export interface MetaAgentDoneEvent {
  type: 'done'
  hops: number
  ts: string
}

export type MetaAgentEvent =
  | MetaAgentTokenEvent
  | MetaAgentToolCallEvent
  | MetaAgentToolResultEvent
  | MetaAgentErrorEvent
  | MetaAgentDoneEvent

// One tool entry in the catalogue endpoint. Mirrors the JSONSchema
// shape Ollama's ``/api/chat`` tools array uses.
export interface MetaAgentToolSchema {
  type: 'function'
  function: {
    name: string
    description: string
    parameters: {
      type: 'object'
      properties: Record<string, unknown>
      required?: string[]
    }
  }
}

export interface MetaAgentToolsResponse {
  tools: MetaAgentToolSchema[]
  names: string[]
  max_hops: number
}

// Frontend chat message — extends the existing ``ChatMessage`` shape by
// adding the optional ``toolCalls`` array. Tool calls are rendered as
// inline collapsed ``<details>`` blocks above the next assistant turn.
export interface MetaAgentToolInvocation {
  id: string
  name: string
  args: Record<string, unknown>
  // result is undefined while the executor is still running.
  result?: {
    ok: boolean
    data: Record<string, unknown>
    duration_ms: number
  }
}

export interface MetaAgentMessage {
  id: string
  role: 'user' | 'assistant' | 'system'
  content: string
  timestamp: Date
  // Tool invocations that ran while producing this assistant turn.
  // Rendered as collapsed surfaces above the message bubble.
  toolCalls?: MetaAgentToolInvocation[]
  error?: string
}

export interface MetaAgentSession {
  id: string
  contextLabel: string
  createdAt: Date
  updatedAt: Date
  messages: MetaAgentMessage[]
  projectSlug: string | null
}
