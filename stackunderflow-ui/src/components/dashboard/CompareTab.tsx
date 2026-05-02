import { useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { IconScale } from '@tabler/icons-react'
import { getCompare, type ComparePeriod } from '../../services/api'
import type { ModelStats } from '../../types/api'
import LoadingSpinner from '../common/LoadingSpinner'
import EmptyState from '../common/EmptyState'
import ProviderChip from '../common/ProviderChip'
import { formatCost, formatNumber, formatModelName } from '../../services/format'
import { useCurrency } from '../../services/currency'

// ---------------------------------------------------------------------------
// CompareTab — v0.6.0 follow-up.
//
// Surfaces `GET /api/compare`. The route already sorts rows by `total_cost`
// desc, so this component renders them in payload order. Period selector is
// a lightweight inline control so the tab stays self-contained — the main
// dashboard period control isn't yet wired through ProjectDashboard.
// ---------------------------------------------------------------------------

const PERIODS: { id: ComparePeriod; label: string }[] = [
  { id: 'today', label: 'Today' },
  { id: 'week', label: '7d' },
  { id: 'month', label: '30d' },
  { id: 'all', label: 'All' },
]

function formatPercent(value: number): string {
  // The compare service emits `one_shot_pct` as a 0..100 number and
  // `retry_rate` / `cache_hit_rate` as 0..1 floats. Branch on magnitude so
  // we can render both with the same helper. Anything > 1 we treat as a
  // percentage already.
  if (!Number.isFinite(value)) return '—'
  const pct = value > 1 ? value : value * 100
  return `${pct.toFixed(1)}%`
}

interface ColumnHeaderProps {
  label: string
  align?: 'left' | 'right'
  hint?: string
}

function ColumnHeader({ label, align = 'left', hint }: ColumnHeaderProps) {
  return (
    <th
      className={`px-3 py-2 text-[10px] uppercase tracking-wider text-gray-500 font-medium ${
        align === 'right' ? 'text-right' : 'text-left'
      }`}
      title={hint}
    >
      {label}
    </th>
  )
}

interface CompareRowProps {
  row: ModelStats
  currency: ReturnType<typeof useCurrency>['currency']
}

function CompareRow({ row, currency }: CompareRowProps) {
  return (
    <tr className="border-t border-gray-200 dark:border-gray-800 hover:bg-gray-50 dark:hover:bg-gray-800/40">
      <td
        className="px-3 py-2 text-xs text-gray-800 dark:text-gray-200 break-all"
        title={row.model}
      >
        {formatModelName(row.model)}
      </td>
      <td className="px-3 py-2">
        <ProviderChip provider={row.provider} />
      </td>
      <td className="px-3 py-2 text-right text-sm tabular-nums text-gray-700 dark:text-gray-300">
        {formatNumber(row.sessions)}
      </td>
      <td className="px-3 py-2 text-right text-sm tabular-nums text-gray-700 dark:text-gray-300">
        {formatNumber(row.calls)}
      </td>
      <td className="px-3 py-2 text-right text-sm tabular-nums text-gray-700 dark:text-gray-300">
        {formatPercent(row.one_shot_pct)}
      </td>
      <td className="px-3 py-2 text-right text-sm tabular-nums text-gray-700 dark:text-gray-300">
        {formatPercent(row.retry_rate)}
      </td>
      <td className="px-3 py-2 text-right text-sm tabular-nums text-gray-700 dark:text-gray-300">
        {formatPercent(row.cache_hit_rate)}
      </td>
      <td className="px-3 py-2 text-right text-sm tabular-nums text-gray-700 dark:text-gray-300">
        {formatCost(row.cost_per_call, currency)}
      </td>
      <td className="px-3 py-2 text-right text-sm tabular-nums text-gray-700 dark:text-gray-300">
        {formatCost(row.cost_per_session, currency)}
      </td>
      <td className="px-3 py-2 text-right text-sm tabular-nums font-medium text-gray-900 dark:text-gray-100">
        {formatCost(row.total_cost, currency)}
      </td>
    </tr>
  )
}

export default function CompareTab() {
  const { currency } = useCurrency()
  const [period, setPeriod] = useState<ComparePeriod>('month')

  const { data, isLoading, error } = useQuery({
    queryKey: ['compare', period],
    queryFn: () => getCompare(period),
    staleTime: 60_000,
  })

  return (
    <div className="space-y-4">
      {/* Header strip — period selector + tab description */}
      <div className="flex items-center justify-between gap-3 flex-wrap">
        <div className="flex items-center gap-2">
          <IconScale size={16} className="text-gray-500" />
          <h2 className="text-sm font-semibold text-gray-800 dark:text-gray-200">
            Per-model comparison
          </h2>
          <span className="text-xs text-gray-500">
            sessions, retry, cache, and unit economics side-by-side
          </span>
        </div>
        <div
          className="inline-flex rounded-md border border-gray-200 dark:border-gray-700 overflow-hidden"
          role="group"
          aria-label="Compare period"
        >
          {PERIODS.map(p => (
            <button
              key={p.id}
              type="button"
              onClick={() => setPeriod(p.id)}
              className={`px-3 py-1.5 text-xs font-medium transition-colors ${
                p.id === period
                  ? 'bg-indigo-500/10 text-indigo-600 dark:text-indigo-400'
                  : 'bg-white dark:bg-gray-900 text-gray-600 dark:text-gray-400 hover:text-gray-900 dark:hover:text-gray-200'
              }`}
            >
              {p.label}
            </button>
          ))}
        </div>
      </div>

      {isLoading && <LoadingSpinner message="Loading compare data..." />}

      {error && (
        <div className="bg-red-50 dark:bg-red-900/20 border border-red-300 dark:border-red-800 rounded-lg p-3 text-red-700 dark:text-red-400 text-sm">
          Failed to load compare data: {error instanceof Error ? error.message : 'Unknown error'}
        </div>
      )}

      {!isLoading && !error && data && data.models.length === 0 && (
        <EmptyState
          icon={<IconScale size={28} />}
          title="No sessions in window"
          description="Try a wider period, or run a session in this window to populate the comparison."
        />
      )}

      {!isLoading && !error && data && data.models.length > 0 && (
        <div className="overflow-x-auto rounded-lg border border-gray-200 dark:border-gray-800">
          <table className="w-full text-sm">
            <thead className="bg-gray-50 dark:bg-gray-800/60">
              <tr>
                <ColumnHeader label="Model" />
                <ColumnHeader label="Provider" />
                <ColumnHeader label="Sessions" align="right" />
                <ColumnHeader label="Calls" align="right" />
                <ColumnHeader
                  label="1-shot %"
                  align="right"
                  hint="Sessions resolved in a single user/assistant exchange"
                />
                <ColumnHeader
                  label="Retry"
                  align="right"
                  hint="(assistant_messages / sessions) − 1"
                />
                <ColumnHeader
                  label="Cache %"
                  align="right"
                  hint="cache_read / (cache_read + input)"
                />
                <ColumnHeader label="$/call" align="right" />
                <ColumnHeader label="$/session" align="right" />
                <ColumnHeader label="Total" align="right" />
              </tr>
            </thead>
            <tbody>
              {data.models.map(row => (
                <CompareRow key={`${row.model}|${row.provider}`} row={row} currency={currency} />
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  )
}
