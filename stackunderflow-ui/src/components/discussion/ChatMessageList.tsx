import { useEffect, useRef } from 'react'
import type { ChatMessage } from '../../types/chat'
import ChatMessageBubble from './ChatMessageBubble'

interface ChatMessageListProps {
  messages: ChatMessage[]
}

export default function ChatMessageList({ messages }: ChatMessageListProps) {
  const bottomRef = useRef<HTMLDivElement>(null)

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: 'smooth' })
  }, [messages])

  if (messages.length === 0) {
    return (
      <div className="flex-1 flex items-center justify-center">
        <div className="text-center text-gray-500">
          <p className="text-sm">No messages yet</p>
          <p className="text-xs mt-1">Start a conversation about the current context</p>
        </div>
      </div>
    )
  }

  return (
    <div className="flex-1 overflow-auto p-3">
      {messages.map((msg) => (
        <ChatMessageBubble key={msg.id} message={msg} />
      ))}
      <div ref={bottomRef} />
    </div>
  )
}
