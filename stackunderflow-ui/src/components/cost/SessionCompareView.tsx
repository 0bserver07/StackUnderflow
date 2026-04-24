import { useCallback, useEffect, useState } from 'react'
import { IconX, IconRefresh, IconArrowsLeftRight } from '@tabler/icons-react'
import type { SessionCost } from '../../types/api'

interface SessionCompareViewProps {
  logPath: string
  sessionAId: string
  sessionBId: string
  onClose?: () => void
}

interface CompareDiff {
  cost: number
  tokens: Record<string, number>
  commands: number
  errors: number
  duration_s: number
}

interface CompareResponse {
  a: SessionCost
  b: SessionCost
  diff: CompareDiff
}

// Metrics where a smaller value is better — green if B < A, red if B > A.
// (Token counts are weakly "bad" — more usage usually means more cost.)
const LOWER_IS_BETTER = new Set([
  'cost',
  'errors',
  'duration_s',
  'input',
  'output',
  'cache_creation',
  'cache_read',
  'commands',
  'messages',
])

type RowKey =
  | 'cost'
  | 'input'
  | 'output'
  | 'cache_read'
  | 'cache_creation'
  | 'commands'
  | 'messages'
  | 'errors'
  | 'duration_s'

const ROWS: { key: RowKey; label: string; kind: 'cost' | 'tokens' | 'count' | 'duration' }[] = [
  { key: 'cost', label: 'Cost', kind: 'cost' },
  { key: 'input', label: 'Input tokens', kind: 'tokens' },
  { key: 'output', label: 'Output tokens', kind: 'tokens' },
  { key: 'cache_read', label: 'Cache read', kind: 'tokens' },
  { key: 'cache_creation', label: 'Cache creation', kind: 'tokens' },
  { key: 'commands', label: 'Commands', kind: 'count' },
  { key: 'messages', label: 'Messages', kind: 'count' },
  { key: 'errors', label: 'Errors', kind: 'count' },
  { key: 'duration_s', label: 'Duration', kind: 'duration' },
]

function shortSession(sid: string): string {
  return sid.length > 12 ? sid.slice(0, 8) : sid
}

function formatCost(cost: number): string {
  const abs = Math.abs(cost)
  if (abs >= 100) return `$${cost.toFixed(0)}`
  if (abs >= 1) return `$${cost.toFixed(2)}`
  return `$${cost.toFixed(4)}`
}

function formatNumber(n: number): string {
  return Math.round(n).toLocaleString()
}

function formatDuration(seconds: number): string {
  if (!seconds) return '0s'
  const abs = Math.abs(seconds)
  if (abs < 60) return `${seconds.toFixed(0)}s`
  const m = seconds / 60
  if (Math.abs(m) < 60) return `${m.toFixed(1)}m`
  const h = m / 60
  return `${h.toFixed(1)}h`
}

function formatValue(kind: 'cost' | 'tokens' | 'count' | 'duration', v: number): string {
  switch (kind) {
    case 'cost':
      return formatCost(v)
    case 'duration':
      return formatDuration(v)
    case 'tokens':
    case 'count':
      return formatNumber(v)
  }
}

function formatDelta(kind: 'cost' | 'tokens' | 'count' | 'duration', delta: number): string {
  if (delta === 0) return '—'
  const sign = delta > 0 ? '+' : '−'
  const body = formatValue(kind, Math.abs(delta))
  return `${sign}${body}`
}

function formatPct(pct: number | null): string {
  if (pct === null) return ''
  if (!isFinite(pct)) return '∞%'
  if (pct === 0) return ''
  const sign = pct > 0 ? '+' : '−'
  return ` (${sign}${Math.abs(pct).toFixed(0)}%)`
}

function pickValue(session: SessionCost, key: RowKey): number {
  switch (key) {
    case 'cost':
      return session.cost ?? 0
    case 'commands':
      return session.commands ?? 0
    case 'messages':
      return session.messages ?? 0
    case 'errors':
      return session.errors ?? 0
    case 'duration_s':
      return session.duration_s ?? 0
    default:
      return session.tokens?.[key] ?? 0
  }
}

function pickDiff(diff: CompareDiff, key: RowKey): number {
  switch (key) {
    case 'cost':
      return diff.cost ?? 0
    case 'commands':
      return diff.commands ?? 0
    case 'messages':
      // Backend diff doesn't include messages; derive from inputs.
      return 0
    case 'errors':
      return diff.errors ?? 0
    case 'duration_s':
      return diff.duration_s ?? 0
    default:
      return diff.tokens?.[key] ?? 0
  }
}

function deltaColor(key: RowKey, delta: number): string {
  if (delta === 0) return 'text-gray-500'
  const lowerIsBetter = LOWER_IS_BETTER.has(key)
  // For "lower-is-better" metrics, negative delta (B < A) = green improvement.
  if (lowerIsBetter) return delta < 0 ? 'text-green-400' : 'text-red-400'
  // Otherwise treat the opposite way — fall back to neutral coloring.
  return delta > 0 ? 'text-green-400' : 'text-red-400'
}

export default function SessionCompareView({
  logPath,
  sessionAId,
  sessionBId,
  onClose,
}: SessionCompareViewProps) {
  const [data, setData] = useState<CompareResponse | null>(null)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const haveBothIds = Boolean(sessionAId && sessionBId)

  const load = useCallback(async () => {
    if (!haveBothIds || !logPath) {
      setData(null)
      return
    }
    setLoading(true)
    setError(null)
    try {
      const url =
        `/api/sessions/compare` +
        `?log_path=${encodeURIComponent(logPath)}` +
        `&a=${encodeURIComponent(sessionAId)}` +
        `&b=${encodeURIComponent(sessionBId)}`
      const res = await fetch(url)
      if (!res.ok) {
        const text = await res.text().catch(() => '')
        throw new Error(`${res.status} ${res.statusText}${text ? `: ${text}` : ''}`)
      }
      const json = (await res.json()) as CompareResponse
      setData(json)
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
      setData(null)
    } finally {
      setLoading(false)
    }
  }, [logPath, sessionAId, sessionBId, haveBothIds])

  useEffect(() => {
    load()
  }, [load])

  // Empty state: missing one or both ids
  if (!haveBothIds) {
    return (
      <div className="bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-6 border border-gray-200 dark:border-gray-800">
        <div className="flex items-center gap-2 mb-2">
          <IconArrowsLeftRight size={16} className="text-indigo-400" />
          <h3 className="text-sm font-medium text-gray-700 dark:text-gray-300">Compare sessions</h3>
          {onClose && (
            <button
              type="button"
              onClick={onClose}
              className="ml-auto text-gray-500 hover:text-gray-700 dark:hover:text-gray-300"
              aria-label="Close compare view"
            >
              <IconX size={14} />
            </button>
          )}
        </div>
        <div className="text-xs text-gray-500 py-6 text-center">
          Pick two sessions to compare them side-by-side.
        </div>
      </div>
    )
  }

  // Header — always rendered above content
  const header = (
    <div className="flex items-center gap-2 mb-3">
      <IconArrowsLeftRight size={16} className="text-indigo-400" />
      <h3 className="text-sm font-medium text-gray-700 dark:text-gray-300">
        Compare sessions
        <span className="ml-2 text-xs text-gray-500 font-normal font-mono">
          {shortSession(sessionAId)} ↔ {shortSession(sessionBId)}
        </span>
      </h3>
      {onClose && (
        <button
          type="button"
          onClick={onClose}
          className="ml-auto text-gray-500 hover:text-gray-700 dark:hover:text-gray-300"
          aria-label="Close compare view"
        >
          <IconX size={14} />
        </button>
      )}
    </div>
  )

  // Loading skeleton
  if (loading) {
    return (
      <div className="bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-4 border border-gray-200 dark:border-gray-800">
        {header}
        <div className="bg-gray-100/50 dark:bg-gray-800/30 rounded border border-gray-200 dark:border-gray-800 overflow-hidden">
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-gray-200 dark:border-gray-800 text-gray-600 dark:text-gray-400 text-xs uppercase tracking-wider">
                <th className="px-3 py-2 text-left">Metric</th>
                <th className="px-3 py-2 text-right">A</th>
                <th className="px-3 py-2 text-right">B</th>
                <th className="px-3 py-2 text-right">Δ</th>
              </tr>
            </thead>
            <tbody>
              {ROWS.map((row) => (
                <tr key={row.key} className="border-b border-gray-200/50 dark:border-gray-800/50 animate-pulse">
                  <td className="px-3 py-2">
                    <div className="h-3 w-24 bg-gray-200/80 dark:bg-gray-700/60 rounded" />
                  </td>
                  <td className="px-3 py-2">
                    <div className="h-3 w-16 bg-gray-200/60 dark:bg-gray-700/40 rounded ml-auto" />
                  </td>
                  <td className="px-3 py-2">
                    <div className="h-3 w-16 bg-gray-200/60 dark:bg-gray-700/40 rounded ml-auto" />
                  </td>
                  <td className="px-3 py-2">
                    <div className="h-3 w-12 bg-gray-200/60 dark:bg-gray-700/40 rounded ml-auto" />
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>
    )
  }

  // Error state
  if (error) {
    return (
      <div className="bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-4 border border-red-300 dark:border-red-900/60">
        {header}
        <div className="bg-red-100 dark:bg-red-950/30 border border-red-300 dark:border-red-900/40 rounded p-3 text-xs text-red-800 dark:text-red-200">
          <div className="font-medium mb-1">Failed to load comparison</div>
          <div className="text-red-700/90 dark:text-red-300/80 break-all">{error}</div>
          <button
            type="button"
            onClick={load}
            className="mt-3 inline-flex items-center gap-1 px-2 py-1 rounded bg-red-200 dark:bg-red-900/50 hover:bg-red-300 dark:hover:bg-red-900/70 border border-red-400 dark:border-red-800 text-red-800 dark:text-red-100 text-[11px] uppercase tracking-wider"
          >
            <IconRefresh size={12} /> Retry
          </button>
        </div>
      </div>
    )
  }

  if (!data) {
    return (
      <div className="bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-4 border border-gray-200 dark:border-gray-800">
        {header}
        <div className="text-xs text-gray-500 py-6 text-center">No comparison data.</div>
      </div>
    )
  }

  return (
    <div className="bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-4 border border-gray-200 dark:border-gray-800">
      {header}
      <div className="bg-gray-100/50 dark:bg-gray-800/30 rounded border border-gray-200 dark:border-gray-800 overflow-hidden">
        <div className="overflow-x-auto">
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-gray-200 dark:border-gray-800 text-gray-600 dark:text-gray-400 text-xs uppercase tracking-wider">
                <th className="px-3 py-2 text-left">Metric</th>
                <th className="px-3 py-2 text-right">
                  <span className="font-mono text-indigo-700 dark:text-indigo-300">A · {shortSession(data.a.session_id)}</span>
                </th>
                <th className="px-3 py-2 text-right">
                  <span className="font-mono text-indigo-700 dark:text-indigo-300">B · {shortSession(data.b.session_id)}</span>
                </th>
                <th className="px-3 py-2 text-right">Δ (B − A)</th>
              </tr>
            </thead>
            <tbody>
              {ROWS.map((row) => {
                const aVal = pickValue(data.a, row.key)
                const bVal = pickValue(data.b, row.key)
                // Prefer backend-computed delta; fall back to client diff for messages.
                const apiDelta = pickDiff(data.diff, row.key)
                const delta = apiDelta !== 0 ? apiDelta : bVal - aVal
                const pct = aVal !== 0 ? (delta / Math.abs(aVal)) * 100 : delta === 0 ? 0 : Infinity
                const color = deltaColor(row.key, delta)
                return (
                  <tr key={row.key} className="border-b border-gray-200/50 dark:border-gray-800/50">
                    <td className="px-3 py-2 text-gray-700 dark:text-gray-300">{row.label}</td>
                    <td className="px-3 py-2 text-right text-gray-800 dark:text-gray-200 tabular-nums">
                      {formatValue(row.kind, aVal)}
                    </td>
                    <td className="px-3 py-2 text-right text-gray-800 dark:text-gray-200 tabular-nums">
                      {formatValue(row.kind, bVal)}
                    </td>
                    <td className={`px-3 py-2 text-right tabular-nums ${color}`}>
                      {formatDelta(row.kind, delta)}
                      <span className="text-[10px] text-gray-500">{formatPct(pct)}</span>
                    </td>
                  </tr>
                )
              })}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  )
}
