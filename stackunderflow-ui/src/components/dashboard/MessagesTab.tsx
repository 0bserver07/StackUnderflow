import { useState, useMemo, useEffect, useRef, useCallback } from 'react'
import { useQuery } from '@tanstack/react-query'
import { IconLoader2, IconX } from '@tabler/icons-react'
import type { DashboardData, Message } from '../../types/api'
import type { Column } from '../common/DataTable'
import DataTable from '../common/DataTable'
import Badge from '../common/Badge'
import Modal from '../common/Modal'
import Markdown from '../common/Markdown'
import { getMessages } from '../../services/api'
import { getParam, NAV_EVENT, type NavDetail } from '../../services/navigation'

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

// ---------------------------------------------------------------------------
// Deep-link (?interaction=ID) types & helpers — spec §C24
// ---------------------------------------------------------------------------

/** Subset of one Record returned by GET /api/interaction/{id}. */
interface InteractionRecord {
  session_id: string
  kind: string
  timestamp: string
  model: string | null
  content: string
  tokens: Record<string, number>
  tools: unknown[]
  is_error: boolean
  message_id: string
  uuid: string
}

/** Shape of the GET /api/interaction/{interactionId} response. */
interface InteractionResponse {
  interaction_id: string
  session_id: string
  start_time: string
  end_time: string
  model: string | null
  tool_count: number
  assistant_steps: number
  tools_used: unknown[]
  command: InteractionRecord
  responses: InteractionRecord[]
  tool_results: InteractionRecord[]
}

type InteractionPanel =
  | { kind: 'loaded'; interactionId: string; message: Message }
  | { kind: 'fetched'; interactionId: string; data: InteractionResponse }
  | { kind: 'loading'; interactionId: string }
  | { kind: 'error'; interactionId: string; message: string }

/**
 * Try to match an interactionId against an in-memory message. The
 * aggregator-side `interaction_id` is a sha256 prefix derived from the
 * user-prompt's timestamp + content, so it won't equal `message_id` /
 * `uuid` directly — but in practice callers (RetryAlertsPanel,
 * OutlierCommandsTable, etc.) sometimes pass `message_id` or `uuid`
 * straight through. We try both before falling back to a network fetch.
 */
function findInteractionInMessages(messages: Message[], id: string): Message | null {
  for (const m of messages) {
    if (m.message_id === id || m.uuid === id) return m
  }
  return null
}

/** Inline fetch — keeps the change scoped to MessagesTab.tsx only. */
async function fetchInteractionById(id: string): Promise<InteractionResponse> {
  const res = await fetch(`/api/interaction/${encodeURIComponent(id)}`)
  if (!res.ok) {
    const text = await res.text().catch(() => '')
    throw new Error(`${res.status} ${res.statusText}${text ? `: ${text}` : ''}`)
  }
  return res.json()
}

export default function MessagesTab({ data, projectName }: MessagesTabProps) {
  const [typeFilter, setTypeFilter] = useState<MessageType>('all')
  const [errorsOnly, setErrorsOnly] = useState(false)
  const [selectedMessage, setSelectedMessage] = useState<Message | null>(null)

  // Deep-link state for ?interaction=ID — spec §C24
  const [panel, setPanel] = useState<InteractionPanel | null>(null)
  const [pulse, setPulse] = useState(false)
  const panelRef = useRef<HTMLDivElement | null>(null)
  const pulseTimerRef = useRef<number | null>(null)

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

  // ---------------------------------------------------------------------
  // Deep-link handling — read ?interaction= on mount, then react to NAV_EVENT.
  // ---------------------------------------------------------------------

  /** Load (and pulse-highlight) the interaction with the given id. */
  const loadInteraction = useCallback(async (interactionId: string) => {
    if (!interactionId) return
    const localHit = findInteractionInMessages(messages, interactionId)
    if (localHit) {
      setPanel({ kind: 'loaded', interactionId, message: localHit })
    } else {
      setPanel({ kind: 'loading', interactionId })
      try {
        const fetched = await fetchInteractionById(interactionId)
        setPanel({ kind: 'fetched', interactionId, data: fetched })
      } catch (e) {
        setPanel({
          kind: 'error',
          interactionId,
          message: e instanceof Error ? e.message : 'Unknown error',
        })
      }
    }
    // Trigger the 2-second pulse highlight. Re-arm cleanly if a previous
    // pulse is still in flight so back-to-back deep-links don't get stuck.
    if (pulseTimerRef.current !== null) {
      window.clearTimeout(pulseTimerRef.current)
    }
    setPulse(true)
    pulseTimerRef.current = window.setTimeout(() => {
      setPulse(false)
      pulseTimerRef.current = null
    }, 2000)
  }, [messages])

  // Mount: read ?interaction= once messages have been retrieved (so we can
  // attempt the in-memory match before falling back to a network fetch).
  useEffect(() => {
    const id = getParam('interaction')
    if (!id) return
    void loadInteraction(id)
    // Intentionally not in deps: we only want this on mount + once messages
    // are populated. `loadInteraction` already closes over `messages`.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [allMessages])

  // Subsequent navigations: NAV_EVENT fired by openInteraction(...)
  useEffect(() => {
    function onNav(e: Event) {
      const ce = e as CustomEvent<NavDetail>
      const detail = ce.detail
      if (!detail || detail.tab !== 'messages') return
      const id = detail.interaction
      if (!id) return
      void loadInteraction(id)
    }
    window.addEventListener(NAV_EVENT, onNav)
    return () => window.removeEventListener(NAV_EVENT, onNav)
  }, [loadInteraction])

  // Scroll the detail panel into view whenever it appears or changes id.
  useEffect(() => {
    if (!panel) return
    panelRef.current?.scrollIntoView({ behavior: 'smooth', block: 'start' })
  }, [panel?.interactionId, panel?.kind])

  // Tidy timers on unmount.
  useEffect(() => {
    return () => {
      if (pulseTimerRef.current !== null) {
        window.clearTimeout(pulseTimerRef.current)
      }
    }
  }, [])

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
        <span className="text-gray-700 dark:text-gray-300 text-xs truncate block max-w-lg">
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
        <span className="text-gray-600 dark:text-gray-400 text-xs whitespace-nowrap">
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
          ? <span className="text-gray-600 dark:text-gray-400 text-xs font-mono">{row.model}</span>
          : <span className="text-gray-500 text-xs">-</span>
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
          <span className="text-gray-500 text-xs">-</span>
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
      {/* One-shot pulse-highlight keyframes for the deep-link panel.
          Scoped via a unique class name so it won't collide globally. */}
      <style>{`
        @keyframes su-msg-pulse {
          0%, 100% { box-shadow: 0 0 0 0 rgba(99, 102, 241, 0); }
          50%      { box-shadow: 0 0 0 4px rgba(99, 102, 241, 0.45); }
        }
        .su-msg-pulse-on {
          animation: su-msg-pulse 1s ease-in-out 2;
        }
      `}</style>

      {/* Deep-link detail panel (?interaction=ID) — additive, sits above the table. */}
      {panel && (
        <InteractionPanelView
          panel={panel}
          pulse={pulse}
          onClose={() => setPanel(null)}
          panelRef={panelRef}
        />
      )}

      <div className="flex items-center justify-between">
        <h2 className="text-sm font-semibold text-gray-800 dark:text-gray-200">
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
          <label className="text-xs text-gray-600 dark:text-gray-400">Type:</label>
          <select
            value={typeFilter}
            onChange={e => setTypeFilter(e.target.value as MessageType)}
            className="bg-white dark:bg-gray-800 border border-gray-200 dark:border-gray-700 rounded px-2 py-1.5 text-xs text-gray-700 dark:text-gray-300 focus:outline-none focus:border-indigo-500"
          >
            <option value="all">All ({messages.length})</option>
            <option value="user">user ({typeCounts.user ?? 0})</option>
            <option value="assistant">assistant ({typeCounts.assistant ?? 0})</option>
            <option value="tool_use">tool_use ({typeCounts.tool_use ?? 0})</option>
            <option value="tool_result">tool_result ({typeCounts.tool_result ?? 0})</option>
          </select>
        </div>

        <label className="flex items-center gap-1.5 text-xs text-gray-600 dark:text-gray-400 cursor-pointer select-none">
          <input
            type="checkbox"
            checked={errorsOnly}
            onChange={e => setErrorsOnly(e.target.checked)}
            className="rounded border-gray-300 dark:border-gray-600 bg-white dark:bg-gray-800 text-indigo-500 focus:ring-indigo-500 focus:ring-offset-0"
          />
          Errors only
        </label>
      </div>

      {error && (
        <div className="bg-red-100 dark:bg-red-900/20 border border-red-300 dark:border-red-800 rounded-lg p-3 text-red-700 dark:text-red-400 text-xs">
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
                <span className="text-xs text-gray-600 dark:text-gray-400 font-mono bg-white dark:bg-gray-800 px-2 py-0.5 rounded">
                  {selectedMessage.model}
                </span>
              )}
              <span className="text-xs text-gray-500">
                {formatTimestamp(selectedMessage.timestamp)}
              </span>
            </div>

            {/* Session info */}
            <div className="text-xs text-gray-500">
              Session: <span className="font-mono text-gray-600 dark:text-gray-400">{selectedMessage.session_id}</span>
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
            <div className="border-t border-gray-200 dark:border-gray-700 pt-3">
              {selectedMessage.type === 'assistant' ? (
                <Markdown content={selectedMessage.content} />
              ) : (
                <pre className="text-gray-700 dark:text-gray-300 whitespace-pre-wrap break-words font-mono text-xs leading-relaxed">
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

// ---------------------------------------------------------------------------
// Detail panel for the deep-linked interaction (?interaction=ID).
// Renders above the paginated list; pulse-highlights for 2s on appearance.
// ---------------------------------------------------------------------------

interface InteractionPanelViewProps {
  panel: InteractionPanel
  pulse: boolean
  onClose: () => void
  panelRef: React.MutableRefObject<HTMLDivElement | null>
}

function InteractionPanelView({ panel, pulse, onClose, panelRef }: InteractionPanelViewProps) {
  const pulseClass = pulse ? 'su-msg-pulse-on' : ''

  return (
    <div
      ref={panelRef}
      className={`bg-indigo-950/30 border border-indigo-800/60 rounded-lg p-3 space-y-2 ${pulseClass}`}
      data-testid="interaction-panel"
      data-interaction-id={panel.interactionId}
    >
      <div className="flex items-start justify-between gap-2">
        <div className="flex items-center gap-2 flex-wrap">
          <Badge color="purple" size="sm">interaction</Badge>
          <code className="text-[10px] text-indigo-300 font-mono">{panel.interactionId}</code>
          {panel.kind === 'loading' && (
            <span className="text-xs text-gray-600 dark:text-gray-400 inline-flex items-center gap-1">
              <IconLoader2 size={12} className="animate-spin" /> loading…
            </span>
          )}
          {panel.kind === 'loaded' && (
            <span className="text-[10px] text-gray-500 uppercase tracking-wider">on page</span>
          )}
          {panel.kind === 'fetched' && (
            <span className="text-[10px] text-gray-500 uppercase tracking-wider">fetched</span>
          )}
        </div>
        <button
          onClick={onClose}
          className="text-gray-500 hover:text-gray-700 dark:hover:text-gray-300 p-0.5 rounded"
          title="Close"
          aria-label="Close interaction panel"
        >
          <IconX size={14} />
        </button>
      </div>

      {panel.kind === 'loading' && (
        <div className="text-xs text-gray-500">Fetching interaction details…</div>
      )}

      {panel.kind === 'error' && (
        <div className="bg-red-100 dark:bg-red-900/20 border border-red-300 dark:border-red-800 rounded p-2 text-red-700 dark:text-red-400 text-xs">
          Failed to load interaction <code>{panel.interactionId}</code>: {panel.message}
        </div>
      )}

      {panel.kind === 'loaded' && (
        <LoadedMessageView message={panel.message} />
      )}

      {panel.kind === 'fetched' && (
        <FetchedInteractionView ix={panel.data} />
      )}
    </div>
  )
}

function LoadedMessageView({ message }: { message: Message }) {
  return (
    <div className="space-y-1.5">
      <div className="flex items-center gap-2 flex-wrap">
        <Badge color={getTypeColor(message.type)} size="sm">{message.type}</Badge>
        {message.model && (
          <span className="text-[10px] text-gray-600 dark:text-gray-400 font-mono bg-white dark:bg-gray-800 px-1.5 py-0.5 rounded">
            {message.model}
          </span>
        )}
        <span className="text-[10px] text-gray-500">{formatTimestamp(message.timestamp)}</span>
        <span className="text-[10px] text-gray-500 font-mono" title={message.session_id}>
          session {truncateSessionId(message.session_id)}
        </span>
      </div>
      <pre className="text-gray-700 dark:text-gray-300 whitespace-pre-wrap break-words font-mono text-xs leading-relaxed max-h-60 overflow-y-auto">
        {message.content}
      </pre>
    </div>
  )
}

function FetchedInteractionView({ ix }: { ix: InteractionResponse }) {
  return (
    <div className="space-y-2">
      <div className="flex items-center gap-2 flex-wrap text-[10px] text-gray-600 dark:text-gray-400">
        <span className="font-mono" title={ix.session_id}>session {truncateSessionId(ix.session_id)}</span>
        <span>•</span>
        <span>{formatTimestamp(ix.start_time)}</span>
        <span>•</span>
        <span>{ix.assistant_steps} step{ix.assistant_steps === 1 ? '' : 's'}</span>
        <span>•</span>
        <span>{ix.tool_count} tool{ix.tool_count === 1 ? '' : 's'}</span>
        {ix.model && (
          <>
            <span>•</span>
            <span className="font-mono">{ix.model}</span>
          </>
        )}
      </div>

      {/* Command (the user prompt) */}
      <div className="border-l-2 border-indigo-700 pl-2">
        <div className="text-[10px] uppercase tracking-wider text-indigo-300 mb-0.5">command</div>
        <pre className="text-gray-700 dark:text-gray-300 whitespace-pre-wrap break-words font-mono text-xs leading-relaxed max-h-40 overflow-y-auto">
          {ix.command.content}
        </pre>
      </div>

      {/* Assistant responses (collapsed by count when long). */}
      {ix.responses.length > 0 && (
        <div className="border-l-2 border-green-800 pl-2">
          <div className="text-[10px] uppercase tracking-wider text-green-300 mb-0.5">
            responses ({ix.responses.length})
          </div>
          <pre className="text-gray-700 dark:text-gray-300 whitespace-pre-wrap break-words font-mono text-xs leading-relaxed max-h-60 overflow-y-auto">
            {ix.responses.map(r => r.content).join('\n\n— —\n\n')}
          </pre>
        </div>
      )}

      {/* Tool results — only render when non-empty. */}
      {ix.tool_results.length > 0 && (
        <div className="border-l-2 border-yellow-800 pl-2">
          <div className="text-[10px] uppercase tracking-wider text-yellow-300 mb-0.5">
            tool_results ({ix.tool_results.length})
          </div>
          <pre className="text-gray-600 dark:text-gray-400 whitespace-pre-wrap break-words font-mono text-xs leading-relaxed max-h-40 overflow-y-auto">
            {ix.tool_results.map(r => r.content).join('\n\n— —\n\n')}
          </pre>
        </div>
      )}
    </div>
  )
}
