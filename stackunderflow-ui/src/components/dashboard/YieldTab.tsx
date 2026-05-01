import { useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import {
  IconAlertTriangle,
  IconCircleCheck,
  IconArrowBackUp,
  IconCircleX,
  IconGitBranch,
  IconTrendingUp,
} from '@tabler/icons-react'
import { getYield, type YieldPeriod } from '../../services/api'
import type { YieldClassification, YieldEntry, YieldSummary } from '../../types/api'
import LoadingSpinner from '../common/LoadingSpinner'
import EmptyState from '../common/EmptyState'
import Badge from '../common/Badge'
import { formatCost } from '../../services/format'
import { useCurrency } from '../../services/currency'

// ---------------------------------------------------------------------------
// YieldTab — v0.6.0 follow-up.
//
// Surfaces `GET /api/yield`. The response carries a `warning` string with
// the heuristic caveat — we render it as a banner near the top so it's
// impossible to look at the breakdown without seeing the disclaimer.
// ---------------------------------------------------------------------------

const PERIODS: { id: YieldPeriod; label: string }[] = [
  { id: 'today', label: 'Today' },
  { id: 'week', label: '7d' },
  { id: 'month', label: '30d' },
  { id: 'all', label: 'All' },
]

type ChipMeta = {
  label: string
  color: 'green' | 'red' | 'yellow' | 'gray'
}

const CLASSIFICATION_META: Record<YieldClassification, ChipMeta> = {
  productive: { label: 'productive', color: 'green' },
  reverted: { label: 'reverted', color: 'red' },
  abandoned: { label: 'abandoned', color: 'yellow' },
  no_repo: { label: 'no repo', color: 'gray' },
}

interface SummaryCardProps {
  icon: React.ReactNode
  title: string
  count: number
  cost: number
  color: string
  currency: ReturnType<typeof useCurrency>['currency']
}

function SummaryCard({ icon, title, count, cost, color, currency }: SummaryCardProps) {
  return (
    <div className="bg-white dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-800 p-4">
      <div className="flex items-center gap-2 mb-2">
        <span className={color}>{icon}</span>
        <span className="text-xs uppercase tracking-wider text-gray-500 font-medium">{title}</span>
      </div>
      <div className="text-2xl font-bold text-gray-900 dark:text-gray-100 tabular-nums">
        {count.toLocaleString()}
      </div>
      <div className="text-xs text-gray-500 mt-1 tabular-nums">{formatCost(cost, currency)}</div>
    </div>
  )
}

function truncate(text: string | null, max = 80): string {
  if (!text) return '—'
  return text.length > max ? `${text.slice(0, max - 1)}…` : text
}

function formatStarted(iso: string): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  return d.toLocaleString(undefined, {
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
  })
}

function formatHours(h: number | null): string {
  if (h === null || !Number.isFinite(h)) return '—'
  if (h < 1) return `${Math.round(h * 60)}m`
  if (h < 24) return `${h.toFixed(1)}h`
  return `${(h / 24).toFixed(1)}d`
}

interface YieldRowProps {
  entry: YieldEntry
  currency: ReturnType<typeof useCurrency>['currency']
}

function YieldRow({ entry, currency }: YieldRowProps) {
  const meta = CLASSIFICATION_META[entry.classification]
  return (
    <tr className="border-t border-gray-200 dark:border-gray-800 hover:bg-gray-50 dark:hover:bg-gray-800/40">
      <td className="px-3 py-2 text-xs text-gray-700 dark:text-gray-300 whitespace-nowrap">
        {formatStarted(entry.started_at)}
      </td>
      <td className="px-3 py-2 text-xs font-mono text-gray-800 dark:text-gray-200 break-all max-w-[220px]">
        {entry.project_slug || '—'}
      </td>
      <td className="px-3 py-2">
        <Badge color={meta.color} size="sm">
          {meta.label}
        </Badge>
      </td>
      <td className="px-3 py-2 text-xs text-gray-600 dark:text-gray-400 max-w-[360px]">
        {truncate(entry.follow_commit_msg)}
      </td>
      <td className="px-3 py-2 text-xs text-gray-600 dark:text-gray-400 text-right tabular-nums whitespace-nowrap">
        {formatHours(entry.follow_commit_age_hours)}
      </td>
      <td className="px-3 py-2 text-sm tabular-nums text-gray-900 dark:text-gray-100 font-medium text-right whitespace-nowrap">
        {formatCost(entry.cost_usd, currency)}
      </td>
    </tr>
  )
}

function HeuristicBanner({ warning }: { warning: string }) {
  return (
    <div className="flex items-start gap-2 bg-yellow-50 dark:bg-yellow-900/20 border border-yellow-300 dark:border-yellow-800 rounded-md p-3 text-yellow-800 dark:text-yellow-300 text-xs">
      <IconAlertTriangle size={14} className="flex-shrink-0 mt-0.5" />
      <span>{warning}</span>
    </div>
  )
}

export default function YieldTab() {
  const { currency } = useCurrency()
  const [period, setPeriod] = useState<YieldPeriod>('week')

  const { data, isLoading, error } = useQuery({
    queryKey: ['yield', period],
    queryFn: () => getYield(period),
    staleTime: 60_000,
  })

  const summary: YieldSummary | undefined = data?.summary

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between gap-3 flex-wrap">
        <div className="flex items-center gap-2">
          <IconGitBranch size={16} className="text-gray-500" />
          <h2 className="text-sm font-semibold text-gray-800 dark:text-gray-200">
            Yield analysis
          </h2>
          <span className="text-xs text-gray-500">
            productive vs reverted vs abandoned, by git follow-up
          </span>
        </div>
        <div
          className="inline-flex rounded-md border border-gray-200 dark:border-gray-700 overflow-hidden"
          role="group"
          aria-label="Yield period"
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

      {data?.warning && <HeuristicBanner warning={data.warning} />}

      {isLoading && <LoadingSpinner message="Computing yield..." />}

      {error && (
        <div className="bg-red-50 dark:bg-red-900/20 border border-red-300 dark:border-red-800 rounded-lg p-3 text-red-700 dark:text-red-400 text-sm">
          Failed to load yield: {error instanceof Error ? error.message : 'Unknown error'}
        </div>
      )}

      {!isLoading && !error && summary && (
        <div className="grid grid-cols-2 lg:grid-cols-4 gap-3">
          <SummaryCard
            icon={<IconCircleCheck size={16} />}
            title="Productive"
            count={summary.productive}
            cost={summary.productive_cost}
            color="text-green-500"
            currency={currency}
          />
          <SummaryCard
            icon={<IconArrowBackUp size={16} />}
            title="Reverted"
            count={summary.reverted}
            cost={summary.reverted_cost}
            color="text-red-500"
            currency={currency}
          />
          <SummaryCard
            icon={<IconCircleX size={16} />}
            title="Abandoned"
            count={summary.abandoned}
            cost={summary.abandoned_cost}
            color="text-yellow-500"
            currency={currency}
          />
          <SummaryCard
            icon={<IconTrendingUp size={16} />}
            title="No repo"
            count={summary.no_repo}
            cost={summary.no_repo_cost}
            color="text-gray-500"
            currency={currency}
          />
        </div>
      )}

      {!isLoading && !error && data && data.entries.length === 0 && (
        <EmptyState
          icon={<IconGitBranch size={28} />}
          title="No sessions in window"
          description="Try a wider period to see per-session yield."
        />
      )}

      {!isLoading && !error && data && data.entries.length > 0 && (
        <div className="overflow-x-auto rounded-lg border border-gray-200 dark:border-gray-800">
          <table className="w-full text-sm">
            <thead className="bg-gray-50 dark:bg-gray-800/60">
              <tr>
                <th className="px-3 py-2 text-left text-[10px] uppercase tracking-wider text-gray-500 font-medium">Started</th>
                <th className="px-3 py-2 text-left text-[10px] uppercase tracking-wider text-gray-500 font-medium">Project</th>
                <th className="px-3 py-2 text-left text-[10px] uppercase tracking-wider text-gray-500 font-medium">Class</th>
                <th className="px-3 py-2 text-left text-[10px] uppercase tracking-wider text-gray-500 font-medium">Follow-up commit</th>
                <th className="px-3 py-2 text-right text-[10px] uppercase tracking-wider text-gray-500 font-medium">Age</th>
                <th className="px-3 py-2 text-right text-[10px] uppercase tracking-wider text-gray-500 font-medium">Cost</th>
              </tr>
            </thead>
            <tbody>
              {data.entries.map(entry => (
                <YieldRow key={entry.session_id} entry={entry} currency={currency} />
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  )
}
