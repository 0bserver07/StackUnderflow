import { IconArrowRight, IconTrendingDown, IconTrendingUp } from '@tabler/icons-react'
import type { CurrencyInfo, RoutingRecommendation } from '../../types/api'
import Badge from '../common/Badge'
import { formatCost, formatNumber } from '../../services/format'

// ---------------------------------------------------------------------------
// RoutingRecCard — one "route work-type X to model Y" prescription.
//
// Delta sign follows the /api/whatif convention: candidate − actual, so a
// negative monthly delta is a saving (rendered green); a positive one is an
// investment (the upshift-for-quality rule) rendered amber.
// ---------------------------------------------------------------------------

interface Props {
  rec: RoutingRecommendation
  currency?: CurrencyInfo | null
}

export default function RoutingRecCard({ rec, currency }: Props) {
  const monthly = rec.estimated_monthly_delta_usd
  const saves = (monthly ?? rec.window_delta_usd) < 0

  return (
    <div className="rounded-lg border border-gray-200 dark:border-gray-800 bg-white dark:bg-gray-900 p-4">
      <div className="flex items-start justify-between gap-3">
        <div className="flex items-center gap-2 min-w-0">
          {saves ? (
            <IconTrendingDown size={16} className="flex-shrink-0 text-emerald-600 dark:text-emerald-400" />
          ) : (
            <IconTrendingUp size={16} className="flex-shrink-0 text-amber-600 dark:text-amber-400" />
          )}
          <span className="text-sm font-medium text-gray-900 dark:text-gray-100 truncate">
            {rec.work_type}
          </span>
          <Badge color={saves ? 'green' : 'orange'} size="sm">
            {saves ? 'saving' : 'quality investment'}
          </Badge>
        </div>
        {monthly !== null && (
          <span
            className={`text-sm font-semibold tabular-nums flex-shrink-0 ${
              saves
                ? 'text-emerald-600 dark:text-emerald-400'
                : 'text-amber-600 dark:text-amber-400'
            }`}
          >
            {saves ? '−' : '+'}
            {formatCost(Math.abs(monthly), currency)}/mo
          </span>
        )}
      </div>

      <div className="mt-2 flex items-center gap-1.5 text-xs text-gray-700 dark:text-gray-300 font-mono">
        <span className="truncate">{rec.from_model}</span>
        <IconArrowRight size={12} className="flex-shrink-0 text-gray-400" />
        <span className="truncate font-semibold">{rec.to_label}</span>
      </div>

      <p className="mt-2 text-xs text-gray-600 dark:text-gray-400">{rec.rationale}</p>

      <div className="mt-2 flex flex-wrap gap-x-3 gap-y-1 text-[11px] text-gray-500 tabular-nums">
        <span>
          window: {formatCost(rec.window_cost_usd, currency)} →{' '}
          {formatCost(rec.candidate_window_cost_usd, currency)}
        </span>
        <span>{formatNumber(rec.evidence.events)} events</span>
        <span>{formatNumber(rec.evidence.sessions)} sessions</span>
        {rec.evidence.reasoning_tokens > 0 && (
          <span>{(rec.evidence.reasoning_share * 100).toFixed(1)}% reasoning</span>
        )}
        {rec.evidence.avg_quality_score !== null && (
          <span>
            quality {rec.evidence.avg_quality_score.toFixed(1)}/5 (n=
            {rec.evidence.graded_sessions})
          </span>
        )}
      </div>

      {rec.caveats.length > 0 && (
        <ul className="mt-2 space-y-0.5">
          {rec.caveats.map((c, i) => (
            <li key={i} className="text-[11px] text-gray-400 dark:text-gray-500">
              ⚠ {c}
            </li>
          ))}
        </ul>
      )}
    </div>
  )
}
