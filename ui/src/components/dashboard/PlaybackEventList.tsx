/**
 * PlaybackEventList — the scrollable list of tool calls (left of the detail
 * pane on the Playback tab). Synced with the scrubber: the current row is
 * highlighted and auto-scrolled into view; clicking a row seeks there.
 *
 * v1 renders a plain list (browsers handle ~1k rows fine); the spec's
 * "virtualized" is a future refinement gated on real-data perf — wiring a
 * windowing lib is deferred until a session that big actually shows up.
 *
 * Spec: .notes/specs/10-playback-timeline.md
 */

import { useEffect, useRef } from 'react'
import { IconAlertTriangle, IconCircleCheck } from '@tabler/icons-react'

import type { PlaybackEvent } from '../../types/api'
import { toolAccent } from './playbackColors'

interface PlaybackEventListProps {
  events: PlaybackEvent[]
  /** Index within `events` of the current step, or -1. */
  currentIndex: number
  onSelect: (index: number) => void
}

export default function PlaybackEventList({ events, currentIndex, onSelect }: PlaybackEventListProps) {
  const currentRef = useRef<HTMLButtonElement | null>(null)

  useEffect(() => {
    currentRef.current?.scrollIntoView({ block: 'nearest' })
  }, [currentIndex])

  if (events.length === 0) {
    return (
      <div className="rounded-md border border-dashed border-gray-300 dark:border-gray-700 p-6 text-center text-sm text-gray-500">
        No events match the current filter.
      </div>
    )
  }

  return (
    <div
      className="rounded-md border border-gray-200 dark:border-gray-800 overflow-y-auto max-h-[28rem]"
      data-testid="playback-event-list"
      role="listbox"
      aria-label="Tool-call timeline"
    >
      {events.map((ev, i) => {
        const accent = toolAccent(ev.tool_name)
        const isCurrent = i === currentIndex
        return (
          <button
            key={`${ev.message_id}-${ev.seq}`}
            ref={isCurrent ? currentRef : undefined}
            type="button"
            onClick={() => onSelect(i)}
            role="option"
            aria-selected={isCurrent}
            data-seq={ev.seq}
            className={`w-full text-left flex items-center gap-2 px-3 py-1.5 border-b border-gray-100 dark:border-gray-800/70 last:border-b-0 ${
              isCurrent
                ? 'bg-indigo-50 dark:bg-indigo-900/20'
                : 'hover:bg-gray-50 dark:hover:bg-gray-800/40'
            }`}
          >
            <span className="text-[10px] tabular-nums text-gray-400 w-8 flex-shrink-0 text-right">{ev.seq}</span>
            <span className={`w-1.5 h-1.5 rounded-full flex-shrink-0 ${accent.dot}`} />
            <span className="text-sm text-gray-800 dark:text-gray-200 truncate flex-1">{ev.summary}</span>
            {ev.success === false && (
              <IconAlertTriangle size={13} className="text-red-500 flex-shrink-0" aria-label="failed" />
            )}
            {ev.success === true && (
              <IconCircleCheck size={13} className="text-green-600/70 dark:text-green-400/70 flex-shrink-0" aria-label="ok" />
            )}
          </button>
        )
      })}
    </div>
  )
}
