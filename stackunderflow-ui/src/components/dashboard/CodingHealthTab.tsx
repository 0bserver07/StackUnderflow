import { useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import {
  IconActivityHeartbeat,
  IconAlertTriangle,
  IconCircleCheck,
  IconFileText,
  IconHistory,
  IconRefresh,
  IconTerminal,
} from '@tabler/icons-react'
import { getPatterns, type PatternsSince } from '../../services/patterns'
import type {
  PatternCommandCluster,
  PatternErrorSignature,
  PatternFileRisk,
  PatternsReportData,
} from '../../types/api'
import LoadingSpinner from '../common/LoadingSpinner'
import EmptyState from '../common/EmptyState'
import Badge from '../common/Badge'

// ---------------------------------------------------------------------------
// CodingHealthTab — cross-session pattern / failure mining (campaign #6).
//
// Surfaces `GET /api/patterns`: recurrence-keyed intelligence across ALL
// sessions in a bounded window — files that keep failing when touched,
// error signatures that resurface session after session (with what the
// sessions that moved past them did next), and Bash failures clustered by
// command. Everything here is advisory: patterns to review, not verdicts.
// ---------------------------------------------------------------------------

const WINDOWS: { id: PatternsSince; label: string }[] = [
  { id: '7d', label: '7d' },
  { id: '30d', label: '30d' },
  { id: '90d', label: '90d' },
]

interface CodingHealthTabProps {
  // Folded into the query key so switching projects refetches (the server
  // scopes to the active project via `deps.current_log_path`).
  projectName: string
}

function pct(rate: number | null): string {
  if (rate === null || !Number.isFinite(rate)) return '—'
  return `${(rate * 100).toFixed(0)}%`
}

function day(ts: string | null): string {
  return ts ? ts.slice(0, 10) : '—'
}

function basename(path: string): string {
  const parts = path.replace(/\\/g, '/').split('/')
  return parts[parts.length - 1] || path
}

/** Dominant error category of a `{category: count}` map, for a compact badge. */
function topCategory(categories: Record<string, number>): string | null {
  let best: string | null = null
  let bestN = 0
  for (const [cat, n] of Object.entries(categories)) {
    if (n > bestN || (n === bestN && best !== null && cat < best)) {
      best = cat
      bestN = n
    }
  }
  return best
}

function riskColor(rate: number | null): 'red' | 'yellow' | 'gray' {
  if (rate === null) return 'gray'
  if (rate >= 0.5) return 'red'
  if (rate >= 0.2) return 'yellow'
  return 'gray'
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

function FileRiskRow({ file }: { file: PatternFileRisk }) {
  const cat = topCategory(file.categories)
  return (
    <tr className="border-t border-gray-200 dark:border-gray-800 hover:bg-gray-50 dark:hover:bg-gray-800/40">
      <td className="px-3 py-2 text-xs font-mono text-gray-700 dark:text-gray-300 max-w-[16rem]">
        <span title={`${file.path}\n${file.reason}`} className="block truncate">
          {basename(file.path)}
        </span>
      </td>
      <td className="px-3 py-2">
        <Badge color={riskColor(file.failure_rate)} size="sm">
          {pct(file.failure_rate)}
        </Badge>
      </td>
      <td className="px-3 py-2 text-xs text-gray-600 dark:text-gray-400 text-right tabular-nums whitespace-nowrap">
        {file.failure_session_count} / {file.touch_session_count}
      </td>
      <td className="px-3 py-2 text-xs text-gray-600 dark:text-gray-400 text-right tabular-nums whitespace-nowrap">
        {file.touch_count}
      </td>
      <td className="px-3 py-2 text-xs text-gray-600 dark:text-gray-400 whitespace-nowrap">
        {cat ? <Badge color="gray" size="sm">{cat}</Badge> : '—'}
      </td>
      <td className="px-3 py-2 text-xs text-gray-600 dark:text-gray-400 text-right tabular-nums whitespace-nowrap">
        {day(file.last_failure_ts)}
      </td>
    </tr>
  )
}

function SignatureCard({ sig }: { sig: PatternErrorSignature }) {
  return (
    <div className="bg-white dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-800 p-3 space-y-2">
      <div className="flex items-start justify-between gap-2 flex-wrap">
        <div className="flex items-center gap-2 min-w-0">
          <Badge color="red" size="sm">{sig.category}</Badge>
          <span
            className="text-xs font-mono text-gray-800 dark:text-gray-200 truncate"
            title={sig.example || sig.signature}
          >
            {sig.signature}
          </span>
        </div>
        <div className="flex items-center gap-2 text-[11px] text-gray-500 whitespace-nowrap tabular-nums">
          <span>{sig.session_count} sessions</span>
          <span>·</span>
          <span>{sig.count}×</span>
          <span>·</span>
          <span>last {day(sig.last_ts)}</span>
        </div>
      </div>
      <div className="flex items-center gap-2 flex-wrap text-[11px] text-gray-500">
        {sig.resolved_session_count > 0 ? (
          <span className="inline-flex items-center gap-1 text-green-700 dark:text-green-400">
            <IconCircleCheck size={12} />
            {sig.resolved_session_count} moved past it
          </span>
        ) : (
          <span className="inline-flex items-center gap-1 text-gray-500">
            <IconRefresh size={12} />
            no session in window moved past it
          </span>
        )}
        {sig.resolution_hints.map(hint => (
          <Badge key={hint.action} color="green" size="sm">
            next: {hint.action} ×{hint.count}
          </Badge>
        ))}
        {sig.top_files.slice(0, 2).map(f => (
          <span key={f} className="font-mono" title={f}>
            {basename(f)}
          </span>
        ))}
      </div>
    </div>
  )
}

function CommandClusterRow({ cluster }: { cluster: PatternCommandCluster }) {
  const cat = topCategory(cluster.categories)
  return (
    <tr className="border-t border-gray-200 dark:border-gray-800 hover:bg-gray-50 dark:hover:bg-gray-800/40">
      <td className="px-3 py-2 text-xs font-mono text-gray-700 dark:text-gray-300 whitespace-nowrap">
        <span title={cluster.example}>{cluster.command}</span>
      </td>
      <td className="px-3 py-2 text-xs text-gray-600 dark:text-gray-400 text-right tabular-nums whitespace-nowrap">
        {cluster.failure_count}
      </td>
      <td className="px-3 py-2 text-xs text-gray-600 dark:text-gray-400 text-right tabular-nums whitespace-nowrap">
        {cluster.session_count}
      </td>
      <td className="px-3 py-2 whitespace-nowrap">
        {cat ? <Badge color="orange" size="sm">{cat}</Badge> : '—'}
      </td>
      <td className="px-3 py-2 text-xs text-gray-600 dark:text-gray-400 text-right tabular-nums whitespace-nowrap">
        {day(cluster.last_failure_ts)}
      </td>
    </tr>
  )
}

export default function CodingHealthTab({ projectName }: CodingHealthTabProps) {
  const [since, setSince] = useState<PatternsSince>('90d')

  const { data, isLoading, error } = useQuery({
    queryKey: ['patterns', projectName, since],
    queryFn: () => getPatterns(since),
    staleTime: 60_000,
  })

  const report: PatternsReportData | undefined = data?.report
  const totals = report?.totals
  const hasAnyActivity = (totals?.session_count ?? 0) > 0
  const hasFindings =
    !!report &&
    (report.file_risk.length > 0 ||
      report.error_signatures.length > 0 ||
      report.command_clusters.length > 0)

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between gap-3 flex-wrap">
        <div className="flex items-center gap-2">
          <IconActivityHeartbeat size={16} className="text-gray-500" />
          <h2 className="text-sm font-semibold text-gray-800 dark:text-gray-200">Coding Health</h2>
          <span className="text-xs text-gray-500">
            recurring failures across sessions — files, errors, commands
          </span>
        </div>
        <div
          className="inline-flex rounded-md border border-gray-200 dark:border-gray-700 overflow-hidden"
          role="group"
          aria-label="Coding health window"
        >
          {WINDOWS.map(w => (
            <button
              key={w.id}
              type="button"
              onClick={() => setSince(w.id)}
              className={`px-3 py-1.5 text-xs font-medium transition-colors ${
                w.id === since
                  ? 'bg-indigo-500/10 text-indigo-600 dark:text-indigo-400'
                  : 'bg-white dark:bg-gray-900 text-gray-600 dark:text-gray-400 hover:text-gray-900 dark:hover:text-gray-200'
              }`}
            >
              {w.label}
            </button>
          ))}
        </div>
      </div>

      {isLoading && <LoadingSpinner message="Mining cross-session patterns..." />}

      {error && (
        <div className="bg-red-50 dark:bg-red-900/20 border border-red-300 dark:border-red-800 rounded-lg p-3 text-red-700 dark:text-red-400 text-sm">
          Failed to load coding health: {error instanceof Error ? error.message : 'Unknown error'}
        </div>
      )}

      {!isLoading && !error && report && !report.sources.message_tool_mart && (
        <div className="flex items-start gap-2 bg-yellow-50 dark:bg-yellow-900/20 border border-yellow-300 dark:border-yellow-800 rounded-md p-3 text-yellow-800 dark:text-yellow-300 text-xs">
          <IconAlertTriangle size={14} className="flex-shrink-0 mt-0.5" />
          <span>
            File-touch history is unavailable (the per-tool mart is empty), so failure rates
            cannot be computed. Run <code className="font-mono">stackunderflow etl backfill</code>{' '}
            to materialize it.
          </span>
        </div>
      )}

      {!isLoading && !error && totals && (
        <div className="grid grid-cols-2 lg:grid-cols-4 gap-3">
          <StatCard
            icon={<IconAlertTriangle size={16} />}
            title="Sessions with failures"
            value={totals.sessions_with_failures.toLocaleString()}
            sub={`of ${totals.session_count.toLocaleString()} sessions in window`}
            color="text-red-500"
          />
          <StatCard
            icon={<IconFileText size={16} />}
            title="Files at risk"
            value={(report?.file_risk.length ?? 0).toLocaleString()}
            sub={`${totals.files_touched.toLocaleString()} files touched`}
            color="text-yellow-500"
          />
          <StatCard
            icon={<IconRefresh size={16} />}
            title="Tool errors"
            value={totals.error_count.toLocaleString()}
            sub={`${totals.attributed_error_count.toLocaleString()} attributed to a call`}
            color="text-orange-500"
          />
          <StatCard
            icon={<IconHistory size={16} />}
            title="Interruptions"
            value={totals.interruption_count.toLocaleString()}
            sub={`across ${totals.interruption_session_count.toLocaleString()} sessions`}
            color="text-purple-500"
          />
        </div>
      )}

      {!isLoading && !error && report && !hasFindings && (
        <EmptyState
          icon={<IconActivityHeartbeat size={28} />}
          title={hasAnyActivity ? 'No recurring failure patterns in window' : 'No activity in window'}
          description={
            hasAnyActivity
              ? 'Nothing failed repeatedly across sessions in this period. Healthy — or try a wider window.'
              : 'No tool calls or errors were recorded in this period. Ingest sessions or widen the window.'
          }
        />
      )}

      {!isLoading && !error && report && report.file_risk.length > 0 && (
        <div className="space-y-2">
          <h3 className="text-xs uppercase tracking-wider text-gray-500 font-medium">
            File risk — fails when touched
          </h3>
          <div className="overflow-x-auto rounded-lg border border-gray-200 dark:border-gray-800">
            <table className="w-full text-sm">
              <thead className="bg-gray-50 dark:bg-gray-800/60">
                <tr>
                  <th className="px-3 py-2 text-left text-[10px] uppercase tracking-wider text-gray-500 font-medium">File</th>
                  <th className="px-3 py-2 text-left text-[10px] uppercase tracking-wider text-gray-500 font-medium">Failure rate</th>
                  <th className="px-3 py-2 text-right text-[10px] uppercase tracking-wider text-gray-500 font-medium">Failing / touching sessions</th>
                  <th className="px-3 py-2 text-right text-[10px] uppercase tracking-wider text-gray-500 font-medium">Touches</th>
                  <th className="px-3 py-2 text-left text-[10px] uppercase tracking-wider text-gray-500 font-medium">Top error</th>
                  <th className="px-3 py-2 text-right text-[10px] uppercase tracking-wider text-gray-500 font-medium">Last failure</th>
                </tr>
              </thead>
              <tbody>
                {report.file_risk.map(file => (
                  <FileRiskRow key={file.path} file={file} />
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}

      {!isLoading && !error && report && report.error_signatures.length > 0 && (
        <div className="space-y-2">
          <h3 className="text-xs uppercase tracking-wider text-gray-500 font-medium">
            Recurring errors — same signature, multiple sessions
          </h3>
          <div className="space-y-2">
            {report.error_signatures.map(sig => (
              <SignatureCard key={`${sig.category}:${sig.signature}`} sig={sig} />
            ))}
          </div>
        </div>
      )}

      {!isLoading && !error && report && report.command_clusters.length > 0 && (
        <div className="space-y-2">
          <h3 className="text-xs uppercase tracking-wider text-gray-500 font-medium">
            <span className="inline-flex items-center gap-1">
              <IconTerminal size={12} />
              Command failure clusters
            </span>
          </h3>
          <div className="overflow-x-auto rounded-lg border border-gray-200 dark:border-gray-800">
            <table className="w-full text-sm">
              <thead className="bg-gray-50 dark:bg-gray-800/60">
                <tr>
                  <th className="px-3 py-2 text-left text-[10px] uppercase tracking-wider text-gray-500 font-medium">Command</th>
                  <th className="px-3 py-2 text-right text-[10px] uppercase tracking-wider text-gray-500 font-medium">Failures</th>
                  <th className="px-3 py-2 text-right text-[10px] uppercase tracking-wider text-gray-500 font-medium">Sessions</th>
                  <th className="px-3 py-2 text-left text-[10px] uppercase tracking-wider text-gray-500 font-medium">Top error</th>
                  <th className="px-3 py-2 text-right text-[10px] uppercase tracking-wider text-gray-500 font-medium">Last failure</th>
                </tr>
              </thead>
              <tbody>
                {report.command_clusters.map(cluster => (
                  <CommandClusterRow key={cluster.command} cluster={cluster} />
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}
    </div>
  )
}
