import { useQuery } from '@tanstack/react-query'
import { IconCreditCard, IconCalendarStats, IconAlertTriangle } from '@tabler/icons-react'
import { getPlan } from '../../services/api'
import type { PlanProjection, PlanUsage } from '../../types/api'
import Badge from '../common/Badge'
import { formatCost } from '../../services/format'
import { useCurrency } from '../../services/currency'

// ---------------------------------------------------------------------------
// PlanBudgetCard — v0.6.0 follow-up.
//
// Renders only when `GET /api/plan` returns a configured plan; otherwise
// returns null so the Overview tab doesn't carry an empty card. Status
// banding (`ok` < 80% < `warn` ≤ 100% < `over`) comes straight from the
// route — we just translate it to a Badge color.
// ---------------------------------------------------------------------------

type StatusColor = 'green' | 'yellow' | 'red' | 'gray'

const STATUS_COLOR: Record<PlanUsage['status'], StatusColor> = {
  ok: 'green',
  warn: 'yellow',
  over: 'red',
}

function statusBarColor(status: PlanUsage['status']): string {
  if (status === 'over') return 'bg-red-500'
  if (status === 'warn') return 'bg-yellow-500'
  return 'bg-green-500'
}

/**
 * Map a projection's alert / status to the amber-vs-red colour the
 * forecast banner uses. Burn-projector v2 surfaces alerts at
 * configurable thresholds (50 / 75 / 90 by default) so we want a
 * graduated palette rather than reusing the binary status colour:
 *
 *  - over budget (`crossed_threshold >= 100` or status === 'over') → red
 *  - any other crossed threshold → amber
 *  - no alert → no banner (caller handles null)
 */
function forecastBannerStyle(
  status: PlanUsage['status'],
  projection: PlanProjection,
): { bg: string; text: string; border: string } {
  const isRed =
    status === 'over' || (projection.crossed_threshold ?? 0) >= 100
  if (isRed) {
    return {
      bg: 'bg-red-50 dark:bg-red-950/30',
      text: 'text-red-800 dark:text-red-300',
      border: 'border-red-200 dark:border-red-900',
    }
  }
  return {
    bg: 'bg-amber-50 dark:bg-amber-950/30',
    text: 'text-amber-800 dark:text-amber-300',
    border: 'border-amber-200 dark:border-amber-900',
  }
}

function formatDate(iso: string): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  return d.toLocaleDateString(undefined, { month: 'short', day: 'numeric' })
}

export default function PlanBudgetCard() {
  const { currency } = useCurrency()
  const { data, isLoading } = useQuery({
    queryKey: ['plan'],
    queryFn: getPlan,
    // Plan rarely changes mid-day; cache aggressively.
    staleTime: 5 * 60_000,
  })

  // Hide the card entirely when no plan is configured. Same posture if the
  // request is in flight — flashing an empty card would be worse than waiting.
  if (isLoading || !data || !data.plan || !data.usage) return null

  const { plan, usage } = data
  // Burn-projector v2 — `projection` is optional on the response type so
  // older servers (pre-v0.8.x) keep working. When absent we fall back to
  // the legacy `usage.projected` number for the Projected detail cell.
  const projection = data.projection ?? null
  // Clamp to [0, 100] so the bar doesn't overflow the rail when usage is
  // 120% of budget — the badge already says "over" in that case.
  const barPct = Math.min(100, Math.max(0, usage.pct))
  const projectedAmount = projection?.projected_month_end_usd ?? usage.projected
  const banner = projection?.alert
    ? forecastBannerStyle(usage.status, projection)
    : null

  return (
    <div className="bg-white dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-800 p-4">
      <div className="flex items-center justify-between gap-3 flex-wrap mb-3">
        <div className="flex items-center gap-2 min-w-0">
          <IconCreditCard size={16} className="text-gray-500 flex-shrink-0" />
          <h3 className="text-sm font-semibold text-gray-800 dark:text-gray-200 truncate">
            {plan.name}
          </h3>
          <Badge color={STATUS_COLOR[usage.status]} size="sm">
            {usage.status === 'ok' ? 'on track' : usage.status === 'warn' ? 'warning' : 'over budget'}
          </Badge>
        </div>
        <div className="flex items-center gap-1.5 text-xs text-gray-500">
          <IconCalendarStats size={12} />
          <span>
            {formatDate(usage.period_start)} — {formatDate(usage.period_end)}
          </span>
          <span className="text-gray-400">·</span>
          <span>
            day {usage.days_so_far}/{usage.days_in_period}
          </span>
        </div>
      </div>

      {/* Progress bar */}
      <div className="space-y-1.5">
        <div className="h-2 bg-gray-200 dark:bg-gray-800 rounded-full overflow-hidden">
          <div
            className={`h-full ${statusBarColor(usage.status)} transition-all`}
            style={{ width: `${barPct}%` }}
            aria-label={`${usage.pct.toFixed(1)}% of budget used`}
          />
        </div>
        <div className="flex items-center justify-between text-xs text-gray-500">
          <span>
            <span className="font-medium text-gray-700 dark:text-gray-300">
              {formatCost(usage.used, currency)}
            </span>{' '}
            of {formatCost(usage.budget, currency)}
          </span>
          <span className="tabular-nums">{usage.pct.toFixed(1)}%</span>
        </div>
      </div>

      {/* Detail row */}
      <div className="grid grid-cols-3 gap-3 mt-4 pt-3 border-t border-gray-100 dark:border-gray-800">
        <div>
          <div className="text-[10px] uppercase tracking-wider text-gray-500">Remaining</div>
          <div className="text-sm font-medium text-gray-900 dark:text-gray-100 tabular-nums">
            {formatCost(usage.remaining, currency)}
          </div>
        </div>
        <div>
          <div className="text-[10px] uppercase tracking-wider text-gray-500">Projected</div>
          <div className="text-sm font-medium text-gray-900 dark:text-gray-100 tabular-nums">
            {formatCost(projectedAmount, currency)}
          </div>
          {projection && (
            <div className="text-[10px] text-gray-400 mt-0.5">
              {projection.projection_method === 'weighted-7d' ? 'weighted 7d' : 'linear'}
            </div>
          )}
        </div>
        <div>
          <div className="text-[10px] uppercase tracking-wider text-gray-500">Budget</div>
          <div className="text-sm font-medium text-gray-900 dark:text-gray-100 tabular-nums">
            {formatCost(plan.monthly_usd, currency)}
          </div>
        </div>
      </div>

      {/* Burn-projector v2 — daily-burn + days-to-limit strip. Renders only
       * when the route returned a `projection` block and there's something
       * substantive to show (a daily burn rate or a "days to limit" hint).
       * Older servers (pre-v0.8.x) won't ship `projection` so the strip
       * stays hidden — the legacy detail row above keeps the card useful.
       */}
      {projection && (projection.daily_burn_usd > 0 || projection.days_to_limit !== null) && (
        <div className="mt-3 pt-3 border-t border-gray-100 dark:border-gray-800 flex items-center justify-between text-xs text-gray-500">
          <span>
            <span className="font-medium text-gray-700 dark:text-gray-300 tabular-nums">
              {formatCost(projection.daily_burn_usd, currency)}
            </span>{' '}
            / day burn
          </span>
          {projection.days_to_limit !== null && (
            <span className="tabular-nums">
              ~{projection.days_to_limit} day{projection.days_to_limit === 1 ? '' : 's'} to limit
            </span>
          )}
        </div>
      )}

      {/* Alert banner — surfaced only when the projection module produced
       * a string. Threshold-crossed and projected-overrun share the same
       * banner slot; the priority is encoded server-side so we just render
       * whatever the route returned.
       */}
      {projection?.alert && banner && (
        <div
          className={`mt-3 px-3 py-2 rounded-md border text-xs flex items-center gap-2 ${banner.bg} ${banner.text} ${banner.border}`}
          role="status"
        >
          <IconAlertTriangle size={14} className="flex-shrink-0" />
          <span>{projection.alert}</span>
        </div>
      )}
    </div>
  )
}
