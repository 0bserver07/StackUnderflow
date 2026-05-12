/**
 * PlaybackScrubber — the horizontal timeline at the top of the Playback tab.
 *
 * Each event is a tick, evenly spaced (a "step through N tool calls" model is
 * more useful here than a true time-axis — gaps between calls are mostly the
 * agent thinking, not interesting state changes). Ticks are colour-coded by
 * tool; the current step is enlarged; hovering a tick shows its summary; a
 * click seeks. A progress fill shows how far through the stream we are.
 *
 * For very long streams (1000+ events) the per-tick rendering still stays
 * cheap — each tick is a tiny absolutely-positioned div — but the component
 * also accepts an already-filtered `events` list, so the common case (a
 * filter chip active) is much smaller.
 *
 * Spec: .notes/specs/10-playback-timeline.md
 */

import { useMemo, useRef, useState } from 'react'

import type { PlaybackEvent } from '../../types/api'
import { toolAccent } from './playbackColors'

interface PlaybackScrubberProps {
  /** Events in display order (already tool-filtered by the parent). */
  events: PlaybackEvent[]
  /** Index *within `events`* of the current step, or -1 when nothing is selected. */
  currentIndex: number
  /** Called with the new index when the user clicks a tick / the track. */
  onSeek: (index: number) => void
}

export default function PlaybackScrubber({ events, currentIndex, onSeek }: PlaybackScrubberProps) {
  const trackRef = useRef<HTMLDivElement | null>(null)
  const [hoverIndex, setHoverIndex] = useState<number | null>(null)

  const n = events.length
  const progressPct = useMemo(() => {
    if (n <= 1 || currentIndex < 0) return 0
    return (currentIndex / (n - 1)) * 100
  }, [n, currentIndex])

  if (n === 0) {
    return (
      <div className="h-12 rounded-md border border-dashed border-gray-300 dark:border-gray-700 flex items-center justify-center text-xs text-gray-500">
        No events to scrub.
      </div>
    )
  }

  // Click anywhere on the track → nearest tick.
  const handleTrackClick = (e: React.MouseEvent<HTMLDivElement>) => {
    const el = trackRef.current
    if (!el) return
    const rect = el.getBoundingClientRect()
    const frac = Math.min(1, Math.max(0, (e.clientX - rect.left) / rect.width))
    const idx = Math.round(frac * (n - 1))
    onSeek(idx)
  }

  const hovered = hoverIndex !== null ? events[hoverIndex] : null

  return (
    <div className="space-y-1.5" data-testid="playback-scrubber">
      <div
        ref={trackRef}
        onClick={handleTrackClick}
        role="slider"
        aria-valuemin={0}
        aria-valuemax={n - 1}
        aria-valuenow={currentIndex < 0 ? 0 : currentIndex}
        aria-label="Playback position"
        className="relative h-10 rounded-md bg-gray-100 dark:bg-gray-800/60 cursor-pointer select-none overflow-hidden"
      >
        {/* progress fill */}
        <div
          className="absolute inset-y-0 left-0 bg-indigo-500/15 dark:bg-indigo-400/15 pointer-events-none"
          style={{ width: `${progressPct}%` }}
        />
        {/* ticks */}
        {events.map((ev, i) => {
          const left = n <= 1 ? 0 : (i / (n - 1)) * 100
          const accent = toolAccent(ev.tool_name)
          const isCurrent = i === currentIndex
          const isFailed = ev.success === false
          return (
            <button
              key={`${ev.message_id}-${ev.seq}`}
              type="button"
              onClick={(e) => {
                e.stopPropagation()
                onSeek(i)
              }}
              onMouseEnter={() => setHoverIndex(i)}
              onMouseLeave={() => setHoverIndex((cur) => (cur === i ? null : cur))}
              className="absolute top-1/2 -translate-x-1/2 -translate-y-1/2 rounded-full transition-transform"
              style={{ left: `${left}%` }}
              aria-label={ev.summary}
              data-seq={ev.seq}
            >
              <span
                className={`block rounded-full ${isFailed ? 'ring-2 ring-red-500/70' : ''} ${
                  isCurrent ? 'w-3.5 h-3.5 ring-2 ring-indigo-500' : 'w-2 h-2 hover:w-2.5 hover:h-2.5'
                } ${accent.dot}`}
              />
            </button>
          )
        })}
      </div>

      {/* hover / current readout */}
      <div className="h-4 text-xs text-gray-500 truncate">
        {hovered ? (
          <span>
            <span className="font-medium text-gray-700 dark:text-gray-300">#{hovered.seq}</span> {hovered.summary}
          </span>
        ) : currentIndex >= 0 && events[currentIndex] ? (
          <span>
            step {currentIndex + 1} of {n} · #{events[currentIndex]!.seq} {events[currentIndex]!.summary}
          </span>
        ) : (
          <span>{n} event{n === 1 ? '' : 's'}</span>
        )}
      </div>
    </div>
  )
}
