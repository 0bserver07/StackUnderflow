import { memo } from 'react'
import { useQuery } from '@tanstack/react-query'
import { IconActivityHeartbeat, IconCalendarStats, IconMessage } from '@tabler/icons-react'
import { getOptimize } from '../../services/api'
import { useFilters } from '../../services/filters'
import type { CostAnomaly } from '../../types/api'
import Badge from '../common/Badge'
import { formatCost } from '../../services/format'

// ---------------------------------------------------------------------------
// CostAnomaliesPanel — audit #7 part 1.
//
// Surfaces `GET /api/optimize?period=month` → `anomalies`: days / sessions
// whose dollar cost is a statistical outlier vs the project's own rolling
// baseline (median + MAD, with a 2σ stddev fallback). Shares the exact
// `getOptimize('month', filters)` query key with OptimizeFindingsPanel so
// React Query serves both panels from one fetch.
//
// Self-hides when there are no anomalies so a healthy install shows nothing.
// ---------------------------------------------------------------------------

const TOP_N = 6

function AnomalyRow({ anomaly }: { anomaly: CostAnomaly }) {
  const isDay = anomaly.kind === 'day'
  const Icon = isDay ? IconCalendarStats : IconMessage
  // Sessions key on an opaque id; show a short prefix so the row stays legible.
  const label = isDay ? anomaly.key : `${anomaly.key.slice(0, 8)}…`
  const model =
    typeof anomaly.details?.model === 'string' ? (anomaly.details.model as string) : null

  return (
    <div className="px-4 py-3 border-t border-gray-100 dark:border-gray-800 flex items-start gap-3">
      <div className="flex-shrink-0 pt-0.5 text-gray-400">
        <Icon size={16} />
      </div>
      <div className="flex-1 min-w-0">
        <div className="flex items-baseline gap-2 flex-wrap">
          <span className="text-sm font-medium text-gray-900 dark:text-gray-100 tabular-nums">
            {label}
          </span>
          <Badge color="red" size="sm">
            {formatCost(anomaly.cost_usd)}
          </Badge>
          {anomaly.ratio !== null && anomaly.ratio >= 1.5 && (
            <span className="text-[11px] text-rose-600 dark:text-rose-400 tabular-nums">
              {anomaly.ratio.toFixed(1)}× baseline
            </span>
          )}
          {model && (
            <span className="text-[11px] text-gray-500 truncate max-w-[40%]">{model}</span>
          )}
        </div>
        <p className="text-xs text-gray-600 dark:text-gray-400 mt-1">{anomaly.reason}</p>
      </div>
    </div>
  )
}

interface CostAnomaliesPanelProps {
  /** Scope to one project slug; absent = whole store. */
  projectSlug?: string
}

function CostAnomaliesPanel({ projectSlug }: CostAnomaliesPanelProps) {
  const { filters } = useFilters()
  const { data, isLoading, error } = useQuery({
    queryKey: ['optimize', 'month', filters.providers, filters.models, projectSlug ?? null],
    queryFn: () =>
      getOptimize('month', { providers: filters.providers, models: filters.models }, projectSlug),
    staleTime: 5 * 60_000,
  })

  // Supplementary panel — stay quiet while loading / on error rather than
  // flashing a spinner above the primary stats.
  if (isLoading || error || !data) return null

  const anomalies = data.anomalies?.anomalies ?? []
  if (anomalies.length === 0) return null

  const visible = anomalies.slice(0, TOP_N)
  const dayCount = data.anomalies?.day_count ?? 0

  return (
    <div className="bg-white dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-800">
      <div className="w-full flex items-center justify-between gap-3 px-4 py-3">
        <div className="flex items-center gap-2">
          <IconActivityHeartbeat size={16} className="text-rose-500" />
          <span className="text-sm font-semibold text-gray-800 dark:text-gray-200">
            Cost anomalies
          </span>
          <span className="text-xs text-gray-500">
            {anomalies.length} outlier{anomalies.length === 1 ? '' : 's'} · {data.scope}
          </span>
        </div>
        {dayCount > 0 && (
          <span className="text-[11px] text-gray-400 tabular-nums">
            vs {dayCount}-day baseline
          </span>
        )}
      </div>

      <div>
        {visible.map((a) => (
          <AnomalyRow key={`${a.kind}-${a.key}`} anomaly={a} />
        ))}
      </div>
    </div>
  )
}

export default memo(CostAnomaliesPanel)
