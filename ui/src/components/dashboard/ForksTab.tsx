import { useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import {
  IconAlertTriangle,
  IconGitBranch,
  IconGitFork,
  IconRobot,
  IconTrash,
} from '@tabler/icons-react'
import { getForks, type ForksPeriod } from '../../services/api'
import type { AbandonedBranch, ForkReportData } from '../../types/api'
import LoadingSpinner from '../common/LoadingSpinner'
import EmptyState from '../common/EmptyState'
import Badge from '../common/Badge'
import { formatCost, formatTokens } from '../../services/format'
import { useCurrency } from '../../services/currency'

// ---------------------------------------------------------------------------
// ForksTab — fork / sidechain economics.
//
// Surfaces `GET /api/forks`: the cost + token share that went to Claude
// subagent (sidechain) messages, and the fork points where the conversation
// branched and one path was abandoned. The response carries a `warning` with
// the DAG-inference caveat, rendered as a banner near the top so the
// heuristic is impossible to miss.
// ---------------------------------------------------------------------------

const PERIODS: { id: ForksPeriod; label: string }[] = [
  { id: 'today', label: 'Today' },
  { id: 'week', label: '7d' },
  { id: 'month', label: '30d' },
  { id: 'all', label: 'All' },
]

interface ForksTabProps {
  // Folded into the query key so switching projects refetches (the server
  // scopes to the active project via `deps.current_log_path`).
  projectName: string
}

function pct(x: number): string {
  if (!Number.isFinite(x)) return '0%'
  return `${(x * 100).toFixed(1)}%`
}

function formatGap(seconds: number | null): string {
  if (seconds === null || !Number.isFinite(seconds)) return '—'
  if (seconds >= 86_400) return `${(seconds / 86_400).toFixed(1)}d`
  if (seconds >= 3_600) return `${(seconds / 3_600).toFixed(1)}h`
  if (seconds >= 60) return `${Math.round(seconds / 60)}m`
  return `${Math.round(seconds)}s`
}

interface StatCardProps {
  icon: React.ReactNode
  title: string
  value: string
  sub?: string
  color: string
}

function StatCard({ icon, title, value, sub, color }: StatCardProps) {
  return (
    <div className="bg-white dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-800 p-4">
      <div className="flex items-center gap-2 mb-2">
        <span className={color}>{icon}</span>
        <span className="text-xs uppercase tracking-wider text-gray-500 font-medium">{title}</span>
      </div>
      <div className="text-2xl font-bold text-gray-900 dark:text-gray-100 tabular-nums">{value}</div>
      {sub && <div className="text-xs text-gray-500 mt-1 tabular-nums">{sub}</div>}
    </div>
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

interface BranchRowProps {
  branch: AbandonedBranch
  currency: ReturnType<typeof useCurrency>['currency']
}

function BranchRow({ branch, currency }: BranchRowProps) {
  return (
    <tr className="border-t border-gray-200 dark:border-gray-800 hover:bg-gray-50 dark:hover:bg-gray-800/40">
      <td className="px-3 py-2 text-xs font-mono text-gray-700 dark:text-gray-300 whitespace-nowrap">
        {branch.session_id.slice(0, 8)}
      </td>
      <td className="px-3 py-2 text-xs font-mono text-gray-600 dark:text-gray-400 whitespace-nowrap">
        {branch.branch_head_uuid.slice(0, 8)}
      </td>
      <td className="px-3 py-2">
        {branch.sidechain ? (
          <Badge color="purple" size="sm">
            sidechain
          </Badge>
        ) : (
          <Badge color="gray" size="sm">
            branch
          </Badge>
        )}
      </td>
      <td className="px-3 py-2 text-xs text-gray-600 dark:text-gray-400 text-right tabular-nums whitespace-nowrap">
        {branch.message_count}
      </td>
      <td className="px-3 py-2 text-xs text-gray-600 dark:text-gray-400 text-right tabular-nums whitespace-nowrap">
        {formatTokens(branch.token_total)}
      </td>
      <td className="px-3 py-2 text-xs text-gray-600 dark:text-gray-400 text-right tabular-nums whitespace-nowrap">
        {formatGap(branch.gap_seconds)}
      </td>
      <td className="px-3 py-2 text-sm tabular-nums text-gray-900 dark:text-gray-100 font-medium text-right whitespace-nowrap">
        {formatCost(branch.cost_usd, currency)}
      </td>
    </tr>
  )
}

export default function ForksTab({ projectName }: ForksTabProps) {
  const { currency } = useCurrency()
  const [period, setPeriod] = useState<ForksPeriod>('all')

  const { data, isLoading, error } = useQuery({
    queryKey: ['forks', projectName, period],
    queryFn: () => getForks(period),
    staleTime: 60_000,
  })

  const report: ForkReportData | undefined = data?.report

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between gap-3 flex-wrap">
        <div className="flex items-center gap-2">
          <IconGitFork size={16} className="text-gray-500" />
          <h2 className="text-sm font-semibold text-gray-800 dark:text-gray-200">
            Forks &amp; Sidechains
          </h2>
          <span className="text-xs text-gray-500">
            subagent spend + branches you started then dropped
          </span>
        </div>
        <div
          className="inline-flex rounded-md border border-gray-200 dark:border-gray-700 overflow-hidden"
          role="group"
          aria-label="Forks period"
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

      {isLoading && <LoadingSpinner message="Analyzing forks..." />}

      {error && (
        <div className="bg-red-50 dark:bg-red-900/20 border border-red-300 dark:border-red-800 rounded-lg p-3 text-red-700 dark:text-red-400 text-sm">
          Failed to load forks: {error instanceof Error ? error.message : 'Unknown error'}
        </div>
      )}

      {!isLoading && !error && report && (
        <div className="grid grid-cols-2 lg:grid-cols-4 gap-3">
          <StatCard
            icon={<IconRobot size={16} />}
            title="Sidechain cost"
            value={formatCost(report.sidechain_cost_usd, currency)}
            sub={`${pct(report.sidechain_cost_share)} of ${formatCost(report.total_cost_usd, currency)} · ${report.sidechain_message_count.toLocaleString()} msgs`}
            color="text-purple-500"
          />
          <StatCard
            icon={<IconRobot size={16} />}
            title="Sidechain tokens"
            value={formatTokens(report.sidechain_token_total)}
            sub={`${pct(report.sidechain_token_share)} of all tokens`}
            color="text-purple-500"
          />
          <StatCard
            icon={<IconGitBranch size={16} />}
            title="Fork points"
            value={report.fork_point_count.toLocaleString()}
            sub="conversation branched here"
            color="text-blue-500"
          />
          <StatCard
            icon={<IconTrash size={16} />}
            title="Abandoned spend"
            value={formatCost(report.abandoned_cost_usd, currency)}
            sub={`${report.abandoned_branch_count.toLocaleString()} dropped ${report.abandoned_branch_count === 1 ? 'branch' : 'branches'}`}
            color="text-yellow-500"
          />
        </div>
      )}

      {!isLoading && !error && report && report.abandoned_branches.length === 0 && (
        <EmptyState
          icon={<IconGitFork size={28} />}
          title="No abandoned branches in window"
          description="Nothing was branched and dropped in this period — or the sunk cost was below the noise floor. Try a wider period."
        />
      )}

      {!isLoading && !error && report && report.abandoned_branches.length > 0 && (
        <div className="space-y-2">
          <h3 className="text-xs uppercase tracking-wider text-gray-500 font-medium">
            Top abandoned branches
          </h3>
          <div className="overflow-x-auto rounded-lg border border-gray-200 dark:border-gray-800">
            <table className="w-full text-sm">
              <thead className="bg-gray-50 dark:bg-gray-800/60">
                <tr>
                  <th className="px-3 py-2 text-left text-[10px] uppercase tracking-wider text-gray-500 font-medium">Session</th>
                  <th className="px-3 py-2 text-left text-[10px] uppercase tracking-wider text-gray-500 font-medium">Branch</th>
                  <th className="px-3 py-2 text-left text-[10px] uppercase tracking-wider text-gray-500 font-medium">Kind</th>
                  <th className="px-3 py-2 text-right text-[10px] uppercase tracking-wider text-gray-500 font-medium">Msgs</th>
                  <th className="px-3 py-2 text-right text-[10px] uppercase tracking-wider text-gray-500 font-medium">Tokens</th>
                  <th className="px-3 py-2 text-right text-[10px] uppercase tracking-wider text-gray-500 font-medium">Dropped before end</th>
                  <th className="px-3 py-2 text-right text-[10px] uppercase tracking-wider text-gray-500 font-medium">Cost</th>
                </tr>
              </thead>
              <tbody>
                {report.abandoned_branches.map(branch => (
                  <BranchRow
                    key={`${branch.session_id}:${branch.branch_head_uuid}`}
                    branch={branch}
                    currency={currency}
                  />
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}
    </div>
  )
}
