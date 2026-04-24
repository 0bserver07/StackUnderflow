import { useMemo, useState } from 'react'
import { IconAlertTriangle, IconRefresh } from '@tabler/icons-react'
import type { RetrySignal } from '../../types/api'
import { openInteraction } from '../../services/navigation'

interface RetryAlertsPanelProps {
  signals: RetrySignal[] | null | undefined
}

type SeverityFilter = 'all' | 'ge2' | 'ge3'

function formatCost(cost: number): string {
  if (cost >= 100) return `$${cost.toFixed(0)}`
  if (cost >= 1) return `$${cost.toFixed(2)}`
  return `$${cost.toFixed(4)}`
}

function formatTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`
  if (n >= 1_000) return `${(n / 1_000).toFixed(0)}K`
  return n.toLocaleString()
}

function formatTime(iso: string): string {
  if (!iso) return ''
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  return d.toLocaleString(undefined, {
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
  })
}

/**
 * Severity per signal — 3+ consecutive failures = red, 2 = amber.
 */
function severity(sig: RetrySignal): 'red' | 'amber' {
  return sig.consecutive_failures >= 3 ? 'red' : 'amber'
}

const STYLES = {
  red: {
    wrapper: 'bg-red-50 dark:bg-red-900/20 border-red-300 dark:border-red-800/60',
    icon: 'text-red-600 dark:text-red-400',
    label: 'text-red-700 dark:text-red-300',
  },
  amber: {
    wrapper: 'bg-amber-50 dark:bg-amber-900/20 border-amber-300 dark:border-amber-800/60',
    icon: 'text-amber-600 dark:text-amber-400',
    label: 'text-amber-700 dark:text-amber-300',
  },
} as const

const FILTERS: Array<{ id: SeverityFilter; label: string; predicate: (s: RetrySignal) => boolean }> = [
  { id: 'all', label: 'All', predicate: () => true },
  { id: 'ge2', label: '≥2 failures', predicate: (s) => s.consecutive_failures >= 2 },
  { id: 'ge3', label: '≥3 failures', predicate: (s) => s.consecutive_failures >= 3 },
]

export default function RetryAlertsPanel({ signals }: RetryAlertsPanelProps) {
  const [filter, setFilter] = useState<SeverityFilter>('all')

  const sortedAll = useMemo(() => {
    if (!signals || signals.length === 0) return []
    return [...signals].sort(
      (a, b) =>
        b.estimated_wasted_cost - a.estimated_wasted_cost ||
        b.consecutive_failures - a.consecutive_failures,
    )
  }, [signals])

  const visible = useMemo(() => {
    const pred = FILTERS.find((f) => f.id === filter)?.predicate ?? (() => true)
    return sortedAll.filter(pred)
  }, [sortedAll, filter])

  const totalWasted = useMemo(
    () => visible.reduce((sum, sig) => sum + (sig.estimated_wasted_cost ?? 0), 0),
    [visible],
  )

  if (!signals || signals.length === 0) {
    return (
      <div
        className="bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-4 border border-gray-200 dark:border-gray-800"
        data-testid="retry-alerts-panel"
      >
        <h3 className="text-sm font-medium text-gray-700 dark:text-gray-300 mb-3">Retry Alerts</h3>
        <div className="text-xs text-gray-500 py-8 text-center">
          No retry storms detected — nice.
        </div>
      </div>
    )
  }

  return (
    <div
      className="bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-4 border border-gray-200 dark:border-gray-800"
      data-testid="retry-alerts-panel"
    >
      <div className="flex items-baseline justify-between mb-3 gap-2 flex-wrap">
        <h3 className="text-sm font-medium text-gray-700 dark:text-gray-300" data-testid="retry-alerts-summary">
          {visible.length} retr{visible.length === 1 ? 'y' : 'ies'} wasted{' '}
          <span className="tabular-nums">{formatCost(totalWasted)}</span> total
        </h3>
        <div
          className="flex items-center gap-1"
          role="group"
          aria-label="Severity filter"
          data-testid="retry-alerts-filters"
        >
          {FILTERS.map((f) => {
            const active = filter === f.id
            return (
              <button
                key={f.id}
                type="button"
                onClick={() => setFilter(f.id)}
                aria-pressed={active}
                data-testid={`retry-alerts-filter-${f.id}`}
                className={
                  'text-[11px] px-2 py-0.5 rounded-full border transition-colors ' +
                  (active
                    ? 'bg-indigo-500/20 border-indigo-500/60 text-indigo-200'
                    : 'bg-gray-100/90 dark:bg-gray-800/80 border-gray-300 dark:border-gray-700 text-gray-600 dark:text-gray-400 hover:text-gray-800 dark:hover:text-gray-200 hover:border-gray-400 dark:hover:border-gray-600')
                }
              >
                {f.label}
              </button>
            )
          })}
        </div>
      </div>

      {visible.length === 0 ? (
        <div
          className="text-xs text-gray-500 py-6 text-center"
          data-testid="retry-alerts-empty-filtered"
        >
          No signals match this severity filter.
        </div>
      ) : (
        <div className="space-y-2" data-testid="retry-alerts-list">
          {visible.map((sig, idx) => {
            const style = STYLES[severity(sig)]
            return (
              <button
                key={`${sig.interaction_id}-${sig.tool}-${idx}`}
                type="button"
                onClick={() => openInteraction(sig.interaction_id)}
                data-testid="retry-alerts-row"
                className={
                  'w-full text-left flex items-start gap-3 p-3 rounded border transition-colors ' +
                  `${style.wrapper} hover:brightness-125 focus:outline-none focus:ring-2 focus:ring-indigo-500/60`
                }
              >
                <IconAlertTriangle
                  size={16}
                  className={`${style.icon} mt-0.5 flex-shrink-0`}
                />
                <div className="flex-1 min-w-0">
                  <div className="flex items-center gap-2 flex-wrap">
                    <span className={`text-sm font-medium ${style.label}`}>{sig.tool}</span>
                    <span className="inline-flex items-center gap-1 text-[10px] text-gray-600 dark:text-gray-400 bg-gray-100/90 dark:bg-gray-800/80 border border-gray-300 dark:border-gray-700 rounded-full px-2 py-0.5">
                      <IconRefresh size={10} />
                      {sig.consecutive_failures}× failed · {sig.total_invocations} total
                    </span>
                    <span className="text-[10px] text-gray-500">{formatTime(sig.timestamp)}</span>
                  </div>
                  <div className="text-xs text-gray-600 dark:text-gray-400 mt-1 tabular-nums">
                    ~{formatTokens(sig.estimated_wasted_tokens)} wasted tokens ·{' '}
                    {formatCost(sig.estimated_wasted_cost)} wasted cost
                  </div>
                </div>
              </button>
            )
          })}
        </div>
      )}
    </div>
  )
}
