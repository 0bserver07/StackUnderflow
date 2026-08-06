// Meta-agent variant of ChatMessageList — renders each assistant
// message's ``toolCalls`` array as inline ``ToolCallSurface`` blocks
// above the message bubble. User messages render unchanged.
//
// Kept separate from ``ChatMessageList`` so the legacy plain-Ollama
// chat path stays binary-compatible with its existing message type.

import { useEffect, useRef, lazy, Suspense } from 'react'
import type { MetaAgentMessage } from '../../types/metaAgent'
import ToolCallSurface from './ToolCallSurface'

// Markdown statically pulls in react-markdown + react-syntax-highlighter
// (a large chunk). The meta-agent sidebar is mounted on every route, so a
// static import would pin those libs into the eager first-paint bundle.
// Lazy-load it instead: the chunk fetches only when an assistant message
// actually renders, and the Suspense fallback shows the raw text so
// streaming output stays visible while the chunk loads (it resolves once,
// then React caches it — no flicker on subsequent token updates).
const Markdown = lazy(() => import('../common/Markdown'))

interface MetaAgentMessageListProps {
  messages: MetaAgentMessage[]
}

function formatTime(date: Date): string {
  return new Date(date).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })
}

export default function MetaAgentMessageList({ messages }: MetaAgentMessageListProps) {
  const bottomRef = useRef<HTMLDivElement>(null)

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: 'smooth' })
  }, [messages])

  if (messages.length === 0) {
    return (
      <div className="flex-1 flex items-center justify-center p-4">
        <div className="text-center text-gray-500">
          <p className="text-sm">Ask about your sessions, costs, or projects.</p>
          <p className="text-xs mt-1 leading-relaxed">
            The meta agent reads from your local StackUnderflow store —
            nothing leaves the machine.
          </p>
        </div>
      </div>
    )
  }

  return (
    <div className="flex-1 overflow-auto p-3">
      {messages.map((msg) => (
        <MetaAgentMessageBubble key={msg.id} message={msg} />
      ))}
      <div ref={bottomRef} />
    </div>
  )
}

interface MetaAgentMessageBubbleProps {
  message: MetaAgentMessage
}

function MetaAgentMessageBubble({ message }: MetaAgentMessageBubbleProps) {
  const isUser = message.role === 'user'

  if (isUser) {
    return (
      <div className="flex justify-end mb-3">
        <div className="max-w-[85%] rounded-lg px-3 py-2 bg-blue-600 text-white">
          <div className="text-sm break-words">
            <p className="whitespace-pre-wrap">{message.content}</p>
          </div>
          <div className="text-[10px] mt-1 text-blue-200">{formatTime(message.timestamp)}</div>
        </div>
      </div>
    )
  }

  return (
    <div className="flex justify-start mb-3">
      <div className="max-w-[95%] w-full">
        {message.toolCalls && message.toolCalls.length > 0 && (
          <div data-testid="meta-tool-calls" className="mb-1">
            {message.toolCalls.map((tc) => (
              <ToolCallSurface key={tc.id} invocation={tc} />
            ))}
          </div>
        )}
        {(message.content || message.error) && (
          <div className="rounded-lg px-3 py-2 bg-white dark:bg-gray-800 text-gray-800 dark:text-gray-200">
            <div className="text-sm break-words">
              {message.content && (
                <Suspense
                  fallback={<p className="whitespace-pre-wrap">{message.content}</p>}
                >
                  <Markdown content={message.content} />
                </Suspense>
              )}
              {message.error && (
                <p className="text-xs text-red-600 dark:text-red-400 mt-1">
                  Error: {message.error}
                </p>
              )}
            </div>
            <div className="text-[10px] mt-1 text-gray-500">{formatTime(message.timestamp)}</div>
          </div>
        )}
      </div>
    </div>
  )
}
