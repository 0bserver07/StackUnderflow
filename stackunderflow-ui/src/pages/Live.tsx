// Live observability tab — Spec 13.
//
// Three panes:
//   1. Event stream (auto-scrolling, last 100 events + tool_calls)
//   2. Burn ticker (rolling $5/min, $1/hr, $TODAY, projected month-end)
//   3. P95 tool latency by tool (sparkline + number)
//
// Watcher-not-running banner: when the snapshot reports
// `watcher.running === false`, the tab renders a yellow banner above
// the panes explaining that the live stream depends on the filesystem
// watcher and pointing at `stackunderflow start` (without `--no-watcher`)
// as the fix. The SSE stream still opens — the stats route surfaces
// historical burn / latency from the existing store, so the panes
// remain useful even on a static dashboard.

import { useMemo } from 'react'
import { useQuery } from '@tanstack/react-query'
import { IconBolt, IconClockHour4, IconActivity, IconAlertTriangle } from '@tabler/icons-react'
import { getLiveStats } from '../services/live'
import { useEventStream } from '../hooks/useEventStream'
import { formatCost, formatNumber } from '../services/format'
import { useCurrency } from '../services/currency'
import EmptyState from '../components/common/EmptyState'
import LoadingSpinner from '../components/common/LoadingSpinner'
import type { LiveToolLatency } from '../types/api'

function formatSecondsCompact(s: number): string {
  if (!Number.isFinite(s) || s < 0) return '-'
  if (s < 1) return `${(s * 1000).toFixed(0)}ms`
  if (s < 60) return `${s.toFixed(1)}s`
  if (s < 3600) return `${(s / 60).toFixed(1)}m`
  return `${(s / 3600).toFixed(1)}h`
}

function formatTime(ts: string): string {
  const d = new Date(ts)
  if (Number.isNaN(d.getTime())) return ts
  return d.toLocaleTimeString(undefined, { hour12: false })
}

export default function Live() {
  const { currency } = useCurrency()

  // Snapshot for the initial paint — the stream provides updates after
  // the first round-trip. Re-fetch every 30s so the page is still
  // useful if the stream silently dies (browser tab suspended, etc.).
  const { data: snapshot, isLoading } = useQuery({
    queryKey: ['liveStats'],
    queryFn: getLiveStats,
    refetchInterval: 30_000,
    refetchOnWindowFocus: true,
  })

  const stream = useEventStream(true)

  // The live burn supersedes the snapshot once the first burn_tick
  // arrives. Pre-tick we render the snapshot so the cards aren't
  // empty for the first 5s of a connection.
  const burn = stream.burn ?? snapshot?.burn ?? null
  // Same logic for the latency table.
  const latency: LiveToolLatency[] = snapshot?.tool_latency ?? []

  // Watcher state: snapshot is authoritative pre-handshake, the SSE
  // ready event takes over once stream.connected flips to true.
  const watcherRunning =
    stream.watcher?.running ?? snapshot?.watcher.running ?? 'unknown'
  const watcherDown = watcherRunning === false

  // Combined event/tool_call stream: interleave by ts so the user sees
  // chronological order, not two parallel walls. Capped at 100.
  //
  // Both source buffers already arrive newest-first (useEventStream prepends
  // each new row), so we merge them with a single linear pass instead of
  // concatenating and re-sorting all ~200 rows on every SSE frame. Equal
  // timestamps keep events ahead of tool_calls, matching the previous stable
  // sort. String compare on ISO timestamps == chronological, same as before.
  const merged = useMemo(() => {
    type Item =
      | { kind: 'event'; ts: string; row: typeof stream.events[number] }
      | { kind: 'tool_call'; ts: string; row: typeof stream.toolCalls[number] }
    const { events, toolCalls } = stream
    const out: Item[] = []
    let i = 0
    let j = 0
    while (out.length < 100 && (i < events.length || j < toolCalls.length)) {
      const e = i < events.length ? events[i] : null
      const t = j < toolCalls.length ? toolCalls[j] : null
      if (e && (!t || e.ts >= t.ts)) {
        out.push({ kind: 'event', ts: e.ts, row: e })
        i++
      } else if (t) {
        out.push({ kind: 'tool_call', ts: t.ts, row: t })
        j++
      } else {
        break
      }
    }
    return out
  }, [stream.events, stream.toolCalls])

  if (isLoading && !stream.connected) {
    return <LoadingSpinner message="Loading live snapshot..." />
  }

  return (
    <div className="max-w-7xl mx-auto p-6 space-y-6">
      {/* Header */}
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-xl font-bold text-gray-900 dark:text-gray-100">
            Live
          </h1>
          <p className="text-sm text-gray-500 mt-0.5">
            Real-time across all active sessions.
            {stream.connected ? (
              <span className="ml-2 inline-flex items-center gap-1 text-emerald-600 dark:text-emerald-400">
                <span className="w-1.5 h-1.5 rounded-full bg-emerald-500 animate-pulse" />
                streaming
              </span>
            ) : (
              <span className="ml-2 text-gray-500">connecting…</span>
            )}
          </p>
        </div>
      </div>

      {/* Watcher-not-running banner. Only renders when we know the
          watcher is *down* (running === false). For "unknown" — i.e.
          the lifespan never ran the watcher handle — we stay silent
          since the user might be running --no-watcher intentionally
          or just opened the page from the CLI. */}
      {watcherDown && (
        <div
          role="alert"
          data-testid="watcher-down-banner"
          className="flex items-start gap-3 bg-yellow-50 dark:bg-yellow-900/20 border border-yellow-300 dark:border-yellow-800 rounded-lg p-4 text-yellow-800 dark:text-yellow-300"
        >
          <IconAlertTriangle size={20} className="shrink-0 mt-0.5" />
          <div className="text-sm space-y-1">
            <p className="font-medium">Filesystem watcher is not running.</p>
            <p>
              The live stream depends on the watcher to ingest new sessions sub-second.
              Restart the server without <code className="px-1 py-0.5 bg-yellow-100 dark:bg-yellow-900/40 rounded text-xs">--no-watcher</code> to
              resume real-time updates. Burn rate and tool latency below reflect data already in the store.
            </p>
          </div>
        </div>
      )}

      {/* Burn ticker — three KPIs across the top. */}
      <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
        <BurnCard
          label="Per minute"
          value={burn ? formatCost(burn.per_minute, currency) : '-'}
          icon={<IconBolt size={18} />}
          subtle={burn ? `${burn.window_minutes}-min rolling avg` : ''}
        />
        <BurnCard
          label="Per hour"
          value={burn ? formatCost(burn.per_hour, currency) : '-'}
          icon={<IconClockHour4 size={18} />}
          subtle={burn ? `${formatCost(burn.window_cost, currency)} in window` : ''}
        />
        <BurnCard
          label="Today"
          value={burn ? formatCost(burn.today_cost, currency) : '-'}
          icon={<IconActivity size={18} />}
          subtle={burn ? `MTD ${formatCost(burn.month_to_date, currency)}` : ''}
        />
        <BurnCard
          label="Projected month-end"
          value={burn ? formatCost(burn.projected_month_end, currency) : '-'}
          icon={<IconActivity size={18} />}
          subtle="straight-line extrapolation"
        />
      </div>

      <div className="grid grid-cols-1 lg:grid-cols-3 gap-6">
        {/* Event stream — the live wall. Spans 2 cols on wide screens. */}
        <div className="lg:col-span-2 bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-4 border border-gray-200 dark:border-gray-800">
          <div className="flex items-center justify-between mb-3">
            <h3 className="text-sm font-medium text-gray-700 dark:text-gray-300">
              Event stream
            </h3>
            <span className="text-xs text-gray-500">
              {merged.length} of last 100
            </span>
          </div>
          <div
            className="space-y-1 overflow-y-auto max-h-[500px] font-mono text-xs"
            data-testid="live-event-stream"
          >
            {merged.length === 0 ? (
              <EmptyState
                title="Waiting for activity"
                description={
                  watcherDown
                    ? 'Watcher is not running — start it to see live events.'
                    : 'Run a Claude / Codex session and events will land here in real time.'
                }
              />
            ) : (
              merged.map((item) => (
                <div
                  key={`${item.kind}-${item.row.id}`}
                  className="flex items-baseline gap-3 py-1 border-b border-gray-200/50 dark:border-gray-800/50"
                >
                  <span className="text-gray-500 tabular-nums w-20 shrink-0">
                    {formatTime(item.ts)}
                  </span>
                  <span
                    className={`px-1.5 py-0.5 rounded text-[10px] font-medium uppercase tracking-wide shrink-0 ${
                      item.kind === 'tool_call'
                        ? 'bg-indigo-500/15 text-indigo-700 dark:text-indigo-300'
                        : 'bg-emerald-500/15 text-emerald-700 dark:text-emerald-300'
                    }`}
                  >
                    {item.kind === 'tool_call' ? item.row.tool_name : 'event'}
                  </span>
                  <span className="text-gray-700 dark:text-gray-300 truncate">
                    {item.kind === 'tool_call' ? (
                      <>
                        {item.row.project_name ?? item.row.project_slug ?? `project ${item.row.project_id}`}
                        {item.row.file_path ? (
                          <span className="text-gray-500"> · {item.row.file_path}</span>
                        ) : null}
                        {item.row.byte_count != null ? (
                          <span className="text-gray-500"> · {formatNumber(item.row.byte_count)}b</span>
                        ) : null}
                      </>
                    ) : (
                      <>
                        {item.row.project_name ?? item.row.project_slug ?? `project ${item.row.project_id}`}
                        <span className="text-gray-500"> · {item.row.model}</span>
                        <span className="text-gray-500"> · {formatCost(item.row.cost_usd, currency)}</span>
                      </>
                    )}
                  </span>
                </div>
              ))
            )}
          </div>
        </div>

        {/* P95 tool latency table — 1 col on wide screens. */}
        <div className="bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-4 border border-gray-200 dark:border-gray-800">
          <h3 className="text-sm font-medium text-gray-700 dark:text-gray-300 mb-3">
            Tool latency (P50 / P95 / P99)
          </h3>
          {latency.length === 0 ? (
            <p className="text-xs text-gray-500">No tool-call samples in the last 24h.</p>
          ) : (
            <div className="space-y-2">
              {latency.map((row) => (
                <div
                  key={row.tool_name}
                  className="flex items-baseline justify-between gap-2 text-xs"
                  data-testid="latency-row"
                >
                  <span className="font-medium text-gray-700 dark:text-gray-300 truncate">
                    {row.tool_name}
                  </span>
                  <div className="flex items-baseline gap-3 tabular-nums shrink-0">
                    <span className="text-gray-500" title={`${row.samples} samples`}>
                      {row.samples}n
                    </span>
                    <span className="text-gray-700 dark:text-gray-300" title="P50">
                      {formatSecondsCompact(row.p50)}
                    </span>
                    <span className="text-indigo-600 dark:text-indigo-400" title="P95">
                      {formatSecondsCompact(row.p95)}
                    </span>
                    <span className="text-rose-600 dark:text-rose-400" title="P99">
                      {formatSecondsCompact(row.p99)}
                    </span>
                  </div>
                </div>
              ))}
            </div>
          )}
          <p className="mt-3 text-[10px] text-gray-500 leading-snug">
            Latency derived from <code>messages.timestamp</code> deltas between a tool_use and the next
            message in the same session — coarse, only as fine as the source-file write cadence.
          </p>
        </div>
      </div>
    </div>
  )
}

interface BurnCardProps {
  label: string
  value: string
  icon: React.ReactNode
  subtle?: string
}

function BurnCard({ label, value, icon, subtle }: BurnCardProps) {
  return (
    <div className="bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-4 border border-gray-200 dark:border-gray-800">
      <div className="flex items-center justify-between text-xs text-gray-500 uppercase tracking-wider">
        <span>{label}</span>
        <span className="text-gray-400 dark:text-gray-600">{icon}</span>
      </div>
      <div className="text-2xl font-bold text-gray-900 dark:text-gray-100 mt-1 tabular-nums">
        {value}
      </div>
      {subtle && <div className="text-[10px] text-gray-500 mt-0.5">{subtle}</div>}
    </div>
  )
}
