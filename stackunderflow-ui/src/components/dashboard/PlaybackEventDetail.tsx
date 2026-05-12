/**
 * PlaybackEventDetail — the right-hand pane of the Playback tab.
 *
 * Shows everything we know about the currently-scrubbed tool call: tool name,
 * one-line summary, target path, a success/failure/unknown badge, byte count,
 * duration, and the 200-char payload excerpt (for Edit calls the excerpt is a
 * `- old / + new` fragment, which renders fine as monospace text).
 *
 * Spec: .notes/specs/10-playback-timeline.md
 */

import {
  IconAlertTriangle,
  IconCircleCheck,
  IconClock,
  IconFileText,
  IconHelpCircle,
  IconRuler2,
} from '@tabler/icons-react'

import type { PlaybackEvent } from '../../types/api'
import { toolAccent } from './playbackColors'

function fmtBytes(n: number | null): string {
  if (n === null || n === undefined) return '—'
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`
  return `${(n / (1024 * 1024)).toFixed(1)} MB`
}

function fmtDuration(ms: number | null): string {
  if (ms === null || ms === undefined) return '—'
  if (ms < 1000) return `${ms} ms`
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)} s`
  const m = Math.floor(ms / 60_000)
  const s = Math.round((ms % 60_000) / 1000)
  return `${m}m ${s}s`
}

function fmtTs(iso: string): string {
  try {
    return new Date(iso).toLocaleString(undefined, {
      month: 'short',
      day: 'numeric',
      hour: '2-digit',
      minute: '2-digit',
      second: '2-digit',
    })
  } catch {
    return iso
  }
}

function SuccessBadge({ success }: { success: boolean | null }) {
  if (success === true) {
    return (
      <span className="inline-flex items-center gap-1 text-xs px-2 py-0.5 rounded bg-green-100 text-green-800 dark:bg-green-900/40 dark:text-green-300">
        <IconCircleCheck size={12} /> ok
      </span>
    )
  }
  if (success === false) {
    return (
      <span className="inline-flex items-center gap-1 text-xs px-2 py-0.5 rounded bg-red-100 text-red-800 dark:bg-red-900/40 dark:text-red-300">
        <IconAlertTriangle size={12} /> failed
      </span>
    )
  }
  return (
    <span className="inline-flex items-center gap-1 text-xs px-2 py-0.5 rounded bg-gray-100 text-gray-600 dark:bg-gray-800 dark:text-gray-400">
      <IconHelpCircle size={12} /> unknown
    </span>
  )
}

interface MetaProps {
  icon: React.ReactNode
  label: string
  value: string
}

function Meta({ icon, label, value }: MetaProps) {
  return (
    <div className="rounded-md border border-gray-200 dark:border-gray-800 px-3 py-2">
      <div className="flex items-center gap-1.5 text-xs text-gray-500">
        {icon}
        {label}
      </div>
      <div className="text-sm font-medium text-gray-800 dark:text-gray-200 mt-0.5 truncate" title={value}>
        {value}
      </div>
    </div>
  )
}

export default function PlaybackEventDetail({ event }: { event: PlaybackEvent | null }) {
  if (!event) {
    return (
      <div className="rounded-md border border-dashed border-gray-300 dark:border-gray-700 p-6 text-center text-sm text-gray-500">
        Pick an event on the timeline or in the list to see its detail.
      </div>
    )
  }
  const accent = toolAccent(event.tool_name)
  return (
    <div className="space-y-3" data-testid="playback-event-detail">
      <header className="flex items-start justify-between gap-3 border-b border-gray-200 dark:border-gray-800 pb-3">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <span className={`text-[11px] font-semibold uppercase tracking-wider px-1.5 py-0.5 rounded ${accent.chip}`}>
              {event.tool_name}
            </span>
            <span className="text-xs text-gray-500">step {event.seq}</span>
            <SuccessBadge success={event.success} />
          </div>
          <h3 className="text-base font-semibold text-gray-900 dark:text-gray-100 mt-1 break-words">
            {event.summary}
          </h3>
          {event.target_path && (
            <div className="text-xs text-gray-500 mt-0.5 break-all font-mono">{event.target_path}</div>
          )}
        </div>
      </header>

      <div className="grid grid-cols-2 sm:grid-cols-3 gap-2">
        <Meta icon={<IconRuler2 size={14} />} label="Payload" value={fmtBytes(event.byte_count)} />
        <Meta icon={<IconClock size={14} />} label="Duration" value={fmtDuration(event.duration_ms)} />
        <Meta icon={<IconClock size={14} />} label="When" value={fmtTs(event.ts)} />
      </div>

      <div className="rounded-md border border-gray-200 dark:border-gray-800 p-3">
        <div className="flex items-center gap-1.5 text-xs uppercase tracking-wider text-gray-500 mb-1.5">
          <IconFileText size={12} /> Payload excerpt
        </div>
        {event.payload_excerpt ? (
          <pre className="text-xs text-gray-800 dark:text-gray-200 whitespace-pre-wrap break-words font-mono leading-relaxed max-h-64 overflow-auto">
            {event.payload_excerpt}
          </pre>
        ) : (
          <div className="text-xs text-gray-500 italic">
            (excerpt not loaded — re-fetch with payloads enabled)
          </div>
        )}
      </div>
    </div>
  )
}
