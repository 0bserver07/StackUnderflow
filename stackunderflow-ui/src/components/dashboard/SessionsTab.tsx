import { useState, useMemo, useEffect, useRef, useCallback } from 'react'
import { useQuery } from '@tanstack/react-query'
import {
  IconArrowLeft,
  IconArrowsLeftRight,
  IconClock,
  IconCode,
  IconEye,
  IconHash,
  IconMessage,
  IconSearch,
  IconTool,
  IconUser,
  IconRobot,
  IconChevronLeft,
  IconChevronRight as IconChevronRightNav,
} from '@tabler/icons-react'
import { getJsonlFiles, getJsonlContent, setProjectByDir } from '../../services/api'
import type { JsonlFile, JsonlContentResponse, SessionEfficiency } from '../../types/api'
import LoadingSpinner from '../common/LoadingSpinner'
import EmptyState from '../common/EmptyState'
import Markdown from '../common/Markdown'
import SessionEfficiencyTable from '../cost/SessionEfficiencyTable'
import SessionCompareView from '../cost/SessionCompareView'
import { NAV_EVENT, getParam, clearParam, type NavDetail } from '../../services/navigation'

interface SessionsTabProps {
  projectName: string
  sessionEfficiency?: SessionEfficiency[]
}

const PAGE_SIZE = 30

// ── Formatting helpers ──────────────────────────────────────────────────────

function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`
  if (n < 1_048_576) return `${(n / 1024).toFixed(1)} KB`
  return `${(n / 1_048_576).toFixed(1)} MB`
}

function fmtTs(ts: string | number | null): string {
  if (ts === null) return '—'
  try {
    const d = typeof ts === 'number' ? new Date(ts * 1000) : new Date(ts)
    return d.toLocaleString(undefined, { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' })
  } catch {
    return String(ts)
  }
}

function fmtDuration(mins: number | null | undefined): string {
  if (!mins) return '—'
  if (mins < 1) return '< 1m'
  if (mins < 60) return `${Math.round(mins)}m`
  const h = Math.floor(mins / 60)
  const m = Math.round(mins % 60)
  return m > 0 ? `${h}h ${m}m` : `${h}h`
}

function fmtDate(ts: string | number | null): string {
  if (ts === null) return '—'
  try {
    const d = typeof ts === 'number' ? new Date(ts * 1000) : new Date(ts)
    return d.toLocaleDateString(undefined, { month: 'short', day: 'numeric', year: 'numeric' })
  } catch {
    return String(ts)
  }
}

function relativeTime(ts: number): string {
  const diff = (Date.now() / 1000) - ts
  if (diff < 3600) return `${Math.round(diff / 60)}m ago`
  if (diff < 86400) return `${Math.round(diff / 3600)}h ago`
  if (diff < 604800) return `${Math.round(diff / 86400)}d ago`
  return fmtDate(ts)
}

// ── Message type styling ────────────────────────────────────────────────────

const ROLE_STYLES: Record<string, { icon: typeof IconUser; color: string; bg: string; label: string }> = {
  human:     { icon: IconUser,  color: 'text-blue-400',   bg: 'bg-blue-500/10',   label: 'You' },
  user:      { icon: IconUser,  color: 'text-blue-400',   bg: 'bg-blue-500/10',   label: 'You' },
  assistant: { icon: IconRobot, color: 'text-emerald-400', bg: 'bg-emerald-500/10', label: 'Claude' },
  tool_use:  { icon: IconTool,  color: 'text-purple-400', bg: 'bg-purple-500/10',  label: 'Tool Call' },
  tool_result: { icon: IconCode, color: 'text-amber-400', bg: 'bg-amber-500/10',  label: 'Tool Result' },
}

// Types we skip entirely in conversation view
const NOISE_TYPES = new Set(['progress', 'file-history-snapshot', 'pr-link', 'queue-operation', 'last-prompt', 'system'])

function isNoise(line: Record<string, unknown>): boolean {
  return NOISE_TYPES.has(String(line.type ?? ''))
}

function isToolReturn(line: Record<string, unknown>): boolean {
  // "user" messages that contain tool_result blocks are tool returns, not real prompts
  if (line.type !== 'user' && line.type !== 'human') return false
  const msg = line.message as Record<string, unknown> | undefined
  const content = msg?.content
  if (!Array.isArray(content)) return false
  return content.some((b: unknown) =>
    typeof b === 'object' && b !== null && (b as Record<string, unknown>).type === 'tool_result'
  )
}

function getRole(line: Record<string, unknown>): string {
  if (isToolReturn(line)) return 'tool_result'
  if (line.type === 'user' || line.type === 'human') return 'human'
  if (line.type === 'assistant') return 'assistant'
  return String(line.type ?? 'unknown')
}

function getContent(line: Record<string, unknown>): string {
  // Claude JSONL structure: { type, message: { role, content, ... }, ... }
  const msg = line.message as Record<string, unknown> | undefined
  const body = msg?.content ?? line.content ?? line.summary

  if (typeof body === 'string') return body
  if (Array.isArray(body)) {
    const parts: string[] = []
    for (const block of body) {
      if (typeof block === 'string') { parts.push(block); continue }
      if (!block || typeof block !== 'object') continue
      const b = block as Record<string, unknown>
      if (b.type === 'text') parts.push(String(b.text ?? ''))
      else if (b.type === 'tool_use') parts.push(`**Tool: ${b.name}**\n\`\`\`json\n${JSON.stringify(b.input, null, 2)}\n\`\`\``)
      else if (b.type === 'tool_result') {
        const inner = b.content
        if (typeof inner === 'string') parts.push(inner)
        else if (Array.isArray(inner)) {
          for (const sub of inner) {
            if (typeof sub === 'string') parts.push(sub)
            else if (sub && typeof sub === 'object' && (sub as Record<string, unknown>).type === 'text')
              parts.push(String((sub as Record<string, unknown>).text ?? ''))
          }
        }
      }
    }
    return parts.join('\n')
  }
  return ''
}

function getTimestamp(line: Record<string, unknown>): string | null {
  if (typeof line.timestamp === 'string') return line.timestamp
  return null
}

function getModel(line: Record<string, unknown>): string | null {
  const msg = line.message as Record<string, unknown> | undefined
  const model = msg?.model as string | undefined
  return model && model !== 'N/A' ? model.replace('claude-', '').replace(/-\d{8,}$/, '') : null
}

function getTokens(line: Record<string, unknown>): { input: number; output: number } | null {
  const msg = line.message as Record<string, unknown> | undefined
  const usage = msg?.usage as Record<string, number> | undefined
  if (!usage) return null
  const i = usage.input_tokens ?? 0
  const o = usage.output_tokens ?? 0
  return (i || o) ? { input: i, output: o } : null
}

// ── Session Card (file list) ────────────────────────────────────────────────

function fmtTokensShort(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`
  return String(n)
}

function fmtCost(amount: number): string {
  if (amount >= 100) return `$${amount.toFixed(0)}`
  if (amount >= 1) return `$${amount.toFixed(2)}`
  if (amount >= 0.01) return `$${amount.toFixed(2)}`
  return `$${amount.toFixed(4)}`
}

function fmtModel(m: string | null | undefined): string {
  if (!m) return ''
  return m.replace('claude-', '').replace(/-\d{8,}$/, '')
}

function SessionCard({
  file,
  selected,
  onClick,
  compareMode = false,
  checked = false,
  checkboxDisabled = false,
  onCheckToggle,
  highlighted = false,
  rowRef,
  sessionId,
}: {
  file: JsonlFile
  selected: boolean
  onClick: () => void
  compareMode?: boolean
  checked?: boolean
  checkboxDisabled?: boolean
  onCheckToggle?: () => void
  highlighted?: boolean
  rowRef?: (el: HTMLDivElement | null) => void
  sessionId: string
}) {
  const duration = file.modified - file.created
  const durationMins = duration / 60
  const totalTokens = (file.input_tokens ?? 0) + (file.output_tokens ?? 0)

  const baseClasses =
    'w-full text-left rounded-lg border transition-colors p-4 flex items-start gap-3'
  const stateClasses = selected
    ? 'bg-white dark:bg-gray-800 border-indigo-500/50'
    : checked
    ? 'bg-indigo-950/30 border-indigo-500/60'
    : 'bg-gray-50/40 dark:bg-gray-900/40 border-gray-200 dark:border-gray-800 hover:border-gray-200 dark:hover:border-gray-700 hover:bg-gray-50/70 dark:hover:bg-gray-900/70'
  const highlightClasses = highlighted
    ? ' ring-2 ring-indigo-400/70 animate-pulse'
    : ''

  const handleRootClick = () => {
    if (compareMode) {
      if (!checkboxDisabled || checked) {
        onCheckToggle?.()
      }
      return
    }
    onClick()
  }

  const contents = (
    <>
      {compareMode && (
        <div className="pt-0.5 shrink-0">
          <input
            type="checkbox"
            checked={checked}
            disabled={checkboxDisabled && !checked}
            onChange={(e) => {
              e.stopPropagation()
              onCheckToggle?.()
            }}
            onClick={(e) => e.stopPropagation()}
            aria-label={`Select session ${sessionId} to compare`}
            data-testid={`session-compare-checkbox-${sessionId}`}
            className="h-4 w-4 rounded border-gray-300 dark:border-gray-600 bg-gray-50 dark:bg-gray-900 text-indigo-500 focus:ring-indigo-500 disabled:opacity-40"
          />
        </div>
      )}
      <div className="flex-1 min-w-0">
        {/* Title row */}
        <div className="flex items-start justify-between gap-3 mb-1.5">
          <div className="flex-1 min-w-0 flex items-center gap-2">
            {file.title ? (
              <div className="text-sm text-gray-800 dark:text-gray-200 line-clamp-2">{file.title}</div>
            ) : (
              <div className="text-sm text-gray-600 dark:text-gray-400 font-mono">{file.name.split('.')[0]}</div>
            )}
            {file.is_subagent && (
              <span className="shrink-0 text-[10px] font-medium text-purple-400 bg-purple-500/15 border border-purple-500/30 px-1.5 py-0.5 rounded">
                Sub-agent
              </span>
            )}
          </div>
          <span className="text-xs text-gray-500 shrink-0">{relativeTime(file.modified)}</span>
        </div>

        {/* Stats row */}
        <div className="flex items-center gap-3 text-[11px] text-gray-500 flex-wrap">
          <span className="flex items-center gap-1">
            <IconClock size={11} />
            {fmtDuration(durationMins > 0.5 ? durationMins : null)}
          </span>
          {file.messages != null && (
            <span className="flex items-center gap-1">
              <IconMessage size={11} />
              {file.messages} msgs
            </span>
          )}
          {file.user_messages != null && (
            <span className="flex items-center gap-1">
              <IconUser size={11} className="text-blue-400/70" />
              {file.user_messages} prompts
            </span>
          )}
          {file.tool_calls != null && file.tool_calls > 0 && (
            <span className="flex items-center gap-1">
              <IconTool size={11} className="text-purple-400/70" />
              {file.tool_calls} tools
            </span>
          )}
          {totalTokens > 0 && (
            <span>{fmtTokensShort(totalTokens)} tokens</span>
          )}
          {file.estimated_cost != null && file.estimated_cost > 0 && (
            <span className="text-emerald-400/80 font-medium">{fmtCost(file.estimated_cost)}</span>
          )}
          {file.model && (
            <span className="text-gray-600 dark:text-gray-400 bg-white dark:bg-gray-800 px-1.5 py-0.5 rounded text-[10px]">{fmtModel(file.model)}</span>
          )}
          <span className="text-gray-600 dark:text-gray-400">{fmtBytes(file.size)}</span>
        </div>

        {/* Dates row */}
        <div className="flex items-center gap-4 text-[10px] text-gray-600 dark:text-gray-400 mt-1.5">
          <span>Created {fmtTs(file.created)}</span>
          <span>Modified {fmtTs(file.modified)}</span>
        </div>
      </div>
    </>
  )

  // Wrap in a div (not a button) when in compare mode so the checkbox can live
  // as a nested interactive element without breaking HTML validity.
  if (compareMode) {
    return (
      <div
        ref={rowRef}
        role="button"
        tabIndex={0}
        data-session-id={sessionId}
        data-testid={`session-row-${sessionId}`}
        onClick={handleRootClick}
        onKeyDown={(e) => {
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault()
            handleRootClick()
          }
        }}
        className={`${baseClasses} ${stateClasses}${highlightClasses} ${
          checkboxDisabled && !checked ? 'cursor-not-allowed opacity-60' : 'cursor-pointer'
        }`}
      >
        {contents}
      </div>
    )
  }

  return (
    <div
      ref={rowRef}
      data-session-id={sessionId}
      data-testid={`session-row-${sessionId}`}
      className={highlightClasses ? `rounded-lg${highlightClasses}` : undefined}
    >
      <button
        onClick={handleRootClick}
        className={`${baseClasses} ${stateClasses}`}
      >
        {contents}
      </button>
    </div>
  )
}

// ── Conversation Message ────────────────────────────────────────────────────

function ConversationMessage({
  line,
  index,
  showRaw,
  isSidechain = false,
  isFirstInSidechainGroup = false,
}: {
  line: Record<string, unknown>
  index: number
  showRaw: boolean
  isSidechain?: boolean
  isFirstInSidechainGroup?: boolean
}) {
  const [expanded, setExpanded] = useState(false)
  // Per-message JSON disclosure. Independent from the global `showRaw` toggle
  // so users can drill into one message without flipping the whole session.
  const [showJson, setShowJson] = useState(false)
  const role = getRole(line)
  const content = getContent(line)
  const ts = getTimestamp(line)
  const model = getModel(line)
  const tokens = getTokens(line)
  const style = ROLE_STYLES[role] ?? ROLE_STYLES.assistant!
  const Icon = style.icon

  if (!content && !showRaw) return null

  const isLong = content.length > 500
  const displayContent = expanded || !isLong ? content : content.slice(0, 500) + '...'

  const sidechainWrapper = (children: React.ReactNode) =>
    isSidechain ? (
      <div className="ml-6 border-l-2 border-purple-500/30 pl-3">
        {isFirstInSidechainGroup && (
          <div className="text-[10px] font-medium text-purple-400/70 mb-1">Sub-agent</div>
        )}
        {children}
      </div>
    ) : (
      <>{children}</>
    )

  if (showRaw) {
    return sidechainWrapper(
      <div className="border border-gray-200 dark:border-gray-800 rounded-lg p-3 bg-gray-50/30 dark:bg-gray-900/30">
        <div className="flex items-center gap-2 mb-2">
          <span className="text-[10px] text-gray-500 font-mono">#{index + 1}</span>
          <span className={`text-xs font-medium ${style.color}`}>{style.label}</span>
          {ts && <span className="text-[10px] text-gray-500">{fmtTs(ts)}</span>}
        </div>
        <pre className="text-[11px] text-gray-600 dark:text-gray-400 overflow-x-auto whitespace-pre-wrap font-mono bg-gray-50/50 dark:bg-gray-950/50 rounded p-2 max-h-96 overflow-y-auto">
          {JSON.stringify(line, null, 2)}
        </pre>
      </div>
    )
  }

  return sidechainWrapper(
    <div className={`rounded-lg border border-gray-200/50 dark:border-gray-800/50 ${style.bg}`}>
      {/* Header */}
      <div className="flex items-center gap-2 px-4 py-2 border-b border-gray-100/30 dark:border-gray-800/30">
        <Icon size={14} className={style.color} />
        <span className={`text-xs font-medium ${style.color}`}>{style.label}</span>
        {model && <span className="text-[10px] text-gray-500 bg-white dark:bg-gray-800 px-1.5 py-0.5 rounded">{model}</span>}
        {tokens && (
          <span className="text-[10px] text-gray-600 dark:text-gray-400">
            {tokens.input.toLocaleString()} in / {tokens.output.toLocaleString()} out
          </span>
        )}
        <span className="flex-1" />
        {ts && <span className="text-[10px] text-gray-600 dark:text-gray-400">{fmtTs(ts)}</span>}
        <button
          type="button"
          onClick={() => setShowJson((v) => !v)}
          aria-pressed={showJson}
          aria-label={showJson ? 'Hide raw JSON' : 'View raw JSON'}
          title={showJson ? 'Hide raw JSON' : 'View raw JSON'}
          className={`text-[10px] px-1.5 py-0.5 rounded border transition-colors ${
            showJson
              ? 'bg-amber-100 text-amber-800 border-amber-300 dark:bg-amber-900/40 dark:text-amber-300 dark:border-amber-800'
              : 'bg-white dark:bg-gray-800 text-gray-600 dark:text-gray-400 border-gray-200 dark:border-gray-700 hover:text-gray-800 dark:hover:text-gray-200'
          }`}
        >
          {showJson ? 'Hide JSON' : 'JSON'}
        </button>
      </div>
      {/* Body */}
      <div className="px-4 py-3">
        <div className="text-sm text-gray-700 dark:text-gray-300 whitespace-pre-wrap break-words">
          <Markdown content={displayContent} />
        </div>
        {isLong && !expanded && (
          <button
            onClick={() => setExpanded(true)}
            className="text-xs text-indigo-400 hover:text-indigo-300 mt-2"
          >
            Show more...
          </button>
        )}
        {isLong && expanded && (
          <button
            onClick={() => setExpanded(false)}
            className="text-xs text-indigo-400 hover:text-indigo-300 mt-2"
          >
            Show less
          </button>
        )}
        {showJson && (
          <pre className="mt-3 text-[11px] text-gray-600 dark:text-gray-400 overflow-x-auto whitespace-pre-wrap font-mono bg-gray-50/70 dark:bg-gray-950/60 rounded p-2 max-h-96 overflow-y-auto border border-gray-200 dark:border-gray-800">
            {JSON.stringify(line, null, 2)}
          </pre>
        )}
      </div>
    </div>
  )
}

// ── Session Viewer ──────────────────────────────────────────────────────────

function SessionViewer({
  data,
  onBack,
}: {
  data: JsonlContentResponse
  onBack: () => void
}) {
  const [showRaw, setShowRaw] = useState(false)
  const [typeFilter, setTypeFilter] = useState<string>('all')
  const [search, setSearch] = useState('')
  const [page, setPage] = useState(0)

  const filtered = useMemo(() => {
    // Always filter out noise (progress, file-history, system, etc.)
    let lines = data.lines.filter(l => !isNoise(l))
    if (typeFilter === 'hide_subagent') {
      lines = lines.filter(l => l.isSidechain !== true)
    } else if (typeFilter !== 'all') {
      lines = lines.filter(l => getRole(l) === typeFilter)
    }
    if (search.trim()) {
      const q = search.toLowerCase()
      lines = lines.filter(l => getContent(l).toLowerCase().includes(q))
    }
    return lines
  }, [data.lines, typeFilter, search])

  const totalPages = Math.max(1, Math.ceil(filtered.length / PAGE_SIZE))
  const p = Math.min(page, totalPages - 1)
  const pageLines = filtered.slice(p * PAGE_SIZE, (p + 1) * PAGE_SIZE)

  // Map to original indices
  const idxMap = useMemo(() => {
    const m = new Map<Record<string, unknown>, number>()
    data.lines.forEach((l, i) => m.set(l, i))
    return m
  }, [data.lines])

  const meta = data.metadata

  return (
    <div className="space-y-4">
      {/* Header bar */}
      <div className="flex items-center gap-3 flex-wrap">
        <button
          onClick={onBack}
          className="flex items-center gap-1.5 px-3 py-1.5 bg-white dark:bg-gray-800 border border-gray-200 dark:border-gray-700 rounded text-sm text-gray-700 dark:text-gray-300 hover:bg-gray-200 dark:hover:bg-gray-700"
        >
          <IconArrowLeft size={14} />
          Sessions
        </button>

        {/* Stats badges */}
        <div className="flex items-center gap-3 text-xs text-gray-500">
          <span className="flex items-center gap-1"><IconMessage size={12} /> {data.total_lines} messages</span>
          <span className="flex items-center gap-1"><IconUser size={12} className="text-blue-400" /> {data.user_count}</span>
          <span className="flex items-center gap-1"><IconRobot size={12} className="text-emerald-400" /> {data.assistant_count}</span>
          {meta.duration_minutes && (
            <span className="flex items-center gap-1"><IconClock size={12} /> {fmtDuration(meta.duration_minutes)}</span>
          )}
        </div>

        <span className="flex-1" />

        {/* Raw toggle */}
        <button
          onClick={() => setShowRaw(!showRaw)}
          className={`flex items-center gap-1.5 px-2.5 py-1 text-xs rounded border transition-colors ${
            showRaw
              ? 'bg-amber-600/20 border-amber-600/50 text-amber-400'
              : 'bg-white dark:bg-gray-800 border-gray-200 dark:border-gray-700 text-gray-600 dark:text-gray-400 hover:text-gray-800 dark:hover:text-gray-200'
          }`}
        >
          {showRaw ? <IconCode size={13} /> : <IconEye size={13} />}
          {showRaw ? 'Raw JSON' : 'Formatted'}
        </button>
      </div>

      {/* Session info */}
      <div className="grid grid-cols-2 sm:grid-cols-4 gap-3 p-3 bg-gray-50/60 dark:bg-gray-900/60 border border-gray-200 dark:border-gray-800 rounded-lg text-xs">
        <div>
          <span className="text-gray-500 block mb-0.5">Started</span>
          <span className="text-gray-800 dark:text-gray-200">{fmtTs(meta.first_timestamp ?? meta.created)}</span>
        </div>
        <div>
          <span className="text-gray-500 block mb-0.5">Ended</span>
          <span className="text-gray-800 dark:text-gray-200">{fmtTs(meta.last_timestamp ?? meta.modified)}</span>
        </div>
        <div>
          <span className="text-gray-500 block mb-0.5">Duration</span>
          <span className="text-gray-800 dark:text-gray-200">{fmtDuration(meta.duration_minutes)}</span>
        </div>
        <div>
          <span className="text-gray-500 block mb-0.5">Working Dir</span>
          <span className="text-gray-800 dark:text-gray-200 font-mono text-[10px] truncate block">{meta.cwd}</span>
        </div>
      </div>

      {/* Filters */}
      <div className="flex gap-2 flex-wrap">
        <div className="relative flex-1 min-w-[200px]">
          <IconSearch size={14} className="absolute left-2.5 top-1/2 -translate-y-1/2 text-gray-500" />
          <input
            type="text"
            value={search}
            onChange={e => { setSearch(e.target.value); setPage(0) }}
            placeholder="Search in conversation..."
            className="w-full pl-8 pr-3 py-1.5 bg-white dark:bg-gray-800 border border-gray-200 dark:border-gray-700 rounded text-xs text-gray-800 dark:text-gray-200 placeholder-gray-500 focus:outline-none focus:border-indigo-500"
          />
        </div>
        <select
          value={typeFilter}
          onChange={e => { setTypeFilter(e.target.value); setPage(0) }}
          className="px-3 py-1.5 bg-white dark:bg-gray-800 border border-gray-200 dark:border-gray-700 rounded text-xs text-gray-700 dark:text-gray-300 focus:outline-none"
        >
          <option value="all">Conversation</option>
          <option value="human">Your prompts</option>
          <option value="assistant">Claude responses</option>
          <option value="tool_result">Tool results</option>
          <option value="hide_subagent">Hide sub-agents</option>
        </select>
      </div>

      {/* Conversation */}
      {filtered.length === 0 ? (
        <EmptyState title="No messages" description="No messages match your filters." />
      ) : (
        <div className="space-y-2">
          {pageLines.map((line, i) => {
            const isSidechain = line.isSidechain === true
            const prevLine = i > 0 ? pageLines[i - 1] : undefined
            const isFirstInSidechainGroup = isSidechain && (!prevLine || prevLine.isSidechain !== true)
            return (
              <ConversationMessage
                key={idxMap.get(line) ?? i}
                line={line}
                index={idxMap.get(line) ?? i}
                showRaw={showRaw}
                isSidechain={isSidechain}
                isFirstInSidechainGroup={isFirstInSidechainGroup}
              />
            )
          })}
        </div>
      )}

      {/* Pagination */}
      {totalPages > 1 && (
        <div className="flex items-center justify-between pt-1">
          <span className="text-xs text-gray-500">
            {p * PAGE_SIZE + 1}–{Math.min((p + 1) * PAGE_SIZE, filtered.length)} of {filtered.length}
          </span>
          <div className="flex items-center gap-1">
            <button
              onClick={() => setPage(x => Math.max(0, x - 1))}
              disabled={p === 0}
              className="p-1 rounded bg-white dark:bg-gray-800 border border-gray-200 dark:border-gray-700 text-gray-600 dark:text-gray-400 disabled:opacity-40"
            >
              <IconChevronLeft size={14} />
            </button>
            <span className="px-2 text-xs text-gray-600 dark:text-gray-400">{p + 1}/{totalPages}</span>
            <button
              onClick={() => setPage(x => Math.min(totalPages - 1, x + 1))}
              disabled={p >= totalPages - 1}
              className="p-1 rounded bg-white dark:bg-gray-800 border border-gray-200 dark:border-gray-700 text-gray-600 dark:text-gray-400 disabled:opacity-40"
            >
              <IconChevronRightNav size={14} />
            </button>
          </div>
        </div>
      )}
    </div>
  )
}

// ── Main Component ──────────────────────────────────────────────────────────

/**
 * Strip `.jsonl` (or any single extension) to get a bare session id that
 * matches the ids emitted by analytics collectors + navigation service.
 */
function toSessionId(fileName: string): string {
  return fileName.replace(/\.jsonl$/i, '')
}

export default function SessionsTab({ projectName, sessionEfficiency }: SessionsTabProps) {
  const [selectedFile, setSelectedFile] = useState<string | null>(null)
  const [sortBy, setSortBy] = useState<'modified' | 'created' | 'size'>('modified')
  const [compareMode, setCompareMode] = useState(false)
  const [comparePair, setComparePair] = useState<string[]>([])
  const [highlightedSession, setHighlightedSession] = useState<string | null>(null)
  const rowRefs = useRef<Map<string, HTMLDivElement | null>>(new Map())

  const filesQuery = useQuery({
    queryKey: ['jsonlFiles', projectName],
    queryFn: () => getJsonlFiles(projectName),
  })

  const contentQuery = useQuery({
    queryKey: ['jsonlContent', selectedFile, projectName],
    queryFn: () => getJsonlContent(selectedFile!, projectName),
    enabled: !!selectedFile,
  })

  // Reuse ProjectDashboard's cached project resolution to get the backend log path
  // required by SessionCompareView. Same queryKey → zero extra network calls.
  const projectInfoQuery = useQuery({
    queryKey: ['setProject', projectName],
    queryFn: () => setProjectByDir(projectName),
    enabled: !!projectName,
    staleTime: 60_000,
  })
  const currentLogPath = projectInfoQuery.data?.log_path ?? ''

  // Highlight + scroll helper — drives both the `?session=` deep-link and
  // cross-tab NAV_EVENT handling.
  const highlightSession = useCallback((sessionId: string | null) => {
    if (!sessionId) return
    setHighlightedSession(sessionId)
    // Wait a tick for the DOM to render the row, then scroll to it.
    requestAnimationFrame(() => {
      const el = rowRefs.current.get(sessionId)
      if (el) el.scrollIntoView({ behavior: 'smooth', block: 'center' })
    })
    window.setTimeout(() => {
      setHighlightedSession((curr) => (curr === sessionId ? null : curr))
    }, 2000)
  }, [])

  // Respond to `?session=ID` on mount and whenever the URL changes.
  useEffect(() => {
    const sid = getParam('session')
    if (sid) {
      highlightSession(sid)
      // Clear the param so a refresh or further navigation doesn't keep
      // re-highlighting. Deep-link goal already satisfied.
      clearParam('session')
    }
  }, [highlightSession])

  useEffect(() => {
    const onPopState = () => {
      const sid = getParam('session')
      if (sid) {
        highlightSession(sid)
        clearParam('session')
      }
    }
    window.addEventListener('popstate', onPopState)
    return () => window.removeEventListener('popstate', onPopState)
  }, [highlightSession])

  // React to external `openSession(...)` calls from navigation.ts.
  useEffect(() => {
    const onNav = (e: Event) => {
      const detail = (e as CustomEvent<NavDetail>).detail
      if (!detail) return
      if (detail.tab === 'sessions' && typeof detail.session === 'string') {
        highlightSession(detail.session)
      }
    }
    window.addEventListener(NAV_EVENT, onNav)
    return () => window.removeEventListener(NAV_EVENT, onNav)
  }, [highlightSession])

  const toggleCompareSelection = useCallback((sessionId: string) => {
    setComparePair((prev) => {
      if (prev.includes(sessionId)) return prev.filter((x) => x !== sessionId)
      if (prev.length >= 2) return prev // hard cap at 2
      return [...prev, sessionId]
    })
  }, [])

  const resetCompareSelection = useCallback(() => setComparePair([]), [])

  if (filesQuery.isLoading) return <LoadingSpinner message="Loading sessions..." />
  if (filesQuery.isError) return <div className="text-red-400 p-4">Failed to load sessions</div>

  const files = filesQuery.data!

  // Session content view — exit early, skips compare UI while viewing one session.
  if (selectedFile) {
    if (contentQuery.isLoading) return <LoadingSpinner message="Loading conversation..." />
    if (contentQuery.isError) return (
      <div className="space-y-3">
        <button onClick={() => setSelectedFile(null)} className="flex items-center gap-1.5 px-3 py-1.5 bg-white dark:bg-gray-800 border border-gray-200 dark:border-gray-700 rounded text-sm text-gray-700 dark:text-gray-300 hover:bg-gray-200 dark:hover:bg-gray-700">
          <IconArrowLeft size={14} /> Back
        </button>
        <div className="text-red-400 p-4">Failed to load conversation</div>
      </div>
    )
    if (contentQuery.data) {
      return <SessionViewer data={contentQuery.data} onBack={() => setSelectedFile(null)} />
    }
  }

  if (files.length === 0) {
    return <EmptyState title="No sessions" description="No session files found." />
  }

  const sorted = [...files].sort((a, b) => {
    if (sortBy === 'size') return b.size - a.size
    if (sortBy === 'created') return b.created - a.created
    return b.modified - a.modified
  })

  const bothSelected = comparePair.length === 2
  const [aId, bId] = comparePair

  return (
    <div className="space-y-4" data-testid="sessions-tab">
      {/* Session efficiency table — analytics-expansion spec §2.5 */}
      <SessionEfficiencyTable data={sessionEfficiency ?? []} />

      {/* Compare panel — rendered above the session list when 2 sessions are checked */}
      {compareMode && bothSelected && aId && bId && (
        <SessionCompareView
          logPath={currentLogPath}
          sessionAId={aId}
          sessionBId={bId}
          onClose={resetCompareSelection}
        />
      )}

      <div className="flex items-center justify-between gap-3 flex-wrap">
        <div className="flex items-center gap-2 text-sm text-gray-600 dark:text-gray-400">
          <IconHash size={14} />
          <span><span className="text-gray-900 dark:text-gray-100 font-medium">{files.length}</span> sessions</span>
          {compareMode && (
            <span className="text-xs text-indigo-400">
              · {comparePair.length}/2 selected
            </span>
          )}
        </div>
        <div className="flex items-center gap-2 text-xs">
          <button
            type="button"
            onClick={() => {
              setCompareMode((v) => {
                const next = !v
                if (!next) setComparePair([])
                return next
              })
            }}
            aria-pressed={compareMode}
            data-testid="sessions-compare-toggle"
            className={`flex items-center gap-1.5 px-2.5 py-1 rounded border transition-colors ${
              compareMode
                ? 'bg-indigo-600/20 border-indigo-500/60 text-indigo-300'
                : 'bg-white dark:bg-gray-800 border-gray-200 dark:border-gray-700 text-gray-600 dark:text-gray-400 hover:text-gray-800 dark:hover:text-gray-200'
            }`}
          >
            <IconArrowsLeftRight size={13} />
            Compare
          </button>
          <div className="flex items-center gap-1">
            {(['modified', 'created', 'size'] as const).map(s => (
              <button
                key={s}
                onClick={() => setSortBy(s)}
                className={`px-2 py-1 rounded ${sortBy === s ? 'bg-gray-200 dark:bg-gray-700 text-gray-800 dark:text-gray-200' : 'text-gray-500 hover:text-gray-700 dark:hover:text-gray-300'}`}
              >
                {s === 'modified' ? 'Recent' : s === 'created' ? 'Oldest' : 'Size'}
              </button>
            ))}
          </div>
        </div>
      </div>

      <div className="space-y-1.5">
        {sorted.map((f) => {
          const sid = toSessionId(f.name)
          const isChecked = comparePair.includes(sid)
          return (
            <SessionCard
              key={f.name}
              file={f}
              sessionId={sid}
              selected={selectedFile === f.name}
              onClick={() => setSelectedFile(f.name)}
              compareMode={compareMode}
              checked={isChecked}
              checkboxDisabled={comparePair.length >= 2}
              onCheckToggle={() => toggleCompareSelection(sid)}
              highlighted={highlightedSession === sid}
              rowRef={(el) => {
                if (el) rowRefs.current.set(sid, el)
                else rowRefs.current.delete(sid)
              }}
            />
          )
        })}
      </div>
    </div>
  )
}
