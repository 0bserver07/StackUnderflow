import { useState, useMemo } from 'react'
import { IconChevronDown, IconChevronRight } from '@tabler/icons-react'
import type { DashboardData, Message } from '../../types/api'
import type { Column } from '../common/DataTable'
import DataTable from '../common/DataTable'
import Badge from '../common/Badge'

interface CommandsTabProps {
  data: DashboardData
}

interface CommandRow {
  index: number
  groupKey: string
  command: Message
  assistantMessage?: Message
  toolsUsed: string[]
  model: string | null
  timestamp: string
}

function extractCommands(messages: Message[]): CommandRow[] {
  // Group consecutive messages by session_id, then pick user messages as commands
  const commands: CommandRow[] = []
  let idx = 1

  for (let i = 0; i < messages.length; i++) {
    const msg = messages[i]!
    if (msg.type !== 'user') continue

    // Look ahead for the assistant response in the same session
    let assistantMsg: Message | undefined
    const toolsSet = new Set<string>()

    for (let j = i + 1; j < messages.length; j++) {
      const next = messages[j]!
      if (next.session_id !== msg.session_id) break
      if (next.type === 'user') break // next user command
      if (next.type === 'assistant' && !assistantMsg) {
        assistantMsg = next
      }
      for (const tool of (next.tools ?? [])) {
        toolsSet.add(typeof tool === 'string' ? tool : tool.name)
      }
    }

    const groupKey = msg.uuid || `${msg.session_id}_${msg.timestamp}_${idx}`

    commands.push({
      index: idx++,
      groupKey,
      command: msg,
      assistantMessage: assistantMsg,
      toolsUsed: Array.from(toolsSet),
      model: assistantMsg?.model ?? msg.model,
      timestamp: msg.timestamp,
    })
  }

  return commands
}

function formatTimestamp(ts: string): string {
  try {
    const d = new Date(ts)
    return d.toLocaleString(undefined, {
      month: 'short',
      day: 'numeric',
      hour: '2-digit',
      minute: '2-digit',
    })
  } catch {
    return ts
  }
}

function escapeCsvField(value: string): string {
  if (value.includes(',') || value.includes('"') || value.includes('\n')) {
    return `"${value.replace(/"/g, '""')}"`
  }
  return value
}

export default function CommandsTab({ data }: CommandsTabProps) {
  const [expandedId, setExpandedId] = useState<string | null>(null)

  // messages_page is { messages: Message[], ... } — extract the array
  const messages = data.messages_page?.messages ?? []
  const commands = useMemo(() => extractCommands(messages), [messages])

  const columns: Column<CommandRow>[] = useMemo(() => [
    {
      key: 'index',
      label: '#',
      width: '50px',
      align: 'right',
      render: (row) => (
        <span className="text-gray-500 text-xs">{row.index}</span>
      ),
      sortValue: (row) => row.index,
    },
    {
      key: 'command',
      label: 'Command',
      render: (row) => (
        <div className="flex items-start gap-1.5 min-w-0">
          {expandedId === row.groupKey ? (
            <IconChevronDown size={14} className="text-gray-500 mt-0.5 shrink-0" />
          ) : (
            <IconChevronRight size={14} className="text-gray-500 mt-0.5 shrink-0" />
          )}
          <span className="text-gray-700 dark:text-gray-300 whitespace-pre-wrap break-words min-w-0">
            {row.command.content.length > 400
              ? row.command.content.slice(0, 400) + '…'
              : row.command.content}
          </span>
        </div>
      ),
    },
    {
      key: 'timestamp',
      label: 'Timestamp',
      width: '140px',
      render: (row) => (
        <span className="text-gray-600 dark:text-gray-400 text-xs whitespace-nowrap">
          {formatTimestamp(row.timestamp)}
        </span>
      ),
      sortValue: (row) => new Date(row.timestamp).getTime(),
    },
    {
      key: 'model',
      label: 'Model',
      width: '160px',
      render: (row) => (
        row.model
          ? <span className="text-gray-600 dark:text-gray-400 text-xs font-mono">{row.model}</span>
          : <span className="text-gray-500 text-xs">-</span>
      ),
    },
    {
      key: 'tools',
      label: 'Tools Used',
      width: '200px',
      render: (row) => (
        row.toolsUsed.length > 0 ? (
          <div className="flex flex-wrap gap-1">
            {row.toolsUsed.map(tool => (
              <Badge key={tool} color="purple" size="sm">{tool}</Badge>
            ))}
          </div>
        ) : (
          <span className="text-gray-500 text-xs">-</span>
        )
      ),
      sortValue: (row) => row.toolsUsed.length,
    },
    {
      key: 'tokens',
      label: 'Tokens',
      width: '80px',
      align: 'right',
      render: (row) => {
        // Show per-message token count if available
        const t = row.command.tokens
        if (t) {
          const total = t.input + t.output
          return (
            <span className="text-gray-600 dark:text-gray-400 text-xs" title={`In: ${t.input} / Out: ${t.output}`}>
              {total.toLocaleString()}
            </span>
          )
        }
        return <span className="text-gray-500 text-xs">-</span>
      },
    },
  ], [expandedId])

  const handleRowClick = (row: CommandRow) => {
    setExpandedId(prev => prev === row.groupKey ? null : row.groupKey)
  }

  const exportCsv = (rows: CommandRow[]): string => {
    const headers = ['#', 'Command', 'Timestamp', 'Model', 'Tools Used']
    const lines = [headers.join(',')]
    for (const row of rows) {
      lines.push([
        String(row.index),
        escapeCsvField(row.command.content),
        escapeCsvField(row.timestamp),
        escapeCsvField(row.model ?? ''),
        escapeCsvField(row.toolsUsed.join('; ')),
      ].join(','))
    }
    return lines.join('\n')
  }

  return (
    <div className="space-y-2">
      <div className="flex items-center justify-between">
        <h2 className="text-sm font-semibold text-gray-800 dark:text-gray-200">
          User Commands
          <span className="ml-2 text-xs text-gray-500 font-normal">
            {commands.length} command{commands.length !== 1 ? 's' : ''}
          </span>
        </h2>
      </div>

      <DataTable
        columns={columns}
        data={commands}
        keyFn={(row) => row.groupKey}
        searchable
        searchPlaceholder="Filter commands..."
        searchFn={(row, query) =>
          row.command.content.toLowerCase().includes(query) ||
          (row.model?.toLowerCase().includes(query) ?? false) ||
          row.toolsUsed.some(t => t.toLowerCase().includes(query))
        }
        onRowClick={handleRowClick}
        perPageOptions={[25, 50, 100, 200]}
        defaultPerPage={25}
        exportFilename="commands.csv"
        exportFn={exportCsv}
        emptyMessage="No user commands found"
      />

      {/* Expanded command detail */}
      {expandedId && (() => {
        const row = commands.find(c => c.groupKey === expandedId)
        if (!row) return null
        return (
          <div className="bg-gray-100/70 dark:bg-gray-800/50 border border-gray-200 dark:border-gray-700 rounded-lg p-4 text-sm">
            <div className="flex items-center justify-between mb-2">
              <span className="text-xs text-gray-600 dark:text-gray-400 font-medium uppercase tracking-wider">
                Full Command Text
              </span>
              <button
                onClick={() => setExpandedId(null)}
                className="text-xs text-gray-500 hover:text-gray-700 dark:hover:text-gray-300"
              >
                Close
              </button>
            </div>
            <pre className="text-gray-700 dark:text-gray-300 whitespace-pre-wrap break-words font-mono text-xs leading-relaxed max-h-96 overflow-y-auto">
              {row.command.content}
            </pre>
            <div className="mt-3 pt-3 border-t border-gray-200 dark:border-gray-700">
              <span className="text-xs text-gray-500">
                Session: {row.command.session_id}
                {row.model && <> | Model: {row.model}</>}
                {row.toolsUsed.length > 0 && <> | Tools: {row.toolsUsed.join(', ')}</>}
              </span>
            </div>
          </div>
        )
      })()}
    </div>
  )
}
