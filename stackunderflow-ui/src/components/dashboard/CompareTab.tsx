import { useMemo, useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { IconScale } from '@tabler/icons-react'
import { getCompare, type ComparePeriod } from '../../services/api'
import type { ModelStats } from '../../types/api'
import LoadingSpinner from '../common/LoadingSpinner'
import EmptyState from '../common/EmptyState'
import ProviderChip from '../common/ProviderChip'
import { formatCost, formatNumber, formatModelName } from '../../services/format'
import { useCurrency } from '../../services/currency'
import { shortenModelId } from '../../services/providerStyle'

// ---------------------------------------------------------------------------
// CompareTab — v0.6.1 multi-provider polish.
//
// Surfaces `GET /api/compare`. The route already sorts rows by `total_cost`
// desc and emits one row per `(provider, model)` pair, so by default we
// render that shape verbatim ("Agent × Model"). The new "Group by" toggle
// also offers a "Model only" mode that client-side aggregates the rows by
// `model` alone — useful for "what's my total Opus spend regardless of
// which agent invoked it".
//
// The group-by UI matters because the same model id (e.g.
// `claude-4.5-sonnet-thinking`) can show up under both `claude` and
// `cursor`; collapsing them silently was hiding the per-agent efficiency
// comparison the dashboard exists to surface.
// ---------------------------------------------------------------------------

const PERIODS: { id: ComparePeriod; label: string }[] = [
  { id: 'today', label: 'Today' },
  { id: 'week', label: '7d' },
  { id: 'month', label: '30d' },
  { id: 'all', label: 'All' },
]

type GroupMode = 'agent_model' | 'model_only'

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
  showProviderChip: boolean
  currency: ReturnType<typeof useCurrency>['currency']
}

function CompareRow({ row, showProviderChip, currency }: CompareRowProps) {
  return (
    <tr className="border-t border-gray-200 dark:border-gray-800 hover:bg-gray-50 dark:hover:bg-gray-800/40">
      <td className="px-3 py-2">
        <div className="inline-flex items-center gap-2 min-w-0">
          {showProviderChip && <ProviderChip provider={row.provider} />}
          <span
            className="text-xs text-gray-800 dark:text-gray-200 truncate"
            title={row.model}
          >
            {formatModelName(row.model)}
          </span>
        </div>
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

/**
 * Collapse `(provider, model)` rows down to one row per `model`. Sums
 * sessions / calls / costs / tokens; recomputes per-row rates from the
 * summed totals so they stay self-consistent.
 *
 * The aggregator stamps `provider = "(combined)"` on every output row —
 * the caller passes `showProviderChip={false}` in this mode anyway so the
 * value never reaches the UI, but keeping it explicit makes the shape
 * round-trippable.
 */
function aggregateByModel(rows: ModelStats[]): ModelStats[] {
  const grouped = new Map<string, {
    sessions: number
    calls: number
    one_shot_sessions: number
    assistant_msgs: number
    cache_read_proxy: number
    cache_total_proxy: number
    total_cost: number
    total_tokens: number
  }>()

  // For one-shot %, retry rate, and cache hit rate we don't have raw
  // numerator/denominator counts on each row — only the rates. Rebuild
  // the numerators by multiplying back into the available denominators
  // (sessions for one-shot, sessions for retry, total cacheable proxy
  // for cache hit). This matches the math the backend uses so combined
  // rows stay in the same ballpark as their ungrouped components.
  for (const r of rows) {
    const existing = grouped.get(r.model) ?? {
      sessions: 0,
      calls: 0,
      one_shot_sessions: 0,
      assistant_msgs: 0,
      cache_read_proxy: 0,
      cache_total_proxy: 0,
      total_cost: 0,
      total_tokens: 0,
    }
    existing.sessions += r.sessions
    existing.calls += r.calls
    existing.total_cost += r.total_cost
    existing.total_tokens += r.total_tokens
    // one_shot_sessions ≈ one_shot_pct * sessions (rate is 0..1 float)
    existing.one_shot_sessions += (r.one_shot_pct ?? 0) * r.sessions
    // retry_rate = (assistant_msgs / sessions) - 1 → assistant_msgs ≈ (1 + retry_rate) * sessions
    existing.assistant_msgs += (1 + (r.retry_rate ?? 0)) * r.sessions
    // cache_hit_rate ≈ cache_read / cacheable. Use calls as the proxy
    // weight so models with more traffic dominate the average — we'd need
    // raw token counts to do this properly, but per-row weighting is
    // strictly better than the unweighted mean.
    existing.cache_read_proxy += (r.cache_hit_rate ?? 0) * r.calls
    existing.cache_total_proxy += r.calls
    grouped.set(r.model, existing)
  }

  const out: ModelStats[] = []
  for (const [model, agg] of grouped) {
    const sessions = agg.sessions
    const calls = agg.calls
    out.push({
      model,
      provider: '(combined)',
      sessions,
      calls,
      one_shot_pct: sessions ? agg.one_shot_sessions / sessions : 0,
      retry_rate: sessions ? agg.assistant_msgs / sessions - 1 : 0,
      cache_hit_rate: agg.cache_total_proxy ? agg.cache_read_proxy / agg.cache_total_proxy : 0,
      cost_per_call: calls ? agg.total_cost / calls : 0,
      cost_per_session: sessions ? agg.total_cost / sessions : 0,
      total_cost: agg.total_cost,
      total_tokens: agg.total_tokens,
    })
  }
  out.sort((a, b) => b.total_cost - a.total_cost)
  return out
}

interface GroupToggleProps {
  mode: GroupMode
  onChange: (m: GroupMode) => void
}

function GroupToggle({ mode, onChange }: GroupToggleProps) {
  const options: { id: GroupMode; label: string; hint: string }[] = [
    {
      id: 'agent_model',
      label: 'Agent × Model',
      hint: 'One row per (provider, model) — same model under different agents shown separately.',
    },
    {
      id: 'model_only',
      label: 'Model only',
      hint: 'Sum across providers for the same model id.',
    },
  ]
  return (
    <div
      className="inline-flex rounded-md border border-gray-200 dark:border-gray-700 overflow-hidden"
      role="group"
      aria-label="Compare grouping"
    >
      {options.map((o) => (
        <button
          key={o.id}
          type="button"
          onClick={() => onChange(o.id)}
          aria-pressed={o.id === mode}
          title={o.hint}
          className={`px-3 py-1.5 text-xs font-medium transition-colors ${
            o.id === mode
              ? 'bg-emerald-500/10 text-emerald-600 dark:text-emerald-400'
              : 'bg-white dark:bg-gray-900 text-gray-600 dark:text-gray-400 hover:text-gray-900 dark:hover:text-gray-200'
          }`}
        >
          {o.label}
        </button>
      ))}
    </div>
  )
}

export default function CompareTab() {
  const { currency } = useCurrency()
  const [period, setPeriod] = useState<ComparePeriod>('month')
  const [groupMode, setGroupMode] = useState<GroupMode>('agent_model')

  const { data, isLoading, error } = useQuery({
    queryKey: ['compare', period],
    queryFn: () => getCompare(period),
    staleTime: 60_000,
  })

  const visibleRows = useMemo(() => {
    if (!data) return [] as ModelStats[]
    if (groupMode === 'agent_model') return data.models
    return aggregateByModel(data.models)
  }, [data, groupMode])

  return (
    <div className="space-y-4">
      {/* Header strip — group-by + period selectors + tab description */}
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
        <div className="flex items-center gap-2 flex-wrap">
          <GroupToggle mode={groupMode} onChange={setGroupMode} />
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
                <ColumnHeader
                  label={groupMode === 'agent_model' ? 'Agent × Model' : 'Model'}
                  hint={
                    groupMode === 'agent_model'
                      ? 'Provider chip + model id. Same model under different providers renders as distinct rows.'
                      : 'Aggregated across all providers per model id.'
                  }
                />
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
              {visibleRows.map(row => (
                <CompareRow
                  key={
                    groupMode === 'agent_model'
                      ? `${row.model}|${row.provider}`
                      : `model|${row.model}`
                  }
                  row={row}
                  showProviderChip={groupMode === 'agent_model'}
                  currency={currency}
                />
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  )
}

// Exported for tests so the aggregator math doesn't need to be re-derived.
export { aggregateByModel }
