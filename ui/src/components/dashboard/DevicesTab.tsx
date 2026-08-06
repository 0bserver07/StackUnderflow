import { useEffect, useMemo, useState, type ReactNode } from 'react'
import { useQuery } from '@tanstack/react-query'
import {
  BarChart,
  Bar,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
} from 'recharts'
import {
  IconArrowsLeftRight,
  IconAlertTriangle,
  IconCurrencyDollar,
  IconDatabase,
  IconStack2,
  IconUsers,
  IconRefresh,
} from '@tabler/icons-react'

import { getSyncOverview, getSyncStatus, type SyncScope } from '../../services/sync'
import type { SyncByProject, SyncOverviewMerged, SyncPeer, SyncStatus } from '../../types/api'
import { formatCost, formatNumber, formatTokens } from '../../services/format'
import { useCurrency } from '../../services/currency'
import LoadingSpinner from '../common/LoadingSpinner'
import EmptyState from '../common/EmptyState'
import Badge from '../common/Badge'
import { ChartCard, EmptyChartCard, useChartTheme, CHART_HEIGHT } from '../charts/chartTheme'

// ---------------------------------------------------------------------------
// DevicesTab — the cross-device analytics overlay (#100 Phase 2).
//
// A dedicated beta tab (following the Worktrees / Context-Replay precedent) so
// the existing per-device Overview/Cost render paths stay byte-identical when
// sync is off. Gated entirely behind `/api/sync/status`: with sync unconfigured
// the tab shows a clean "not set up" empty state and no toggle. When configured,
// a This device ↔ All devices toggle flips between the local view and the
// merged `local UNION ALL <mart>_remote` roll-up (`/api/sync/overview`).
//
// Cost figures arrive pre-converted to the active currency (the route converts),
// so `formatCost` only needs the active symbol — same contract as the Cost tab.
// ---------------------------------------------------------------------------

interface DevicesTabProps {
  // Accepted for parity with the other project-scoped tabs and folded into the
  // query keys so a project switch refetches. The sync surface itself is
  // store-global (it merges every device's aggregates), so the value is not
  // sent to the endpoints — it only namespaces the cache.
  projectName: string
}

/** First 10 chars of a device UUID — full ids are 32 hex chars and overflow. */
function shortId(id: string, n = 10): string {
  return id.length > n ? `${id.slice(0, n)}…` : id
}

/** `2026-07-02T14:03:09.123Z` → `2026-07-02 14:03` (best-effort, null-safe). */
function formatTs(ts: string | null | undefined): string {
  if (!ts) return '—'
  return ts.replace('T', ' ').slice(0, 16)
}

/** Human project label — the merged display_name, falling back to the slug. */
function projectLabel(p: SyncByProject): string {
  const name = (p.display_name ?? '').trim()
  return name || p.slug
}

// Hoisted typed tuple — Recharts' `radius` wants [number,number,number,number],
// and an inline array literal infers as `number[]` (not the tuple). Same reason
// DailyCostChart hoists its radius constants.
const TOP_RADIUS: [number, number, number, number] = [4, 4, 0, 0]

interface StatCardProps {
  icon: ReactNode
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

/** This device ↔ All devices segmented toggle. Only rendered when sync is
 *  configured (the caller gates it). */
function ScopeToggle({
  scope,
  onChange,
}: {
  scope: SyncScope
  onChange: (next: SyncScope) => void
}) {
  const base =
    'px-3 py-1.5 text-xs font-medium transition-colors focus:outline-none focus:z-10'
  const active = 'bg-indigo-500 text-white'
  const idle =
    'bg-white dark:bg-gray-900 text-gray-600 dark:text-gray-400 hover:bg-gray-50 dark:hover:bg-gray-800'
  return (
    <div
      className="inline-flex rounded-md border border-gray-300 dark:border-gray-700 overflow-hidden"
      role="group"
      aria-label="Analytics scope"
    >
      <button
        type="button"
        onClick={() => onChange('this-device')}
        aria-pressed={scope === 'this-device'}
        className={`${base} ${scope === 'this-device' ? active : idle} border-r border-gray-300 dark:border-gray-700`}
      >
        This device
      </button>
      <button
        type="button"
        onClick={() => onChange('all-devices')}
        aria-pressed={scope === 'all-devices'}
        className={`${base} ${scope === 'all-devices' ? active : idle}`}
      >
        All devices
      </button>
    </div>
  )
}

/** The merged all-devices body: headline totals, a per-day cost trend, the
 *  per-project breakdown, and the contributing-device list. */
function MergedView({ merged, status }: { merged: SyncOverviewMerged; status: SyncStatus }) {
  const { currency } = useCurrency()
  const palette = useChartTheme()

  const costTickFormatter = useMemo(() => (v: number) => formatCost(v, currency), [currency])
  // Two-arg shape mirrors DailyCostChart's proven formatter (Recharts calls it
  // with (value, name)); we don't use the series name here.
  const costTooltipFormatter = useMemo(
    () =>
      (value: number, _name: string) =>
        [formatCost(value, currency), 'Cost'] as [string, string],
    [currency],
  )

  const totalTokens = merged.totals.input_tokens + merged.totals.output_tokens
  const cachedTokens = merged.totals.cache_read + merged.totals.cache_create

  // Per-project rows, richest spenders first. Copy before sorting (never
  // mutate the query cache). Cap the table so a large multi-device store
  // doesn't render hundreds of rows.
  const topProjects = useMemo(
    () => [...merged.by_project].sort((a, b) => b.total_cost_usd - a.total_cost_usd).slice(0, 25),
    [merged.by_project],
  )

  // Enrich the per-device breakdown with each peer's last-seen from status.
  const peerByUuid = useMemo(() => {
    const m = new Map<string, SyncPeer>()
    for (const p of status.peers) m.set(p.remote_device_uuid, p)
    return m
  }, [status.peers])

  return (
    <div className="space-y-4">
      {/* headline totals */}
      <div className="grid grid-cols-2 lg:grid-cols-4 gap-3">
        <StatCard
          icon={<IconCurrencyDollar size={16} />}
          title="Merged spend"
          value={formatCost(merged.totals.cost_usd, currency)}
          sub="across every device"
          color="text-purple-500"
        />
        <StatCard
          icon={<IconDatabase size={16} />}
          title="Tokens"
          value={formatTokens(totalTokens)}
          sub={`${formatTokens(cachedTokens)} cached`}
          color="text-indigo-500"
        />
        <StatCard
          icon={<IconStack2 size={16} />}
          title="Sessions"
          value={formatNumber(merged.totals.session_count)}
          sub={`${formatNumber(merged.totals.message_count)} messages`}
          color="text-emerald-500"
        />
        <StatCard
          icon={<IconUsers size={16} />}
          title="Devices"
          value={formatNumber(merged.devices.length)}
          sub={`${status.peer_count} known peer${status.peer_count === 1 ? '' : 's'}`}
          color="text-blue-500"
        />
      </div>

      {/* per-day cost trend — house chart chrome */}
      {merged.by_day.length > 0 ? (
        <ChartCard title="Merged daily cost">
          <ResponsiveContainer width="100%" height={CHART_HEIGHT}>
            <BarChart data={merged.by_day}>
              <CartesianGrid strokeDasharray="3 3" stroke={palette.grid} />
              <XAxis
                dataKey="day"
                tick={palette.tick}
                tickLine={palette.axisLine}
                axisLine={palette.axisLine}
              />
              <YAxis
                tick={palette.tick}
                tickLine={palette.axisLine}
                axisLine={palette.axisLine}
                tickFormatter={costTickFormatter}
              />
              <Tooltip
                contentStyle={palette.tooltipContent}
                labelStyle={palette.tooltipLabel}
                formatter={costTooltipFormatter}
              />
              <Bar dataKey="cost_usd" fill="#818CF8" radius={TOP_RADIUS} isAnimationActive={false} />
            </BarChart>
          </ResponsiveContainer>
        </ChartCard>
      ) : (
        <EmptyChartCard title="Merged daily cost" message="No daily data yet" />
      )}

      {/* per-project breakdown */}
      <div className="space-y-2">
        <h3 className="text-xs uppercase tracking-wider text-gray-500 font-medium">
          By project (all devices)
        </h3>
        {topProjects.length === 0 ? (
          <div className="text-xs text-gray-400 dark:text-gray-500 px-1">No projects merged yet.</div>
        ) : (
          <div className="overflow-x-auto rounded-lg border border-gray-200 dark:border-gray-800">
            <table className="w-full text-sm">
              <thead className="bg-gray-50 dark:bg-gray-800/60">
                <tr>
                  <th className="px-3 py-2 text-left text-[10px] uppercase tracking-wider text-gray-500 font-medium">Project</th>
                  <th className="px-3 py-2 text-left text-[10px] uppercase tracking-wider text-gray-500 font-medium">Provider</th>
                  <th className="px-3 py-2 text-right text-[10px] uppercase tracking-wider text-gray-500 font-medium">Sessions</th>
                  <th className="px-3 py-2 text-right text-[10px] uppercase tracking-wider text-gray-500 font-medium">Messages</th>
                  <th className="px-3 py-2 text-right text-[10px] uppercase tracking-wider text-gray-500 font-medium">Cost</th>
                </tr>
              </thead>
              <tbody>
                {topProjects.map((p) => (
                  <tr
                    key={`${p.provider}/${p.slug}`}
                    className="border-t border-gray-200 dark:border-gray-800 hover:bg-gray-50 dark:hover:bg-gray-800/40"
                  >
                    <td className="px-3 py-2 text-xs text-gray-800 dark:text-gray-200 max-w-[20rem]">
                      <span title={p.slug} className="block truncate">{projectLabel(p)}</span>
                    </td>
                    <td className="px-3 py-2 text-xs text-gray-500 dark:text-gray-400 whitespace-nowrap">{p.provider}</td>
                    <td className="px-3 py-2 text-xs text-gray-600 dark:text-gray-400 text-right tabular-nums whitespace-nowrap">
                      {formatNumber(p.total_sessions)}
                    </td>
                    <td className="px-3 py-2 text-xs text-gray-600 dark:text-gray-400 text-right tabular-nums whitespace-nowrap">
                      {formatNumber(p.total_messages)}
                    </td>
                    <td className="px-3 py-2 text-sm text-gray-900 dark:text-gray-100 font-medium text-right tabular-nums whitespace-nowrap">
                      {formatCost(p.total_cost_usd, currency)}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>

      {/* contributing devices / peers */}
      <div className="space-y-2">
        <h3 className="text-xs uppercase tracking-wider text-gray-500 font-medium">
          Contributing devices
        </h3>
        <div className="overflow-x-auto rounded-lg border border-gray-200 dark:border-gray-800">
          <table className="w-full text-sm">
            <thead className="bg-gray-50 dark:bg-gray-800/60">
              <tr>
                <th className="px-3 py-2 text-left text-[10px] uppercase tracking-wider text-gray-500 font-medium">Device</th>
                <th className="px-3 py-2 text-left text-[10px] uppercase tracking-wider text-gray-500 font-medium">Last seen</th>
                <th className="px-3 py-2 text-right text-[10px] uppercase tracking-wider text-gray-500 font-medium">Projects</th>
                <th className="px-3 py-2 text-right text-[10px] uppercase tracking-wider text-gray-500 font-medium">Cost</th>
              </tr>
            </thead>
            <tbody>
              {merged.devices.map((d) => {
                const peer = d.is_local ? undefined : peerByUuid.get(d.device_uuid)
                const name = d.is_local
                  ? 'This device'
                  : (d.alias ?? peer?.alias ?? shortId(d.device_uuid))
                return (
                  <tr
                    key={d.device_uuid}
                    className="border-t border-gray-200 dark:border-gray-800 hover:bg-gray-50 dark:hover:bg-gray-800/40"
                  >
                    <td className="px-3 py-2 text-xs text-gray-800 dark:text-gray-200 whitespace-nowrap">
                      <span className="inline-flex items-center gap-2">
                        {name}
                        {d.is_local && <Badge color="blue" size="sm">local</Badge>}
                      </span>
                    </td>
                    <td className="px-3 py-2 text-xs text-gray-500 dark:text-gray-400 tabular-nums whitespace-nowrap">
                      {d.is_local ? '—' : formatTs(peer?.last_seen)}
                    </td>
                    <td className="px-3 py-2 text-xs text-gray-600 dark:text-gray-400 text-right tabular-nums whitespace-nowrap">
                      {formatNumber(d.projects)}
                    </td>
                    <td className="px-3 py-2 text-sm text-gray-900 dark:text-gray-100 font-medium text-right tabular-nums whitespace-nowrap">
                      {formatCost(d.cost_usd, currency)}
                    </td>
                  </tr>
                )
              })}
            </tbody>
          </table>
        </div>
        {status.peer_count === 0 && (
          <div className="text-[11px] text-gray-400 dark:text-gray-500 px-1">
            No peers pulled yet — run <code className="font-mono">stackunderflow sync pull</code> on
            this device after another machine has pushed.
          </div>
        )}
      </div>
    </div>
  )
}

export default function DevicesTab({ projectName }: DevicesTabProps) {
  // Local sync config + peers + availability. Pure local read; safe with sync
  // off (returns enabled: false).
  const statusQuery = useQuery({
    queryKey: ['syncStatus', projectName],
    queryFn: getSyncStatus,
    staleTime: 30_000,
  })
  const status = statusQuery.data

  // Scope toggle. Auto-flip to all-devices once, when cross-device data exists,
  // unless the user has already picked a side — the merged view is the point of
  // this tab, so we show it by default when there's something to merge.
  const [scope, setScope] = useState<SyncScope>('this-device')
  const [userPicked, setUserPicked] = useState(false)
  useEffect(() => {
    if (!userPicked && status?.all_devices_available) setScope('all-devices')
  }, [userPicked, status?.all_devices_available])

  const pickScope = (next: SyncScope) => {
    setUserPicked(true)
    setScope(next)
  }

  const overviewQuery = useQuery({
    queryKey: ['syncOverview', projectName, scope],
    queryFn: () => getSyncOverview(scope),
    // Only the merged path hits the union query; the this-device stub carries
    // no data worth fetching, so we render a local placeholder instead.
    enabled: status?.enabled === true && scope === 'all-devices',
    staleTime: 30_000,
  })
  const overview = overviewQuery.data
  const merged = overview && overview.merged ? overview : null

  // ── status still loading / failed ──────────────────────────────────────
  if (statusQuery.isLoading) {
    return <LoadingSpinner message="Checking sync status..." />
  }
  if (statusQuery.error || !status) {
    return (
      <div className="bg-red-50 dark:bg-red-900/20 border border-red-300 dark:border-red-800 rounded-lg p-3 text-red-700 dark:text-red-400 text-sm">
        Failed to load sync status:{' '}
        {statusQuery.error instanceof Error ? statusQuery.error.message : 'Unknown error'}
      </div>
    )
  }

  // ── sync unconfigured — clean empty state, no toggle ───────────────────
  if (!status.enabled) {
    return (
      <div className="space-y-3">
        <EmptyState
          icon={<IconArrowsLeftRight size={28} />}
          title="Multi-device sync isn't set up"
          description="Sync merges your coding analytics across machines — end-to-end encrypted, opt-in, and read-only on pull. Set it up on this device to unlock the all-devices view."
        />
        <div className="flex justify-center">
          <code className="font-mono text-xs bg-gray-100 dark:bg-gray-800 text-gray-700 dark:text-gray-300 rounded px-2.5 py-1.5 border border-gray-200 dark:border-gray-700">
            stackunderflow sync init
          </code>
        </div>
      </div>
    )
  }

  // ── configured ─────────────────────────────────────────────────────────
  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between gap-3 flex-wrap">
        <div className="flex items-center gap-2">
          <IconArrowsLeftRight size={16} className="text-gray-500" />
          <h2 className="text-sm font-semibold text-gray-800 dark:text-gray-200">Devices</h2>
          <span className="text-xs text-gray-500">merged analytics across your machines</span>
          {merged && merged.merge_warnings > 0 && (
            <span
              title={`${merged.merge_warnings} duplicate session${merged.merge_warnings === 1 ? '' : 's'} seen on more than one device were counted once (local-then-lowest-device tiebreak).`}
            >
              <Badge color="yellow" size="sm">
                <IconAlertTriangle size={11} className="mr-1" />
                {merged.merge_warnings} merge warning{merged.merge_warnings === 1 ? '' : 's'}
              </Badge>
            </span>
          )}
        </div>
        <div className="flex items-center gap-2 flex-wrap">
          {(merged?.generated_at || status.scanned_at) && (
            <span className="text-[11px] text-gray-400 dark:text-gray-500 tabular-nums whitespace-nowrap">
              {formatTs(merged?.generated_at ?? status.scanned_at)}
            </span>
          )}
          <button
            type="button"
            onClick={() => {
              statusQuery.refetch()
              if (scope === 'all-devices') overviewQuery.refetch()
            }}
            disabled={statusQuery.isFetching || overviewQuery.isFetching}
            className="inline-flex items-center gap-1.5 px-2.5 py-1.5 text-xs font-medium rounded border border-gray-300 dark:border-gray-700 text-gray-700 dark:text-gray-300 hover:bg-gray-50 dark:hover:bg-gray-800 disabled:opacity-50"
          >
            <IconRefresh
              size={12}
              className={statusQuery.isFetching || overviewQuery.isFetching ? 'animate-spin' : ''}
            />
            Refresh
          </button>
          <ScopeToggle scope={scope} onChange={pickScope} />
        </div>
      </div>

      {scope === 'this-device' ? (
        <div className="rounded-lg border border-gray-200 dark:border-gray-800 bg-gray-50/60 dark:bg-gray-800/20 p-4 text-sm text-gray-600 dark:text-gray-300 space-y-1">
          <p className="font-medium text-gray-700 dark:text-gray-200">Showing this device only.</p>
          <p className="text-xs text-gray-500 dark:text-gray-400">
            Per-device analytics live in the Overview and Cost tabs. Switch to{' '}
            <span className="font-medium">All devices</span> to merge{' '}
            {status.peer_count > 0
              ? `${status.peer_count} peer${status.peer_count === 1 ? '' : 's'}`
              : 'other machines'}{' '}
            into one cross-device view.
            {!status.all_devices_available &&
              ' No peer data has been pulled yet — run `stackunderflow sync pull` first.'}
          </p>
        </div>
      ) : overviewQuery.isLoading ? (
        <LoadingSpinner message="Merging devices..." />
      ) : overviewQuery.error ? (
        <div className="bg-red-50 dark:bg-red-900/20 border border-red-300 dark:border-red-800 rounded-lg p-3 text-red-700 dark:text-red-400 text-sm">
          Failed to merge devices:{' '}
          {overviewQuery.error instanceof Error ? overviewQuery.error.message : 'Unknown error'}
        </div>
      ) : merged ? (
        <MergedView merged={merged} status={status} />
      ) : (
        <EmptyState
          icon={<IconUsers size={28} />}
          title="No merged data"
          description="Sync is configured but nothing has been merged yet. Push from another device, then run stackunderflow sync pull here."
        />
      )}
    </div>
  )
}
