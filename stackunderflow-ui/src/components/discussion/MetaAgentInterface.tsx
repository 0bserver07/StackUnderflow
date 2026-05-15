// Meta-agent chat surface — replaces the legacy plain-Ollama ChatInterface.
//
// Differences from the old ChatInterface:
//   * Streams from ``/api/meta-agent/chat`` (NDJSON, our route) instead
//     of the raw Ollama proxy. The route loops tool calls server-side.
//   * Tool calls produce ``MetaAgentToolInvocation`` records that render
//     as inline ``ToolCallSurface`` blocks above the assistant bubble.
//   * Sessions persist under a different ``localStorage`` key so the
//     legacy chat history isn't accidentally co-mingled.
//
// When the user picks a model that the heuristic doesn't recognise as
// supporting tool-calling, we still send the tools array but render a
// pill above the composer so the user knows tool execution may be a
// no-op.

import { useState, useEffect, useCallback, useRef } from 'react'
import { IconAlertTriangle } from '@tabler/icons-react'
import { ollamaApi } from '../../services/ollama'
import { metaAgentApi, modelLikelySupportsTools } from '../../services/metaAgent'
import type { OllamaModel } from '../../types/chat'
import type { QADetailResponse } from '../../types/api'
import type {
  MetaAgentEvent,
  MetaAgentMessage,
  MetaAgentSession,
  MetaAgentToolInvocation,
} from '../../types/metaAgent'
import ModelSelector from './ModelSelector'
import MetaAgentMessageList from './MetaAgentMessageList'
import ChatInput from './ChatInput'
import ChatSessionManager from './ChatSessionManager'

interface MetaAgentInterfaceProps {
  currentQA: QADetailResponse | null
  currentSessionFile: string | null
  selectedProject: string | null
}

const STORAGE_KEY = 'stackunderflow_metaAgentSessions'

function generateId(): string {
  return `${Date.now()}-${Math.random().toString(36).substring(2, 9)}`
}

function loadSessions(): MetaAgentSession[] {
  try {
    const raw = localStorage.getItem(STORAGE_KEY)
    if (!raw) return []
    const parsed = JSON.parse(raw)
    return parsed.map((s: MetaAgentSession) => ({
      ...s,
      createdAt: new Date(s.createdAt),
      updatedAt: new Date(s.updatedAt),
      messages: s.messages.map((m: MetaAgentMessage) => ({
        ...m,
        timestamp: new Date(m.timestamp),
      })),
    }))
  } catch {
    return []
  }
}

function saveSessions(sessions: MetaAgentSession[]) {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(sessions))
  } catch {
    // Storage full / disabled — silently drop.
  }
}

function getContextLabel(
  qa: QADetailResponse | null,
  sessionFile: string | null,
  project: string | null,
): string {
  if (qa) {
    const preview = qa.question_text.substring(0, 40)
    return preview.length < qa.question_text.length ? `${preview}...` : preview
  }
  if (sessionFile) return sessionFile.replace('.jsonl', '').substring(0, 30)
  if (project) return project
  return 'General'
}

export default function MetaAgentInterface({
  currentQA,
  currentSessionFile,
  selectedProject,
}: MetaAgentInterfaceProps) {
  const [sessions, setSessions] = useState<MetaAgentSession[]>(loadSessions)
  const [currentSessionId, setCurrentSessionId] = useState<string | null>(null)
  const [isGenerating, setIsGenerating] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [currentModel, setCurrentModel] = useState('')
  const [models, setModels] = useState<OllamaModel[]>([])
  const [modelsLoaded, setModelsLoaded] = useState(false)

  const abortControllerRef = useRef<AbortController | null>(null)

  const currentSession = sessions.find((s) => s.id === currentSessionId) || null
  const messages = currentSession?.messages || []

  const loadModels = useCallback(async () => {
    try {
      const modelList = await ollamaApi.listModels()
      setModels(modelList)
      setModelsLoaded(true)
      if (modelList.length > 0 && !currentModel && modelList[0]) {
        setCurrentModel(modelList[0].name)
      }
    } catch {
      setModelsLoaded(true)
    }
  }, [currentModel])

  useEffect(() => {
    if (!modelsLoaded) {
      const timer = setTimeout(loadModels, 1000)
      return () => clearTimeout(timer)
    }
  }, [modelsLoaded, loadModels])

  useEffect(() => {
    saveSessions(sessions)
  }, [sessions])

  const createNewSession = useCallback(() => {
    const label = getContextLabel(currentQA, currentSessionFile, selectedProject)
    const newSession: MetaAgentSession = {
      id: generateId(),
      contextLabel: label,
      createdAt: new Date(),
      updatedAt: new Date(),
      messages: [],
      projectSlug: selectedProject,
    }
    setSessions((prev) => [newSession, ...prev])
    setCurrentSessionId(newSession.id)
    return newSession.id
  }, [currentQA, currentSessionFile, selectedProject])

  const handleSend = useCallback(
    async (content: string) => {
      if (!currentModel) {
        setError('No model selected')
        return
      }
      setError(null)

      let sessionId = currentSessionId
      if (!sessionId) sessionId = createNewSession()

      const userMessage: MetaAgentMessage = {
        id: generateId(),
        role: 'user',
        content,
        timestamp: new Date(),
      }
      const assistantId = generateId()
      const assistantMessage: MetaAgentMessage = {
        id: assistantId,
        role: 'assistant',
        content: '',
        timestamp: new Date(),
        toolCalls: [],
      }

      setSessions((prev) =>
        prev.map((s) =>
          s.id === sessionId
            ? {
                ...s,
                messages: [...s.messages, userMessage, assistantMessage],
                updatedAt: new Date(),
              }
            : s,
        ),
      )

      // Build the message history we send upstream — drop our internal
      // ``toolCalls`` field (the backend tracks its own copy) and strip
      // the placeholder assistant turn.
      const session = sessions.find((s) => s.id === sessionId)
      const baseMessages = (session?.messages || []).map((m) => ({
        role: m.role,
        content: m.content,
      }))
      const upstream: Array<{ role: string; content: string }> = [
        ...baseMessages,
        { role: 'user', content },
      ]

      setIsGenerating(true)
      const controller = new AbortController()
      abortControllerRef.current = controller

      try {
        await metaAgentApi.chat(
          {
            messages: upstream,
            model: currentModel,
            tools_enabled: true,
            project_slug: selectedProject,
          },
          (event: MetaAgentEvent) => {
            setSessions((prev) =>
              prev.map((s) => {
                if (s.id !== sessionId) return s
                const next = [...s.messages]
                const idx = next.findIndex((m) => m.id === assistantId)
                if (idx < 0) return s
                const current = next[idx]
                if (!current) return s
                if (event.type === 'token') {
                  next[idx] = { ...current, content: current.content + event.delta }
                } else if (event.type === 'tool_call') {
                  const invocation: MetaAgentToolInvocation = {
                    id: event.id,
                    name: event.name,
                    args: event.args,
                  }
                  next[idx] = {
                    ...current,
                    toolCalls: [...(current.toolCalls || []), invocation],
                  }
                } else if (event.type === 'tool_result') {
                  const updated = (current.toolCalls || []).map((tc) =>
                    tc.id === event.id
                      ? {
                          ...tc,
                          result: {
                            ok: event.ok,
                            data: event.data,
                            duration_ms: event.duration_ms,
                          },
                        }
                      : tc,
                  )
                  next[idx] = { ...current, toolCalls: updated }
                } else if (event.type === 'error') {
                  next[idx] = { ...current, error: event.message }
                }
                // ``done`` is a terminal event; nothing per-message to do.
                return { ...s, messages: next, updatedAt: new Date() }
              }),
            )
          },
          controller.signal,
        )
      } catch (err) {
        if (err instanceof Error && err.name !== 'AbortError') {
          setError(err.message)
        }
      } finally {
        setIsGenerating(false)
        abortControllerRef.current = null
      }
    },
    [currentModel, currentSessionId, sessions, selectedProject, createNewSession],
  )

  const handleStop = useCallback(() => {
    abortControllerRef.current?.abort()
  }, [])

  const handleDeleteSession = useCallback(
    (id: string) => {
      setSessions((prev) => prev.filter((s) => s.id !== id))
      if (currentSessionId === id) setCurrentSessionId(null)
    },
    [currentSessionId],
  )

  const sessionSummaries = sessions.map((s) => ({
    id: s.id,
    contextLabel: s.contextLabel,
    updatedAt: s.updatedAt,
  }))

  const toolsLikelyWork = modelLikelySupportsTools(currentModel)

  return (
    <div className="h-full flex flex-col bg-white dark:bg-gray-950">
      <ModelSelector
        models={models}
        currentModel={currentModel}
        onSelectModel={setCurrentModel}
        onRefresh={loadModels}
      />
      <ChatSessionManager
        sessions={sessionSummaries}
        currentSessionId={currentSessionId}
        onSwitch={setCurrentSessionId}
        onNew={createNewSession}
        onDelete={handleDeleteSession}
      />
      {!toolsLikelyWork && currentModel && (
        <div
          data-testid="meta-agent-tools-warning"
          className="mx-3 mt-2 px-2 py-1.5 bg-amber-50 dark:bg-amber-900/20 border border-amber-300 dark:border-amber-800 rounded text-[11px] text-amber-800 dark:text-amber-300 flex items-center gap-1.5"
        >
          <IconAlertTriangle size={12} className="shrink-0" />
          <span>
            Tool-calling may not work with <span className="font-mono">{currentModel}</span> — pick a
            tools-capable model (qwen2.5-coder, llama3.2…)
          </span>
        </div>
      )}
      {error && (
        <div className="mx-3 mt-2 px-3 py-2 bg-red-100 dark:bg-red-900/20 border border-red-300 dark:border-red-800 rounded text-xs text-red-700 dark:text-red-400">
          {error}
        </div>
      )}
      <MetaAgentMessageList messages={messages} />
      <ChatInput
        onSend={handleSend}
        isGenerating={isGenerating}
        onStop={handleStop}
        disabled={!currentModel}
      />
    </div>
  )
}
