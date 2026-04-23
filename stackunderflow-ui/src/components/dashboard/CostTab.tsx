import type { DashboardStats } from '../../types/api'
import TrendDeltaStrip from '../cost/TrendDeltaStrip'
import CacheRoiCard from '../cost/CacheRoiCard'
import ErrorCostCard from '../cost/ErrorCostCard'
import SessionCostBarChart from '../cost/SessionCostBarChart'
import CommandCostList from '../cost/CommandCostList'
import ToolCostBarChart from '../cost/ToolCostBarChart'
import TokenCompositionDonut from '../cost/TokenCompositionDonut'
import TokenCompositionStack from '../cost/TokenCompositionStack'
import OutlierCommandsTable from '../cost/OutlierCommandsTable'
import RetryAlertsPanel from '../cost/RetryAlertsPanel'

interface CostTabProps {
  stats: DashboardStats
}

/**
 * Cost tab — analytics-expansion spec §2.3. Layout:
 *   1. TrendDeltaStrip (full width)
 *   2. CacheRoiCard · ErrorCostCard (two columns)
 *   3. SessionCostBarChart (full width)
 *   4. CommandCostList (full width)
 *   5. ToolCostBarChart · TokenCompositionDonut (two columns)
 *   6. TokenCompositionStack (full width)
 *   7. OutlierCommandsTable (full width)
 *   8. RetryAlertsPanel (full width)
 *
 * Degrades gracefully when individual analytics fields are missing from the
 * cached payload — each child renders its own empty state.
 */
export default function CostTab({ stats }: CostTabProps) {
  const tokenTotals =
    stats.token_composition?.totals ??
    (stats.overview?.total_tokens
      ? {
          input: stats.overview.total_tokens.input ?? 0,
          output: stats.overview.total_tokens.output ?? 0,
          cache_read: stats.overview.total_tokens.cache_read ?? 0,
          cache_creation: stats.overview.total_tokens.cache_creation ?? 0,
        }
      : {})

  return (
    <div className="space-y-6">
      {/* 1. Trend strip full-width */}
      <TrendDeltaStrip trends={stats.trends} />

      {/* 2. Cache ROI · Error cost two-column */}
      <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
        <CacheRoiCard cache={stats.cache} />
        <ErrorCostCard errorCost={stats.error_cost} />
      </div>

      {/* 3. Session cost bar chart full-width */}
      <SessionCostBarChart data={stats.session_costs ?? []} />

      {/* 4. Command cost list full-width */}
      <CommandCostList data={stats.command_costs ?? []} />

      {/* 5. Tool cost · Token donut two-column */}
      <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
        <ToolCostBarChart data={stats.tool_costs ?? {}} />
        <TokenCompositionDonut totals={tokenTotals} />
      </div>

      {/* 6. Token composition stack full-width */}
      <TokenCompositionStack daily={stats.token_composition?.daily ?? {}} />

      {/* 7. Outlier commands table full-width */}
      <OutlierCommandsTable outliers={stats.outliers} />

      {/* 8. Retry alerts panel full-width */}
      <RetryAlertsPanel signals={stats.retry_signals ?? []} />
    </div>
  )
}
