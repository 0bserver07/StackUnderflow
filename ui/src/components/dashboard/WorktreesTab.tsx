import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  IconActivity,
  IconAlertTriangle,
  IconBinaryTree2,
  IconCheck,
  IconChevronDown,
  IconChevronRight,
  IconCopy,
  IconCurrencyDollar,
  IconGitMerge,
  IconRefresh,
  IconTerminal2,
  IconTrash,
} from '@tabler/icons-react'
import { setProjectByDir } from '../../services/api'
import { attributeWorktrees, getWorktrees } from '../../services/worktrees'
import type { WorktreeInfo, WorktreesResponse } from '../../types/api'
import LoadingSpinner from '../common/LoadingSpinner'
import EmptyState from '../common/EmptyState'
import Badge from '../common/Badge'
import { formatCost } from '../../services/format'
import { useCurrency } from '../../services/currency'

// ---------------------------------------------------------------------------
// WorktreesTab — worktree intelligence (campaign #8).
//
// Surfaces `GET /api/worktrees`: a live, read-only git scan of the project's
// worktrees — which parallel-agent checkouts exist, what they cost, and a
// per-worktree prune verdict (ACTIVE / MERGED_SAFE_TO_PRUNE /
// HAS_UNIQUE_WORK). Prune output is strictly a PREVIEW: the exact commands
// are shown for the user to copy and run — staxtrace never mutates git
// state. "Attribute fragments" fires POST /api/worktrees/attribute, which
// folds phantom worktree "projects" into their parent's analytics
// (store-only write, additive column).
// ---------------------------------------------------------------------------

interface WorktreesTabProps {
  // Folded into the query key so switching projects refetches. Also resolves
  // the project's `log_path` (via the cached setProject query) so the scan
  // request carries explicit project scope.
  projectName: string
}

type VerdictColor = 'blue' | 'green' | 'yellow' | 'gray'

/** Badge text + color for a verdict. Unknown strings (future backend
 *  verdicts) fall back to a gray badge instead of crashing the row. */
function verdictMeta(verdict: string): { label: string; color: VerdictColor } {
  switch (verdict) {
    case 'MERGED_SAFE_TO_PRUNE':
      return { label: 'safe to prune', color: 'green' }
    case 'HAS_UNIQUE_WORK':
      return { label: 'unique work', color: 'yellow' }
    case 'ACTIVE':
      return { label: 'active', color: 'blue' }
    default:
      return { label: verdict.toLowerCase().replace(/_/g, ' '), color: 'gray' }
  }
}

/** Last three path segments — worktree paths are long and share a prefix
 *  with the parent repo, so the tail is the informative part. */
function shortPath(path: string): string {
  const norm = path.replace(/\\/g, '/').replace(/\/+$/, '')
  const parts = norm.split('/').filter(Boolean)
  if (parts.length <= 3) return norm
  return `…/${parts.slice(-3).join('/')}`
}

function formatAge(days: number): string {
  if (!Number.isFinite(days)) return '—'
  if (days < 1) return '<1d'
  return `${Math.round(days)}d`
}

/** `2026-07-02T14:03:09.123` → `2026-07-02 14:03:09` (best-effort). */
function formatScannedAt(ts: string): string {
  return ts.replace('T', ' ').slice(0, 19)
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

function CopyButton({ text, label }: { text: string; label: string }) {
  const [copied, setCopied] = useState(false)
  const onCopy = async () => {
    try {
      await navigator.clipboard.writeText(text)
      setCopied(true)
      setTimeout(() => setCopied(false), 1500)
    } catch {
      // Clipboard unavailable (permissions/insecure context) — stay silent;
      // the commands remain selectable in the <pre> below.
    }
  }
  return (
    <button
      type="button"
      onClick={onCopy}
      className="inline-flex items-center gap-1 px-2 py-1 text-[11px] rounded border border-gray-300 dark:border-gray-700 text-gray-700 dark:text-gray-300 hover:bg-gray-50 dark:hover:bg-gray-800"
    >
      {copied ? <IconCheck size={12} className="text-emerald-500" /> : <IconCopy size={12} />}
      {copied ? 'Copied' : label}
    </button>
  )
}

/** The expanded detail panel under a worktree row: identity fields plus the
 *  prune-command PREVIEW block (copyable; never executed by the tool). */
function WorktreeDetail({ worktree }: { worktree: WorktreeInfo }) {
  const commands = worktree.prune_commands.join('\n')
  return (
    <div className="space-y-3">
      <dl className="grid grid-cols-1 sm:grid-cols-2 gap-x-8 gap-y-1 text-xs">
        <div className="flex gap-2 sm:col-span-2">
          <dt className="text-gray-500 w-24 flex-shrink-0">Full path</dt>
          <dd className="font-mono text-gray-700 dark:text-gray-300 break-all">{worktree.path}</dd>
        </div>
        <div className="flex gap-2">
          <dt className="text-gray-500 w-24 flex-shrink-0">Parent repo</dt>
          <dd className="font-mono text-gray-700 dark:text-gray-300 break-all">{worktree.parent_repo}</dd>
        </div>
        <div className="flex gap-2">
          <dt className="text-gray-500 w-24 flex-shrink-0">Parent slug</dt>
          <dd className="font-mono text-gray-700 dark:text-gray-300 break-all">{worktree.parent_slug}</dd>
        </div>
        <div className="flex gap-2">
          <dt className="text-gray-500 w-24 flex-shrink-0">HEAD</dt>
          <dd className="font-mono text-gray-700 dark:text-gray-300">{worktree.head.slice(0, 10)}</dd>
        </div>
      </dl>

      {worktree.verdict !== 'MERGED_SAFE_TO_PRUNE' && worktree.prune_commands.length > 0 && (
        <div className="flex items-start gap-2 bg-yellow-50 dark:bg-yellow-900/20 border border-yellow-300 dark:border-yellow-800 rounded-md p-2.5 text-yellow-800 dark:text-yellow-300 text-xs">
          <IconAlertTriangle size={14} className="flex-shrink-0 mt-0.5" />
          <span>
            {worktree.verdict === 'ACTIVE'
              ? 'This worktree looks active — pruning it now would interrupt work in progress.'
              : 'This worktree has commits or edits that never landed on the default branch — pruning it now would discard that work.'}
          </span>
        </div>
      )}

      {worktree.prune_commands.length > 0 ? (
        <div className="rounded-md border border-gray-200 dark:border-gray-800 bg-white dark:bg-gray-950">
          <div className="px-3 py-2 flex items-center justify-between gap-2 flex-wrap border-b border-gray-100 dark:border-gray-800">
            <span className="inline-flex items-center gap-1.5 text-[11px] font-medium text-gray-700 dark:text-gray-300">
              <IconTerminal2 size={12} />
              Prune preview
              <Badge color="gray" size="sm">never auto-run</Badge>
            </span>
            <CopyButton text={commands} label="Copy commands" />
          </div>
          <pre className="p-3 text-[11px] font-mono leading-relaxed overflow-x-auto text-gray-800 dark:text-gray-200">
            {commands}
          </pre>
          <div className="px-3 pb-2.5 text-[11px] text-gray-400 dark:text-gray-500">
            Preview only — staxtrace never runs these commands or touches git state. Review
            them, then copy and run them yourself when you&apos;re sure.
          </div>
        </div>
      ) : (
        <div className="text-[11px] text-gray-400 dark:text-gray-500">
          No prune commands for this worktree.
        </div>
      )}
    </div>
  )
}

interface WorktreeRowProps {
  worktree: WorktreeInfo
  currency: ReturnType<typeof useCurrency>['currency']
  expanded: boolean
  onToggle: () => void
}

function WorktreeRow({ worktree, currency, expanded, onToggle }: WorktreeRowProps) {
  const verdict = verdictMeta(worktree.verdict)
  return (
    <>
      <tr
        onClick={onToggle}
        className="border-t border-gray-200 dark:border-gray-800 hover:bg-gray-50 dark:hover:bg-gray-800/40 cursor-pointer"
      >
        <td className="px-3 py-2 text-xs font-mono text-gray-700 dark:text-gray-300 max-w-[18rem]">
          <span title={worktree.path} className="block truncate">
            {shortPath(worktree.path)}
          </span>
        </td>
        <td className="px-3 py-2 text-xs font-mono text-gray-600 dark:text-gray-400 max-w-[12rem]">
          <span title={worktree.branch} className="block truncate">
            {worktree.branch}
          </span>
        </td>
        <td className="px-3 py-2 whitespace-nowrap">
          <Badge color={verdict.color} size="sm">
            {verdict.label}
          </Badge>
        </td>
        <td
          className={`px-3 py-2 text-xs text-right tabular-nums whitespace-nowrap ${
            worktree.dirty_count > 0
              ? 'text-yellow-700 dark:text-yellow-400 font-medium'
              : 'text-gray-600 dark:text-gray-400'
          }`}
        >
          {worktree.dirty_count}
        </td>
        <td
          className={`px-3 py-2 text-xs text-right tabular-nums whitespace-nowrap ${
            worktree.unique_commits > 0
              ? 'text-yellow-700 dark:text-yellow-400 font-medium'
              : 'text-gray-600 dark:text-gray-400'
          }`}
        >
          {worktree.unique_commits}
        </td>
        <td className="px-3 py-2 text-xs text-gray-600 dark:text-gray-400 text-right tabular-nums whitespace-nowrap">
          {formatAge(worktree.age_days)}
        </td>
        <td className="px-3 py-2 text-xs text-gray-600 dark:text-gray-400 text-right tabular-nums whitespace-nowrap">
          {worktree.sessions.toLocaleString()}
        </td>
        <td className="px-3 py-2 text-sm tabular-nums text-gray-900 dark:text-gray-100 font-medium text-right whitespace-nowrap">
          {formatCost(worktree.cost_usd, currency)}
        </td>
        <td className="px-3 py-2 text-right">
          <button
            type="button"
            onClick={e => {
              e.stopPropagation()
              onToggle()
            }}
            aria-expanded={expanded}
            aria-label={expanded ? 'Collapse worktree details' : 'Expand worktree details'}
            className="text-gray-400 hover:text-gray-700 dark:hover:text-gray-200"
          >
            {expanded ? <IconChevronDown size={14} /> : <IconChevronRight size={14} />}
          </button>
        </td>
      </tr>
      {expanded && (
        <tr className="border-t border-gray-100 dark:border-gray-800 bg-gray-50/60 dark:bg-gray-800/20">
          <td colSpan={9} className="px-4 py-3">
            <WorktreeDetail worktree={worktree} />
          </td>
        </tr>
      )}
    </>
  )
}

export default function WorktreesTab({ projectName }: WorktreesTabProps) {
  const { currency } = useCurrency()
  const queryClient = useQueryClient()
  const [expandedPath, setExpandedPath] = useState<string | null>(null)

  // Same key + fn as ProjectDashboard's setProject query, so this is a cache
  // read in practice — it only exists to hand us the project's `log_path`.
  const { data: projectInfo } = useQuery({
    queryKey: ['setProject', projectName],
    queryFn: () => setProjectByDir(projectName),
    staleTime: 60_000,
  })
  const logPath = projectInfo?.log_path

  const { data, isLoading, error, refetch, isFetching } = useQuery({
    queryKey: ['worktrees', projectName, logPath],
    queryFn: () => getWorktrees(logPath),
    enabled: !!logPath,
    staleTime: 60_000,
  })

  const attributeMutation = useMutation({
    mutationFn: () => attributeWorktrees(logPath),
    onSuccess: () => {
      // Attribution changes the roll-up (summary cost here, "includes N
      // worktree sessions" on the parent project), so refetch both surfaces.
      queryClient.invalidateQueries({ queryKey: ['worktrees'] })
      queryClient.invalidateQueries({ queryKey: ['dashboardData', projectName] })
    },
  })

  const report: WorktreesResponse | undefined = data
  const summary = report?.summary
  // Disabled-while-log-path-resolves looks like loading, not empty.
  const pendingScan = isLoading || (!data && !error)

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between gap-3 flex-wrap">
        <div className="flex items-center gap-2">
          <IconBinaryTree2 size={16} className="text-gray-500" />
          <h2 className="text-sm font-semibold text-gray-800 dark:text-gray-200">Worktrees</h2>
          <span className="text-xs text-gray-500">
            agent checkouts — what they cost, what&apos;s safe to prune
          </span>
        </div>
        <div className="flex items-center gap-2 flex-wrap">
          {report?.scanned_at && (
            <span className="text-[11px] text-gray-400 dark:text-gray-500 tabular-nums whitespace-nowrap">
              scanned {formatScannedAt(report.scanned_at)}
            </span>
          )}
          <button
            type="button"
            onClick={() => refetch()}
            disabled={isFetching || !logPath}
            className="inline-flex items-center gap-1.5 px-2.5 py-1.5 text-xs font-medium rounded border border-gray-300 dark:border-gray-700 text-gray-700 dark:text-gray-300 hover:bg-gray-50 dark:hover:bg-gray-800 disabled:opacity-50"
          >
            <IconRefresh size={12} className={isFetching ? 'animate-spin' : ''} />
            Rescan
          </button>
          <button
            type="button"
            onClick={() => attributeMutation.mutate()}
            disabled={attributeMutation.isPending || !logPath}
            title="Fold sessions recorded under worktree checkouts into this project's analytics. Store-only write — git is never touched."
            className="inline-flex items-center gap-1.5 px-2.5 py-1.5 text-xs font-medium rounded border border-indigo-300 dark:border-indigo-800 text-indigo-600 dark:text-indigo-400 hover:bg-indigo-50 dark:hover:bg-indigo-900/20 disabled:opacity-50"
          >
            <IconGitMerge size={12} className={attributeMutation.isPending ? 'animate-spin' : ''} />
            {attributeMutation.isPending ? 'Attributing…' : 'Attribute fragments'}
          </button>
        </div>
      </div>

      {attributeMutation.isSuccess && (
        <div
          className="text-xs text-green-700 dark:text-green-400 bg-green-50 dark:bg-green-900/20 border border-green-300 dark:border-green-800 rounded-md px-3 py-2"
          role="status"
        >
          Attribution complete — {attributeMutation.data.updated.toLocaleString()}{' '}
          {attributeMutation.data.updated === 1 ? 'record' : 'records'} updated. Worktree sessions
          now roll up into this project&apos;s analytics.
        </div>
      )}
      {attributeMutation.isError && (
        <div
          className="text-xs text-red-700 dark:text-red-400 bg-red-50 dark:bg-red-900/20 border border-red-300 dark:border-red-800 rounded-md px-3 py-2"
          role="status"
        >
          Attribution failed:{' '}
          {attributeMutation.error instanceof Error
            ? attributeMutation.error.message
            : 'Unknown error'}
        </div>
      )}

      {pendingScan && <LoadingSpinner message="Scanning worktrees (live against git)..." />}

      {error && (
        <div className="bg-red-50 dark:bg-red-900/20 border border-red-300 dark:border-red-800 rounded-lg p-3 text-red-700 dark:text-red-400 text-sm">
          Failed to scan worktrees: {error instanceof Error ? error.message : 'Unknown error'}
        </div>
      )}

      {!pendingScan && !error && summary && (
        <div className="grid grid-cols-2 lg:grid-cols-5 gap-3">
          <StatCard
            icon={<IconBinaryTree2 size={16} />}
            title="Worktrees"
            value={summary.total.toLocaleString()}
            sub="found by the live scan"
            color="text-indigo-500"
          />
          <StatCard
            icon={<IconTrash size={16} />}
            title="Safe to prune"
            value={summary.safe_to_prune.toLocaleString()}
            sub="fully merged — nothing unique left"
            color="text-green-500"
          />
          <StatCard
            icon={<IconAlertTriangle size={16} />}
            title="Unique work"
            value={summary.has_unique_work.toLocaleString()}
            sub="commits or edits not on the default branch"
            color="text-yellow-500"
          />
          <StatCard
            icon={<IconActivity size={16} />}
            title="Active"
            value={summary.active.toLocaleString()}
            sub="recent activity — leave alone"
            color="text-blue-500"
          />
          <StatCard
            icon={<IconCurrencyDollar size={16} />}
            title="Attributed cost"
            value={formatCost(summary.attributed_cost_usd, currency)}
            sub="session spend traced to worktrees"
            color="text-purple-500"
          />
        </div>
      )}

      {!pendingScan && !error && report && report.worktrees.length === 0 && (
        <EmptyState
          icon={<IconBinaryTree2 size={28} />}
          title="No worktrees detected"
          description="Worktrees are extra checkouts of a repo (git worktree add) that parallel agents often leave behind. The scan runs live against git on every visit, so new ones appear as soon as they exist."
        />
      )}

      {!pendingScan && !error && report && report.worktrees.length > 0 && (
        <div className="space-y-2">
          <div className="flex items-center justify-between gap-3 flex-wrap">
            <h3 className="text-xs uppercase tracking-wider text-gray-500 font-medium">
              Detected worktrees
            </h3>
            <span className="text-[11px] text-gray-400 dark:text-gray-500">
              Click a row for details + prune preview — commands are shown, never run.
            </span>
          </div>
          <div className="overflow-x-auto rounded-lg border border-gray-200 dark:border-gray-800">
            <table className="w-full text-sm">
              <thead className="bg-gray-50 dark:bg-gray-800/60">
                <tr>
                  <th className="px-3 py-2 text-left text-[10px] uppercase tracking-wider text-gray-500 font-medium">Path</th>
                  <th className="px-3 py-2 text-left text-[10px] uppercase tracking-wider text-gray-500 font-medium">Branch</th>
                  <th className="px-3 py-2 text-left text-[10px] uppercase tracking-wider text-gray-500 font-medium">Verdict</th>
                  <th className="px-3 py-2 text-right text-[10px] uppercase tracking-wider text-gray-500 font-medium">Dirty</th>
                  <th className="px-3 py-2 text-right text-[10px] uppercase tracking-wider text-gray-500 font-medium">Unique commits</th>
                  <th className="px-3 py-2 text-right text-[10px] uppercase tracking-wider text-gray-500 font-medium">Age</th>
                  <th className="px-3 py-2 text-right text-[10px] uppercase tracking-wider text-gray-500 font-medium">Sessions</th>
                  <th className="px-3 py-2 text-right text-[10px] uppercase tracking-wider text-gray-500 font-medium">Cost</th>
                  <th className="px-3 py-2" aria-hidden="true" />
                </tr>
              </thead>
              <tbody>
                {report.worktrees.map(worktree => (
                  <WorktreeRow
                    key={worktree.path}
                    worktree={worktree}
                    currency={currency}
                    expanded={expandedPath === worktree.path}
                    onToggle={() =>
                      setExpandedPath(prev => (prev === worktree.path ? null : worktree.path))
                    }
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
