import { useState, useEffect, useCallback, useRef } from 'react'
import { ollamaApi } from '../../services/ollama'
import type { ChatMessage, ChatSession, OllamaModel, OllamaChatMessage } from '../../types/chat'
import type { QADetailResponse } from '../../types/api'
import ModelSelector from './ModelSelector'
import ChatMessageList from './ChatMessageList'
import ChatInput from './ChatInput'
import ChatSessionManager from './ChatSessionManager'

interface ChatInterfaceProps {
  currentQA: QADetailResponse | null
  currentSessionFile: string | null
  selectedProject: string | null
}

const STORAGE_KEY = 'stackunderflow_chatSessions'

function generateId(): string {
  return `${Date.now()}-${Math.random().toString(36).substring(2, 9)}`
}

function loadSessions(): ChatSession[] {
  try {
    const raw = localStorage.getItem(STORAGE_KEY)
    if (!raw) return []
    const parsed = JSON.parse(raw)
    return parsed.map((s: ChatSession) => ({
      ...s,
      createdAt: new Date(s.createdAt),
      updatedAt: new Date(s.updatedAt),
      messages: s.messages.map((m: ChatMessage) => ({
        ...m,
        timestamp: new Date(m.timestamp),
      })),
    }))
  } catch {
    return []
  }
}

function saveSessions(sessions: ChatSession[]) {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(sessions))
  } catch {
    // Storage full or unavailable
  }
}

function buildSystemPrompt(
  qa: QADetailResponse | null,
  sessionFile: string | null,
  project: string | null
): string {
  const parts: string[] = [
    'You are a helpful assistant for the StackUnderflow code exploration tool. You help users understand code sessions, Q&A pairs, and development patterns.',
  ]

  if (project) {
    parts.push(`The user is currently browsing the project: ${project}`)
  }

  if (qa) {
    parts.push(
      `The user is viewing a Q&A pair:\n` +
      `Question: ${qa.question_text}\n` +
      `Answer: ${qa.answer_text}\n` +
      `Tools: ${qa.tools_used.join(', ')}\n` +
      `Model: ${qa.model || 'unknown'}`
    )
  }

  if (sessionFile) {
    parts.push(`The current session file is: ${sessionFile}`)
  }

  return parts.join('\n\n')
}

function getContextLabel(
  qa: QADetailResponse | null,
  sessionFile: string | null,
  project: string | null
): string {
  if (qa) {
    const preview = qa.question_text.substring(0, 40)
    return preview.length < qa.question_text.length ? `${preview}...` : preview
  }
  if (sessionFile) return sessionFile.replace('.jsonl', '').substring(0, 30)
  if (project) return project
  return 'General'
}

export default function ChatInterface({ currentQA, currentSessionFile, selectedProject }: ChatInterfaceProps) {
  const [sessions, setSessions] = useState<ChatSession[]>(loadSessions)
  const [currentSessionId, setCurrentSessionId] = useState<string | null>(null)
  const [isGenerating, setIsGenerating] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [currentModel, setCurrentModel] = useState('')
  const [models, setModels] = useState<OllamaModel[]>([])

  const abortControllerRef = useRef<AbortController | null>(null)

  // Current session messages
  const currentSession = sessions.find((s) => s.id === currentSessionId) || null
  const messages = currentSession?.messages || []

  const [modelsLoaded, setModelsLoaded] = useState(false)

  // Load models lazily — only when user hasn't loaded yet
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
      // Ollama not running — not an error, it's optional
    }
  }, [currentModel])

  // Don't auto-load on mount — wait for user interaction
  useEffect(() => {
    if (!modelsLoaded) {
      // Defer the load so it doesn't spam on initial render
      const timer = setTimeout(loadModels, 2000)
      return () => clearTimeout(timer)
    }
  }, [modelsLoaded, loadModels])

  // Persist sessions
  useEffect(() => {
    saveSessions(sessions)
  }, [sessions])

  const createNewSession = useCallback(() => {
    const label = getContextLabel(currentQA, currentSessionFile, selectedProject)
    const contextId = currentQA?.id || currentSessionFile || selectedProject || 'general'
    const newSession: ChatSession = {
      id: generateId(),
      contextId,
      contextLabel: label,
      createdAt: new Date(),
      updatedAt: new Date(),
      messages: [],
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

      // Ensure we have a session
      let sessionId = currentSessionId
      if (!sessionId) {
        sessionId = createNewSession()
      }

      // Add user message
      const userMessage: ChatMessage = {
        id: generateId(),
        role: 'user',
        content,
        timestamp: new Date(),
      }

      setSessions((prev) =>
        prev.map((s) =>
          s.id === sessionId
            ? { ...s, messages: [...s.messages, userMessage], updatedAt: new Date() }
            : s
        )
      )

      // Build messages for Ollama
      const systemPrompt = buildSystemPrompt(currentQA, currentSessionFile, selectedProject)
      const ollamaMessages: OllamaChatMessage[] = [
        { role: 'system', content: systemPrompt },
      ]

      // Get the updated session messages
      const existingSession = sessions.find((s) => s.id === sessionId)
      const existingMessages = existingSession?.messages || []
      for (const msg of existingMessages) {
        ollamaMessages.push({ role: msg.role, content: msg.content })
      }
      ollamaMessages.push({ role: 'user', content })

      // Create assistant message placeholder
      const assistantMessageId = generateId()
      const assistantMessage: ChatMessage = {
        id: assistantMessageId,
        role: 'assistant',
        content: '',
        timestamp: new Date(),
      }

      setSessions((prev) =>
        prev.map((s) => {
          if (s.id !== sessionId) return s
          // Only add assistant placeholder; user message was already added above
          const updated = [...s.messages]
          if (!updated.some((m) => m.id === assistantMessageId)) {
            updated.push(assistantMessage)
          }
          return { ...s, messages: updated, updatedAt: new Date() }
        })
      )

      setIsGenerating(true)
      const controller = new AbortController()
      abortControllerRef.current = controller

      try {
        await ollamaApi.chat(
          { model: currentModel, messages: ollamaMessages, stream: true },
          (chunk) => {
            if (!chunk.done && chunk.message?.content) {
              setSessions((prev) =>
                prev.map((s) =>
                  s.id === sessionId
                    ? {
                        ...s,
                        messages: s.messages.map((m) =>
                          m.id === assistantMessageId
                            ? { ...m, content: m.content + chunk.message.content }
                            : m
                        ),
                        updatedAt: new Date(),
                      }
                    : s
                )
              )
            }
          },
          controller.signal
        )
      } catch (err) {
        if (err instanceof Error && err.name === 'AbortError') {
          // User cancelled
        } else {
          setError(err instanceof Error ? err.message : 'Chat request failed')
        }
      } finally {
        setIsGenerating(false)
        abortControllerRef.current = null
      }
    },
    [currentModel, currentSessionId, currentQA, currentSessionFile, selectedProject, sessions, createNewSession]
  )

  const handleStop = useCallback(() => {
    abortControllerRef.current?.abort()
  }, [])

  const handleDeleteSession = useCallback(
    (id: string) => {
      setSessions((prev) => prev.filter((s) => s.id !== id))
      if (currentSessionId === id) {
        setCurrentSessionId(null)
      }
    },
    [currentSessionId]
  )

  const sessionSummaries = sessions.map((s) => ({
    id: s.id,
    contextLabel: s.contextLabel,
    updatedAt: s.updatedAt,
  }))

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
      {error && (
        <div className="mx-3 mt-2 px-3 py-2 bg-red-100 dark:bg-red-900/20 border border-red-300 dark:border-red-800 rounded text-xs text-red-700 dark:text-red-400">
          {error}
        </div>
      )}
      <ChatMessageList messages={messages} />
      <ChatInput
        onSend={handleSend}
        isGenerating={isGenerating}
        onStop={handleStop}
        disabled={!currentModel}
      />
    </div>
  )
}
