import { useState, useRef, useCallback, KeyboardEvent } from 'react'
import { IconSend, IconPlayerStop } from '@tabler/icons-react'

interface ChatInputProps {
  onSend: (content: string) => void
  isGenerating: boolean
  onStop: () => void
  disabled: boolean
}

export default function ChatInput({ onSend, isGenerating, onStop, disabled }: ChatInputProps) {
  const [input, setInput] = useState('')
  const textareaRef = useRef<HTMLTextAreaElement>(null)

  const handleSend = useCallback(() => {
    const trimmed = input.trim()
    if (!trimmed || disabled) return
    onSend(trimmed)
    setInput('')
    if (textareaRef.current) {
      textareaRef.current.style.height = 'auto'
    }
  }, [input, disabled, onSend])

  const handleKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault()
      handleSend()
    }
  }

  const handleInput = (e: React.ChangeEvent<HTMLTextAreaElement>) => {
    setInput(e.target.value)
    const target = e.target
    target.style.height = 'auto'
    target.style.height = `${Math.min(target.scrollHeight, 120)}px`
  }

  return (
    <div className="border-t border-gray-200 dark:border-gray-800 p-2">
      <div className="flex items-end gap-2">
        <textarea
          ref={textareaRef}
          value={input}
          onChange={handleInput}
          onKeyDown={handleKeyDown}
          placeholder={disabled ? 'Select a model to start chatting...' : 'Ask a question...'}
          disabled={disabled || isGenerating}
          rows={1}
          className="flex-1 bg-white dark:bg-gray-800 text-sm text-gray-800 dark:text-gray-200 border border-gray-200 dark:border-gray-700 rounded-lg px-3 py-2 resize-none focus:outline-none focus:border-blue-500 placeholder-gray-500 dark:placeholder-gray-600 disabled:opacity-50"
        />
        {isGenerating ? (
          <button
            onClick={onStop}
            className="p-2 bg-red-600 hover:bg-red-700 text-white rounded-lg transition-colors shrink-0"
            title="Stop generating"
          >
            <IconPlayerStop size={18} />
          </button>
        ) : (
          <button
            onClick={handleSend}
            disabled={disabled || !input.trim()}
            className="p-2 bg-blue-600 hover:bg-blue-700 disabled:bg-gray-300 dark:disabled:bg-gray-700 disabled:text-gray-500 text-white rounded-lg transition-colors shrink-0"
            title="Send message"
          >
            <IconSend size={18} />
          </button>
        )}
      </div>
    </div>
  )
}
