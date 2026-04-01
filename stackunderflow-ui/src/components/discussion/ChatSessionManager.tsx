import { IconPlus, IconTrash } from '@tabler/icons-react'

interface ChatSessionSummary {
  id: string
  contextLabel: string
  updatedAt: Date
}

interface ChatSessionManagerProps {
  sessions: ChatSessionSummary[]
  currentSessionId: string | null
  onSwitch: (id: string) => void
  onNew: () => void
  onDelete: (id: string) => void
}

function formatDate(date: Date): string {
  return new Date(date).toLocaleDateString([], { month: 'short', day: 'numeric' })
}

export default function ChatSessionManager({
  sessions,
  currentSessionId,
  onSwitch,
  onNew,
  onDelete,
}: ChatSessionManagerProps) {
  return (
    <div className="border-b border-gray-800">
      <div className="flex items-center justify-between px-3 py-1.5">
        <span className="text-[10px] text-gray-500 uppercase tracking-wider">Sessions</span>
        <button
          onClick={onNew}
          className="p-0.5 text-gray-500 hover:text-blue-400 transition-colors"
          title="New chat session"
        >
          <IconPlus size={14} />
        </button>
      </div>
      {sessions.length > 0 && (
        <div className="max-h-24 overflow-auto px-1 pb-1 space-y-0.5">
          {sessions.map((session) => (
            <div
              key={session.id}
              className={`flex items-center gap-1 px-2 py-1 rounded text-xs cursor-pointer group ${
                session.id === currentSessionId
                  ? 'bg-gray-700 text-gray-200'
                  : 'text-gray-400 hover:bg-gray-800 hover:text-gray-300'
              }`}
              onClick={() => onSwitch(session.id)}
            >
              <span className="flex-1 truncate">{session.contextLabel}</span>
              <span className="text-[10px] text-gray-600 shrink-0">{formatDate(session.updatedAt)}</span>
              <button
                onClick={(e) => {
                  e.stopPropagation()
                  onDelete(session.id)
                }}
                className="p-0.5 text-gray-600 hover:text-red-400 opacity-0 group-hover:opacity-100 transition-all shrink-0"
                title="Delete session"
              >
                <IconTrash size={12} />
              </button>
            </div>
          ))}
        </div>
      )}
    </div>
  )
}
