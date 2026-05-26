// ---------------------------------------------------------------------------
// Wave 4F — ETL pipeline status badge.
//
// Mounted in the dashboard header (left of the user/settings/theme controls).
// Polls `/api/etl/status` every 10s via React Query and renders a colour-coded
// chip reflecting the pipeline's health (live / syncing / stale / error).
// Click-through opens a popover with per-mart watermarks, per-provider event
// counts, and the watcher running state.
//
// Graceful degradation: if the route 404s (Wave 4C not merged yet) the badge
// renders a muted "ETL pipeline not ready" chip rather than crashing.
//
// Responsive: secondary text is hidden under 600px (sm: breakpoint) so only
// the dot + label survives on narrow screens.
// ---------------------------------------------------------------------------

import { useEffect, useRef, useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import {
  EtlPipelineNotReadyError,
  etlHealthColor,
  formatEtlBadgeText,
  formatLagDuration,
  getEtlStatus,
} from '../../services/api'
import type { EtlStatusResponse } from '../../types/api'

const REFRESH_MS = 10_000
const REFRESH_MS_ACTIVE = 2_000   // Tighten polling while a job is in flight
const STALE_MS = 5_000

export default function EtlStatusBadge() {
  const [popoverOpen, setPopoverOpen] = useState(false)
  const popoverRef = useRef<HTMLDivElement>(null)
  const previousHealth = useRef<string | null>(null)
  const [toast, setToast] = useState<string | null>(null)

  const { data, error, isLoading } = useQuery({
    queryKey: ['etl-status'],
    queryFn: getEtlStatus,
    // While a backfill is running we want sub-second feedback in the
    // header — drop the poll cadence to 2s. Otherwise the standard 10s
    // tick is fine.
    refetchInterval: (query) => {
      const d = query.state.data as EtlStatusResponse | undefined
      return d?.current_job?.status === 'running' ? REFRESH_MS_ACTIVE : REFRESH_MS
    },
    staleTime: STALE_MS,
    // Don't retry on 404 (route not deployed yet) — keeps the console clean.
    retry: (failureCount, err) => {
      if (err instanceof EtlPipelineNotReadyError) return false
      return failureCount < 2
    },
  })

  // Toast on transition to live ("ETL caught up"). Suppressed while a
  // backfill failure is still inside its TTL window — the assembler
  // has escalated health to "error" for that case, so the live →
  // live transition we'd normally celebrate isn't actually a recovery.
  useEffect(() => {
    if (!data) return
    const prev = previousHealth.current
    const recentlyFailed = data.last_job?.status === 'failed'
    if (prev && prev !== 'live' && data.health === 'live' && !recentlyFailed) {
      setToast('ETL caught up')
      const t = window.setTimeout(() => setToast(null), 3500)
      return () => window.clearTimeout(t)
    }
    previousHealth.current = data.health
  }, [data])

  // Click-outside to close the popover.
  useEffect(() => {
    if (!popoverOpen) return
    function handler(e: MouseEvent) {
      if (popoverRef.current && !popoverRef.current.contains(e.target as Node)) {
        setPopoverOpen(false)
      }
    }
    document.addEventListener('mousedown', handler)
    return () => document.removeEventListener('mousedown', handler)
  }, [popoverOpen])

  // Route-not-ready state — render disabled chip, swallow the error.
  if (error instanceof EtlPipelineNotReadyError) {
    return (
      <div
        className="hidden md:inline-flex items-center gap-1.5 px-2 py-1 text-[11px] rounded border border-gray-300 dark:border-gray-700 bg-gray-100/60 dark:bg-gray-800/40 text-gray-500 dark:text-gray-500"
        title="The ETL status route is not available on this build."
        aria-label="ETL pipeline not ready"
      >
        <span className="h-1.5 w-1.5 rounded-full bg-gray-400 dark:bg-gray-600" aria-hidden="true" />
        <span className="hidden lg:inline">ETL pipeline not ready</span>
      </div>
    )
  }

  // Generic failure state — keep the badge visible but red so the user
  // notices, with the underlying error in the title attribute.
  if (error) {
    return (
      <div
        className="inline-flex items-center gap-1.5 px-2 py-1 text-[11px] rounded border border-red-300 dark:border-red-800 bg-red-50 dark:bg-red-900/20 text-red-700 dark:text-red-400"
        title={`Failed to fetch ETL status: ${error instanceof Error ? error.message : String(error)}`}
        aria-label="ETL status fetch failed"
      >
        <span className="h-1.5 w-1.5 rounded-full bg-red-500" aria-hidden="true" />
        <span className="hidden sm:inline">ETL fetch failed</span>
      </div>
    )
  }

  if (isLoading || !data) {
    return (
      <div
        className="inline-flex items-center gap-1.5 px-2 py-1 text-[11px] rounded border border-gray-300 dark:border-gray-700 bg-white dark:bg-gray-800 text-gray-500"
        aria-label="Loading ETL status"
      >
        <span className="h-1.5 w-1.5 rounded-full bg-gray-400 dark:bg-gray-600 animate-pulse" aria-hidden="true" />
        <span className="hidden sm:inline">ETL…</span>
      </div>
    )
  }

  const colour = etlHealthColor(data.health)
  const text = formatEtlBadgeText(data)

  // Tailwind fragments per badge colour. Done inline so the colour set is
  // discoverable in one place; the four health states map 1:1 to four
  // tailwind class strings.
  const badgeBg: Record<string, string> = {
    green:
      'border-green-300 bg-green-50 text-green-800 dark:border-green-800 dark:bg-green-900/30 dark:text-green-300',
    blue: 'border-blue-300 bg-blue-50 text-blue-800 dark:border-blue-800 dark:bg-blue-900/30 dark:text-blue-300',
    yellow:
      'border-yellow-300 bg-yellow-50 text-yellow-800 dark:border-yellow-800 dark:bg-yellow-900/30 dark:text-yellow-300',
    red: 'border-red-300 bg-red-50 text-red-700 dark:border-red-800 dark:bg-red-900/20 dark:text-red-400',
  }

  return (
    <div className="relative" ref={popoverRef}>
      <button
        type="button"
        onClick={() => setPopoverOpen(v => !v)}
        className={`inline-flex items-center gap-1.5 px-2 py-1 text-[11px] rounded border transition-colors ${badgeBg[colour.badge]}`}
        title={text}
        aria-label={text}
        aria-expanded={popoverOpen}
      >
        <span
          className={`h-1.5 w-1.5 rounded-full ${colour.dot} ${colour.pulse ? 'animate-pulse' : ''}`}
          aria-hidden="true"
        />
        <span className="hidden sm:inline whitespace-nowrap">{text}</span>
      </button>

      {popoverOpen && <EtlStatusPopover data={data} />}

      {toast && (
        <div
          role="status"
          className="absolute right-0 top-full mt-2 z-50 px-3 py-1.5 rounded border border-green-300 dark:border-green-800 bg-green-50 dark:bg-green-900/40 text-[11px] text-green-800 dark:text-green-300 shadow-lg whitespace-nowrap"
        >
          {toast}
        </div>
      )}
    </div>
  )
}

// ---------------------------------------------------------------------------
// Popover — dense breakdown rendered when the badge is clicked.
// ---------------------------------------------------------------------------

interface EtlStatusPopoverProps {
  data: import('../../types/api').EtlStatusResponse
}

function EtlStatusPopover({ data }: EtlStatusPopoverProps) {
  const { watcher, marts, events, lag_seconds, health } = data
  const martEntries = Object.entries(marts).sort(([a], [b]) => a.localeCompare(b))
  const providerEntries = Object.entries(events.by_provider).sort(([, a], [, b]) => b - a)

  return (
    <div
      className="absolute right-0 top-full mt-2 w-80 z-50 bg-white dark:bg-gray-800 border border-gray-300 dark:border-gray-700 rounded-lg shadow-xl text-xs"
      role="dialog"
      aria-label="ETL pipeline status"
    >
      {/* Header */}
      <div className="px-3 py-2 border-b border-gray-200 dark:border-gray-700">
        <div className="flex items-center justify-between">
          <span className="font-semibold text-gray-800 dark:text-gray-200">ETL pipeline</span>
          <span className="font-mono uppercase tracking-wide text-[10px] text-gray-500">{health}</span>
        </div>
        <div className="text-[11px] text-gray-500 mt-0.5">
          Lag {formatLagDuration(lag_seconds)} · {events.total.toLocaleString()} events total
        </div>
      </div>

      {/* Watcher */}
      <div className="px-3 py-2 border-b border-gray-200 dark:border-gray-700">
        <div className="flex items-center justify-between">
          <span className="text-gray-600 dark:text-gray-400">Watcher</span>
          <span className="font-mono text-gray-800 dark:text-gray-200">
            {watcher.enabled ? (watcher.running ? 'running' : 'enabled · idle') : 'disabled'}
          </span>
        </div>
        <div className="flex items-center justify-between mt-1">
          <span className="text-gray-600 dark:text-gray-400">Last refresh</span>
          <span className="font-mono text-gray-800 dark:text-gray-200">
            {watcher.last_refresh_ts ?? 'never'}
          </span>
        </div>
        <div className="flex items-center justify-between mt-1">
          <span className="text-gray-600 dark:text-gray-400">Last cycle</span>
          <span className="font-mono text-gray-800 dark:text-gray-200">
            {watcher.events_in_last_cycle != null ? (
              <>
                {watcher.events_in_last_cycle.toLocaleString()} event
                {watcher.events_in_last_cycle === 1 ? '' : 's'}
              </>
            ) : (
              'unknown'
            )}
          </span>
        </div>
      </div>

      {/* Marts */}
      <div className="px-3 py-2 border-b border-gray-200 dark:border-gray-700">
        <div className="text-gray-600 dark:text-gray-400 mb-1.5">Marts</div>
        {martEntries.length === 0 ? (
          <div className="text-gray-500 italic">No marts registered.</div>
        ) : (
          <ul className="space-y-1">
            {martEntries.map(([name, mart]) => {
              const lag = events.max_id - mart.watermark
              return (
                <li key={name} className="flex items-center justify-between">
                  <span className="font-mono text-gray-700 dark:text-gray-300">{name}</span>
                  <span className="font-mono text-[10px] text-gray-500 tabular-nums">
                    wm {mart.watermark.toLocaleString()} / {events.max_id.toLocaleString()}
                    {lag > 0 ? ` (-${lag.toLocaleString()})` : ''} · {mart.row_count.toLocaleString()} rows
                  </span>
                </li>
              )
            })}
          </ul>
        )}
      </div>

      {/* Providers */}
      <div className="px-3 py-2">
        <div className="text-gray-600 dark:text-gray-400 mb-1.5">Events by provider</div>
        {providerEntries.length === 0 ? (
          <div className="text-gray-500 italic">No events ingested yet.</div>
        ) : (
          <ul className="space-y-1">
            {providerEntries.map(([provider, count]) => (
              <li key={provider} className="flex items-center justify-between">
                <span className="font-mono text-gray-700 dark:text-gray-300">{provider}</span>
                <span className="font-mono text-[10px] text-gray-500 tabular-nums">
                  {count.toLocaleString()}
                </span>
              </li>
            ))}
          </ul>
        )}
      </div>
    </div>
  )
}
