import { useState, useMemo } from 'react'
import { useQuery } from '@tanstack/react-query'
import { IconLoader2 } from '@tabler/icons-react'
import type { DashboardData, Message } from '../../types/api'
import type { Column } from '../common/DataTable'
import DataTable from '../common/DataTable'
import Badge from '../common/Badge'
import Modal from '../common/Modal'
import Markdown from '../common/Markdown'
import { getMessages } from '../../services/api'

interface MessagesTabProps {
  data: DashboardData
  projectName: string
}

type MessageType = 'all' | 'user' | 'assistant' | 'tool_use' | 'tool_result'

const TYPE_COLORS: Record<string, 'blue' | 'green' | 'purple' | 'yellow' | 'gray' | 'red'> = {
  user: 'blue',
  assistant: 'green',
  tool_use: 'purple',
  tool_result: 'yellow',
  system: 'gray',
  error: 'red',
}

function getTypeColor(type: string): 'blue' | 'green' | 'purple' | 'yellow' | 'gray' | 'red' {
  return TYPE_COLORS[type] ?? 'gray'
}

function formatTimestamp(ts: string): string {
  try {
    const d = new Date(ts)
    return d.toLocaleString(undefined, {
      month: 'short',
      day: 'numeric',
      hour: '2-digit',
      minute: '2-digit',
      second: '2-digit',
    })
  } catch {
    return ts
  }
}

function truncateSessionId(id: string): string {
  if (id.length <= 12) return id
  return id.slice(0, 8) + '...'
}

function isErrorMessage(msg: Message): boolean {
  return (
    msg.error ||
    msg.type === 'error' ||
    msg.content.toLowerCase().includes('error') ||
    msg.content.toLowerCase().includes('traceback') ||
    msg.content.toLowerCase().includes('exception')
  )
}

function escapeCsvField(value: string): string {
  if (value.includes(',') || value.includes('"') || value.includes('\n')) {
    return `"${value.replace(/"/g, '""')}"`
  }
  return value
}

export default function MessagesTab({ data, projectName }: MessagesTabProps) {
  const [typeFilter, setTypeFilter] = useState<MessageType>('all')
  const [errorsOnly, setErrorsOnly] = useState(false)
  const [selectedMessage, setSelectedMessage] = useState<Message | null>(null)

  // Initial messages from dashboard data
  const initialMessages = data.messages_page?.messages ?? []

  // Load ALL messages from the API
  const { data: allMessages, isLoading, error } = useQuery({
    queryKey: ['allMessages', projectName],
    queryFn: () => getMessages(),
    // Fall back to dashboard messages while loading
    placeholderData: initialMessages,
  })

  const messages = allMessages ?? initialMessages

  // Apply filters
  const filteredMessages = useMemo(() => {
    let result = messages
    if (typeFilter !== 'all') {
      result = result.filter(m => m.type === typeFilter)
    }
    if (errorsOnly) {
      result = result.filter(isErrorMessage)
    }
    return result
  }, [messages, typeFilter, errorsOnly])

  const columns: Column<Message>[] = useMemo(() => [
    {
      key: 'type',
      label: 'Type',
      width: '100px',
      render: (row) => (
        <Badge color={getTypeColor(row.type)} size="sm">
          {row.type}
        </Badge>
      ),
      sortValue: (row) => row.type,
    },
    {
      key: 'content',
      label: 'Content',
      render: (row) => (
        <span className="text-gray-300 text-xs truncate block max-w-lg">
          {row.content.length > 150
            ? row.content.slice(0, 150) + '...'
            : row.content}
        </span>
      ),
    },
    {
      key: 'timestamp',
      label: 'Timestamp',
      width: '160px',
      render: (row) => (
        <span className="text-gray-400 text-xs whitespace-nowrap">
          {formatTimestamp(row.timestamp)}
        </span>
      ),
      sortValue: (row) => new Date(row.timestamp).getTime(),
    },
    {
      key: 'model',
      label: 'Model',
      width: '140px',
      render: (row) => (
        row.model
          ? <span className="text-gray-400 text-xs font-mono">{row.model}</span>
          : <span className="text-gray-600 text-xs">-</span>
      ),
    },
    {
      key: 'session',
      label: 'Session',
      width: '100px',
      render: (row) => (
        <span
          className="text-gray-500 text-xs font-mono"
          title={row.session_id}
        >
          {truncateSessionId(row.session_id)}
        </span>
      ),
    },
    {
      key: 'tools',
      label: 'Tools',
      width: '160px',
      render: (row) => (
        row.tools && row.tools.length > 0 ? (
          <div className="flex flex-wrap gap-1">
            {row.tools.map(tool => {
              const name = typeof tool === 'string' ? tool : tool.name
              return <Badge key={name} color="purple" size="sm">{name}</Badge>
            })}
          </div>
        ) : (
          <span className="text-gray-600 text-xs">-</span>
        )
      ),
    },
  ], [])

  const handleRowClick = (row: Message) => {
    setSelectedMessage(row)
  }

  const exportCsv = (rows: Message[]): string => {
    const headers = ['Type', 'Content', 'Timestamp', 'Model', 'Session', 'Tools']
    const lines = [headers.join(',')]
    for (const row of rows) {
      lines.push([
        escapeCsvField(row.type),
        escapeCsvField(row.content),
        escapeCsvField(row.timestamp),
        escapeCsvField(row.model ?? ''),
        escapeCsvField(row.session_id),
        escapeCsvField((row.tools ?? []).map(t => typeof t === 'string' ? t : t.name).join('; ')),
      ].join(','))
    }
    return lines.join('\n')
  }

  // Count message types for filter labels
  const typeCounts = useMemo(() => {
    const counts: Record<string, number> = {}
    for (const m of messages) {
      counts[m.type] = (counts[m.type] ?? 0) + 1
    }
    return counts
  }, [messages])

  return (
    <div className="space-y-3">
      <div className="flex items-center justify-between">
        <h2 className="text-sm font-semibold text-gray-200">
          All Messages
          <span className="ml-2 text-xs text-gray-500 font-normal">
            {isLoading ? (
              <IconLoader2 size={12} className="inline animate-spin" />
            ) : (
              <>{filteredMessages.length.toLocaleString()} of {messages.length.toLocaleString()}</>
            )}
          </span>
        </h2>
      </div>

      {/* Filter controls */}
      <div className="flex items-center gap-3 flex-wrap">
        <div className="flex items-center gap-2">
          <label className="text-xs text-gray-400">Type:</label>
          <select
            value={typeFilter}
            onChange={e => setTypeFilter(e.target.value as MessageType)}
            className="bg-gray-800 border border-gray-700 rounded px-2 py-1.5 text-xs text-gray-300 focus:outline-none focus:border-indigo-500"
          >
            <option value="all">All ({messages.length})</option>
            <option value="user">user ({typeCounts.user ?? 0})</option>
            <option value="assistant">assistant ({typeCounts.assistant ?? 0})</option>
            <option value="tool_use">tool_use ({typeCounts.tool_use ?? 0})</option>
            <option value="tool_result">tool_result ({typeCounts.tool_result ?? 0})</option>
          </select>
        </div>

        <label className="flex items-center gap-1.5 text-xs text-gray-400 cursor-pointer select-none">
          <input
            type="checkbox"
            checked={errorsOnly}
            onChange={e => setErrorsOnly(e.target.checked)}
            className="rounded border-gray-600 bg-gray-800 text-indigo-500 focus:ring-indigo-500 focus:ring-offset-0"
          />
          Errors only
        </label>
      </div>

      {error && (
        <div className="bg-red-900/20 border border-red-800 rounded-lg p-3 text-red-400 text-xs">
          Failed to load all messages: {error instanceof Error ? error.message : 'Unknown error'}. Showing initial batch.
        </div>
      )}

      <DataTable
        columns={columns}
        data={filteredMessages}
        keyFn={(row) => row.message_id || (row.session_id + '_' + row.timestamp + '_' + row.type)}
        searchable
        searchPlaceholder="Search message content..."
        searchFn={(row, query) =>
          row.content.toLowerCase().includes(query) ||
          row.type.toLowerCase().includes(query) ||
          (row.model?.toLowerCase().includes(query) ?? false) ||
          row.session_id.toLowerCase().includes(query)
        }
        onRowClick={handleRowClick}
        perPageOptions={[25, 50, 100, 200]}
        defaultPerPage={25}
        exportFilename="messages.csv"
        exportFn={exportCsv}
        emptyMessage="No messages match the current filters"
      />

      {/* Message detail modal */}
      <Modal
        isOpen={!!selectedMessage}
        onClose={() => setSelectedMessage(null)}
        title={selectedMessage ? `${selectedMessage.type} message` : ''}
      >
        {selectedMessage && (
          <div className="space-y-3 max-h-[70vh] overflow-y-auto">
            {/* Metadata */}
            <div className="flex flex-wrap items-center gap-2">
              <Badge color={getTypeColor(selectedMessage.type)} size="md">
                {selectedMessage.type}
              </Badge>
              {selectedMessage.model && (
                <span className="text-xs text-gray-400 font-mono bg-gray-800 px-2 py-0.5 rounded">
                  {selectedMessage.model}
                </span>
              )}
              <span className="text-xs text-gray-500">
                {formatTimestamp(selectedMessage.timestamp)}
              </span>
            </div>

            {/* Session info */}
            <div className="text-xs text-gray-500">
              Session: <span className="font-mono text-gray-400">{selectedMessage.session_id}</span>
            </div>

            {/* Tokens */}
            {selectedMessage.tokens && (
              <div className="text-xs text-gray-500">
                Tokens: in={selectedMessage.tokens.input} out={selectedMessage.tokens.output} cache_read={selectedMessage.tokens.cache_read} cache_create={selectedMessage.tokens.cache_creation}
              </div>
            )}

            {/* Tools */}
            {selectedMessage.tools && selectedMessage.tools.length > 0 && (
              <div className="flex flex-wrap gap-1">
                <span className="text-xs text-gray-500 mr-1">Tools:</span>
                {selectedMessage.tools.map(tool => {
                  const name = typeof tool === 'string' ? tool : tool.name
                  return <Badge key={name} color="purple" size="sm">{name}</Badge>
                })}
              </div>
            )}

            {/* Content */}
            <div className="border-t border-gray-700 pt-3">
              {selectedMessage.type === 'assistant' ? (
                <Markdown content={selectedMessage.content} />
              ) : (
                <pre className="text-gray-300 whitespace-pre-wrap break-words font-mono text-xs leading-relaxed">
                  {selectedMessage.content}
                </pre>
              )}
            </div>
          </div>
        )}
      </Modal>
    </div>
  )
}
