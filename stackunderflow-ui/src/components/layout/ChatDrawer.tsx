import { IconX } from '@tabler/icons-react'
import ChatInterface from '../discussion/ChatInterface'

interface ChatDrawerProps {
  open: boolean
  onClose: () => void
}

export default function ChatDrawer({ open, onClose }: ChatDrawerProps) {
  if (!open) return null

  return (
    <div className="fixed inset-y-0 right-0 w-96 bg-white dark:bg-gray-950 border-l border-gray-200 dark:border-gray-800 shadow-2xl z-40 flex flex-col">
      <div className="flex items-center justify-between px-3 py-2 border-b border-gray-200 dark:border-gray-800">
        <span className="text-sm font-medium text-gray-700 dark:text-gray-300">Ollama Chat</span>
        <button
          onClick={onClose}
          className="p-1 text-gray-500 hover:text-gray-700 dark:hover:text-gray-300 rounded hover:bg-gray-200 dark:hover:bg-gray-800"
        >
          <IconX size={16} />
        </button>
      </div>
      <div className="flex-1 overflow-hidden">
        <ChatInterface currentQA={null} currentSessionFile={null} selectedProject={null} />
      </div>
    </div>
  )
}
