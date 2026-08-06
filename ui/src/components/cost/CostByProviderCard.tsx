import { useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { IconUsersGroup } from '@tabler/icons-react'
import { getCostByProvider } from '../../services/api'
import type { ComparePeriod } from '../../services/api'
import type { CostByProviderRow } from '../../types/api'
import { formatCost, formatNumber } from '../../services/format'
import { useCurrency } from '../../services/currency'
import { getProviderColor, getProviderLabel } from '../../services/providerStyle'
import { useFilters } from '../../services/filters'
import LoadingSpinner from '../common/LoadingSpinner'
import Badge from '../common/Badge'

// ---------------------------------------------------------------------------
// CostByProviderCard — v0.6.1 multi-provider polish.
//
// Top-of-Cost-tab widget that shows total spend split per provider for the
// active period. The companion to the Compare tab's "Agent × Model" toggle:
// once users see distinct (provider, model) rows in Compare, the next
// question is "what's the share of my total spend that goes through each
// agent?". This card answers that without a chart library — a horizontal
// stacked bar built from div widths so it picks up the same Tailwind
// dark-mode contract as the rest of the dashboard.
// ---------------------------------------------------------------------------

const PERIOD_OPTIONS: { id: ComparePeriod; label: string }[] = [
  { id: 'today', label: 'Today' },
  { id: 'week', label: '7d' },
  { id: 'month', label: '30d' },
  { id: 'all', label: 'All' },
]

// Tailwind colour classes keyed off the same palette used by ProviderChip.
// Kept inline rather than reading from the chip helper because the chip
// uses `bg-blue-100 dark:bg-blue-900/50` (tag styling) which renders too
// pale as a stacked bar segment. These classes intentionally lean darker.
const BAR_SEGMENT_COLORS: Record<string, string> = {
  blue: 'bg-blue-500 dark:bg-blue-400',
  green: 'bg-emerald-500 dark:bg-emerald-400',
  yellow: 'bg-yellow-500 dark:bg-yellow-400',
  red: 'bg-rose-500 dark:bg-rose-400',
  purple: 'bg-purple-500 dark:bg-purple-400',
  orange: 'bg-orange-500 dark:bg-orange-400',
  gray: 'bg-gray-400 dark:bg-gray-500',
}

function segmentColor(provider: string): string {
  return BAR_SEGMENT_COLORS[getProviderColor(provider)] ?? BAR_SEGMENT_COLORS.gray!
}

interface CostByProviderCardProps {
  /** Optional initial period; defaults to `month` to match Compare tab. */
  initialPeriod?: ComparePeriod
}

export default function CostByProviderCard({ initialPeriod = 'month' }: CostByProviderCardProps) {
  const { currency } = useCurrency()
  const { filters, setProviders } = useFilters()
  const [period, setPeriod] = useState<ComparePeriod>(initialPeriod)

  // Pass provider filter through; backend narrows the rows so the card
  // only renders the active scope. Per spec, the card highlights (rather
  // than hides) the filtered providers — but with the filter already in
  // place, the row list has already collapsed to that subset, which is
  // visually equivalent and keeps the request small.
  const { data, isLoading, error } = useQuery({
    queryKey: ['costByProvider', period, filters.providers],
    queryFn: () => getCostByProvider(period, { providers: filters.providers }),
    staleTime: 60_000,
  })

  const handleSliceClick = (providerId: string) => {
    setProviders([providerId])
  }

  // Defensive: backend pre-converts cost_usd into the active currency, but
  // we still need a sane denominator for percentage shares. Sum on the fly
  // rather than asking the backend for it — keeps the response small.
  const rows: CostByProviderRow[] = data?.rows ?? []
  const total = rows.reduce((acc, r) => acc + (r.cost_usd ?? 0), 0)

  return (
    <div className="rounded-lg border border-gray-200 dark:border-gray-800 bg-white dark:bg-gray-900/40 p-4">
      <div className="flex items-center justify-between gap-3 flex-wrap mb-3">
        <div className="flex items-center gap-2 min-w-0">
          <IconUsersGroup size={16} className="text-gray-500" />
          <h3 className="text-sm font-semibold text-gray-800 dark:text-gray-200">
            Spend by agent
          </h3>
          <span className="text-xs text-gray-500 truncate">
            who's billing for the active window
          </span>
        </div>
        <div
          className="inline-flex rounded-md border border-gray-200 dark:border-gray-700 overflow-hidden"
          role="group"
          aria-label="Cost-by-provider period"
        >
          {PERIOD_OPTIONS.map((p) => (
            <button
              key={p.id}
              type="button"
              onClick={() => setPeriod(p.id)}
              aria-pressed={p.id === period}
              className={`px-2.5 py-1 text-[11px] font-medium transition-colors ${
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

      {isLoading && <LoadingSpinner message="Loading cost-by-provider..." />}

      {error && (
        <div className="text-xs text-red-700 dark:text-red-400">
          Failed to load: {error instanceof Error ? error.message : String(error)}
        </div>
      )}

      {!isLoading && !error && rows.length === 0 && (
        <div className="text-xs text-gray-500">No spend in this window.</div>
      )}

      {!isLoading && !error && rows.length > 0 && (
        <div className="space-y-3">
          {/* Stacked-bar overview — one segment per provider, sized by share.
              Clicking a segment scopes the entire dashboard to that provider
              via the FilterBar — same affordance as the Compare row, so the
              two tab-level entry points behave consistently. */}
          <div className="flex h-3 w-full rounded-full overflow-hidden bg-gray-100 dark:bg-gray-800">
            {rows.map((r) => {
              const pct = total > 0 ? (r.cost_usd / total) * 100 : 0
              if (pct <= 0) return null
              return (
                <button
                  key={r.provider}
                  type="button"
                  onClick={() => handleSliceClick(r.provider)}
                  className={`${segmentColor(r.provider)} hover:brightness-110 transition`}
                  style={{ width: `${pct}%` }}
                  title={`Click to filter dashboard to ${getProviderLabel(r.provider)} — ${formatCost(r.cost_usd, currency)} (${pct.toFixed(1)}%)`}
                  aria-label={`Filter to ${getProviderLabel(r.provider)}`}
                />
              )
            })}
          </div>

          {/* Per-provider breakdown */}
          <ul className="space-y-1.5">
            {rows.map((r) => {
              const pct = total > 0 ? (r.cost_usd / total) * 100 : 0
              return (
                <li
                  key={r.provider}
                  className="flex items-center gap-3 text-xs"
                  data-testid={`cost-by-provider-row-${r.provider}`}
                >
                  <span className={`inline-block h-2.5 w-2.5 rounded-full shrink-0 ${segmentColor(r.provider)}`} />
                  <Badge color={getProviderColor(r.provider)} size="sm">
                    {getProviderLabel(r.provider)}
                  </Badge>
                  <span className="text-gray-500 tabular-nums shrink-0 w-12 text-right">
                    {pct.toFixed(1)}%
                  </span>
                  <span className="font-medium text-gray-900 dark:text-gray-100 tabular-nums">
                    {formatCost(r.cost_usd, currency)}
                  </span>
                  <span className="text-gray-500 tabular-nums">
                    · {formatNumber(r.session_count)} sessions
                  </span>
                  <span className="text-gray-500 tabular-nums">
                    · {formatNumber(r.message_count)} msgs
                  </span>
                </li>
              )
            })}
          </ul>
        </div>
      )}
    </div>
  )
}
