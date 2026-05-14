/**
 * Playback tab — a scrubbable, step-through view of a session's tool calls.
 *
 * Instead of reading a flat message list, you move forward/backward through
 * just the *state-changing events* (each Read / Edit / Bash / Task ...). The
 * layout:
 *
 *   ┌───────────────────────────────────────────────────────────────┐
 *   │ session picker · filter chips · ◀ ▶ play/pause · speed         │
 *   ├───────────────────────────────────────────────────────────────┤
 *   │ ░░░░░░░░░●░░░░░░░░░░░░░░░░░  ← horizontal scrubber (ticks)     │
 *   ├──────────────────────────────┬────────────────────────────────┤
 *   │ event list (click to jump)   │ current-event detail panel     │
 *   └──────────────────────────────┴────────────────────────────────┘
 *
 * Keyboard: `j`/→ next · `k`/← prev · `space` play/pause · `1`/`2`/`3` speed
 * · `f` focus the filter chips · `Home`/`End` jump to ends.
 *
 * URL state: `?session=<id>&seq=<n>` — shareable and back-button friendly.
 *
 * Spec: .notes/specs/10-playback-timeline.md (this is v1: the event stream;
 * v2 — virtual-filesystem reconstruction — is a separate spec later).
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import {
  IconHistory,
  IconPlayerPauseFilled,
  IconPlayerPlayFilled,
  IconPlayerSkipBackFilled,
  IconPlayerSkipForwardFilled,
  IconChevronLeft,
  IconChevronRight,
} from '@tabler/icons-react'

import { getJsonlFiles, getPlayback, readPlaybackSelection, writePlaybackSelection } from '../../services/api'
import LoadingSpinner from '../common/LoadingSpinner'
import EmptyState from '../common/EmptyState'
import PlaybackScrubber from './PlaybackScrubber'
import PlaybackEventList from './PlaybackEventList'
import PlaybackEventDetail from './PlaybackEventDetail'
import { FILTER_CHIP_TOOLS } from './playbackColors'
import type { JsonlFile } from '../../types/api'

// Speed levels mapped to keyboard keys 1/2/3. The auto-advance interval is
// `BASE_STEP_MS / multiplier`.
const SPEEDS = [
  { key: '1', label: '1×', mult: 1 },
  { key: '2', label: '2×', mult: 2 },
  { key: '3', label: '4×', mult: 4 },
] as const
const BASE_STEP_MS = 900

function sessionIdFromFile(name: string): string {
  return name.endsWith('.jsonl') ? name.slice(0, -'.jsonl'.length) : name
}

function shortId(id: string, n = 8): string {
  return id.length > n ? `${id.slice(0, n)}…` : id
}

function sessionLabel(f: JsonlFile): string {
  const sid = sessionIdFromFile(f.name)
  const title = (f.title || '').trim()
  const calls = f.tool_calls ?? 0
  const head = title ? title.slice(0, 60) : shortId(sid, 12)
  return `${head} — ${calls} tool call${calls === 1 ? '' : 's'}`
}

interface PlaybackTabProps {
  projectName: string
}

export default function PlaybackTab({ projectName }: PlaybackTabProps) {
  const initial = readPlaybackSelection(typeof window !== 'undefined' ? window.location.search : '')
  const [selectedSession, setSelectedSession] = useState<string | null>(initial.session)
  // `selectedSeq` is the *seq value* of the focused event (a global index into
  // the unfiltered stream), not its position in the currently-filtered list.
  const [selectedSeq, setSelectedSeq] = useState<number | null>(initial.seq)
  const [toolFilter, setToolFilter] = useState<Set<string>>(new Set())
  const [playing, setPlaying] = useState(false)
  const [speedMult, setSpeedMult] = useState(1)

  const containerRef = useRef<HTMLDivElement | null>(null)
  const firstChipRef = useRef<HTMLButtonElement | null>(null)

  // ── data ──────────────────────────────────────────────────────────────

  const filesQuery = useQuery({
    queryKey: ['playback', 'sessions', projectName],
    queryFn: () => getJsonlFiles(projectName),
  })

  // Playback-eligible = has at least one tool call. Most recent first.
  const eligibleSessions: JsonlFile[] = useMemo(() => {
    const files = filesQuery.data?.files ?? []
    return files
      .filter((f) => (f.tool_calls ?? 0) > 0)
      .slice()
      .sort((a, b) => (b.modified ?? 0) - (a.modified ?? 0))
  }, [filesQuery.data])

  // Default to the most-recent eligible session when none is in the URL (or
  // the URL points at one that no longer exists / has no tool calls).
  useEffect(() => {
    if (eligibleSessions.length === 0) return
    const known = new Set(eligibleSessions.map((f) => sessionIdFromFile(f.name)))
    if (!selectedSession || !known.has(selectedSession)) {
      setSelectedSession(sessionIdFromFile(eligibleSessions[0]!.name))
      setSelectedSeq(null)
    }
  }, [eligibleSessions, selectedSession])

  const playbackQuery = useQuery({
    queryKey: ['playback', 'events', selectedSession],
    queryFn: () => getPlayback(selectedSession!, { includePayload: true }),
    enabled: !!selectedSession,
  })

  const allEvents = playbackQuery.data?.events ?? []
  const truncated = playbackQuery.data?.truncated ?? false

  const filteredEvents = useMemo(() => {
    if (toolFilter.size === 0) return allEvents
    return allEvents.filter((e) => toolFilter.has(e.tool_name))
  }, [allEvents, toolFilter])

  // Resolve the current position (index into `filteredEvents`) from the seq in
  // state. If that seq isn't in the filtered view, fall back to the start.
  const currentIndex = useMemo(() => {
    if (filteredEvents.length === 0) return -1
    if (selectedSeq === null) return 0
    const idx = filteredEvents.findIndex((e) => e.seq === selectedSeq)
    return idx >= 0 ? idx : 0
  }, [filteredEvents, selectedSeq])

  const currentEvent = currentIndex >= 0 ? filteredEvents[currentIndex] ?? null : null

  // ── seek / navigation ─────────────────────────────────────────────────

  const seekTo = useCallback(
    (index: number) => {
      if (filteredEvents.length === 0) return
      const clamped = Math.min(filteredEvents.length - 1, Math.max(0, index))
      setSelectedSeq(filteredEvents[clamped]!.seq)
    },
    [filteredEvents],
  )

  const step = useCallback(
    (delta: number) => {
      if (filteredEvents.length === 0) return
      const base = currentIndex < 0 ? 0 : currentIndex
      seekTo(base + delta)
    },
    [filteredEvents.length, currentIndex, seekTo],
  )

  // ── auto-play timer ───────────────────────────────────────────────────

  useEffect(() => {
    if (!playing) return
    if (filteredEvents.length === 0) {
      setPlaying(false)
      return
    }
    const interval = Math.max(120, Math.round(BASE_STEP_MS / speedMult))
    const id = window.setInterval(() => {
      setSelectedSeq((seq) => {
        const idx = seq === null ? 0 : filteredEvents.findIndex((e) => e.seq === seq)
        const cur = idx < 0 ? 0 : idx
        if (cur >= filteredEvents.length - 1) {
          // Reached the end — stop on the next tick.
          window.setTimeout(() => setPlaying(false), 0)
          return filteredEvents[filteredEvents.length - 1]!.seq
        }
        return filteredEvents[cur + 1]!.seq
      })
    }, interval)
    return () => window.clearInterval(id)
  }, [playing, speedMult, filteredEvents])

  // Stop playback whenever the session or the filter set changes.
  useEffect(() => {
    setPlaying(false)
  }, [selectedSession, toolFilter])

  // ── URL sync (replaceState — the tab is one history entry) ─────────────

  useEffect(() => {
    if (typeof window === 'undefined') return
    const url = new URL(window.location.href)
    const merged = new URLSearchParams(url.search)
    if (selectedSession) merged.set('session', selectedSession)
    else merged.delete('session')
    if (currentEvent) merged.set('seq', String(currentEvent.seq))
    else merged.delete('seq')
    const mergedSearch = merged.toString()
    const target = `${url.pathname}${mergedSearch ? `?${mergedSearch}` : ''}${url.hash}`
    const current = `${window.location.pathname}${window.location.search}${window.location.hash}`
    if (target !== current) {
      window.history.replaceState({}, '', target)
    }
    // keep the URL-state helper referenced (lint) — its symmetric reader is
    // used on mount.
    void writePlaybackSelection
  }, [selectedSession, currentEvent])

  // ── keyboard ──────────────────────────────────────────────────────────

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLDivElement>) => {
      // Don't hijack typing in the session picker.
      const tag = (e.target as HTMLElement)?.tagName
      if (tag === 'SELECT' || tag === 'INPUT' || tag === 'TEXTAREA') return
      if (e.key === 'j' || e.key === 'ArrowRight') {
        e.preventDefault()
        step(1)
      } else if (e.key === 'k' || e.key === 'ArrowLeft') {
        e.preventDefault()
        step(-1)
      } else if (e.key === ' ' || e.key === 'Spacebar') {
        e.preventDefault()
        setPlaying((p) => !p)
      } else if (e.key === 'Home') {
        e.preventDefault()
        seekTo(0)
      } else if (e.key === 'End') {
        e.preventDefault()
        seekTo(filteredEvents.length - 1)
      } else if (e.key === 'f') {
        e.preventDefault()
        firstChipRef.current?.focus()
      } else {
        const sp = SPEEDS.find((s) => s.key === e.key)
        if (sp) {
          e.preventDefault()
          setSpeedMult(sp.mult)
        }
      }
    },
    [step, seekTo, filteredEvents.length],
  )

  // ── filter chips ──────────────────────────────────────────────────────

  const toggleTool = (tool: string) => {
    setToolFilter((prev) => {
      const next = new Set(prev)
      if (next.has(tool)) next.delete(tool)
      else next.add(tool)
      return next
    })
  }

  // Which chip tools actually occur in this session (so we can dim the rest).
  const presentTools = useMemo(() => new Set(allEvents.map((e) => e.tool_name)), [allEvents])

  // ── render ────────────────────────────────────────────────────────────

  if (filesQuery.isLoading) return <LoadingSpinner message="Loading sessions..." />
  if (filesQuery.error) {
    return (
      <div className="p-4 text-sm text-red-600 dark:text-red-400">
        Failed to load sessions:{' '}
        {filesQuery.error instanceof Error ? filesQuery.error.message : 'Unknown error'}
      </div>
    )
  }
  if (eligibleSessions.length === 0) {
    return (
      <EmptyState
        icon={<IconHistory size={40} />}
        title="No tool calls yet in this project"
        description="Once a Claude Code session runs some tools (Read / Edit / Bash …), you'll be able to scrub through them here step by step."
      />
    )
  }

  return (
    <div
      ref={containerRef}
      onKeyDown={handleKeyDown}
      tabIndex={0}
      className="space-y-4 outline-none"
      data-testid="playback-tab"
    >
      {/* ── controls row ── */}
      <div className="flex flex-col lg:flex-row lg:items-center gap-3">
        <select
          value={selectedSession ?? ''}
          onChange={(e) => {
            setSelectedSession(e.target.value || null)
            setSelectedSeq(null)
          }}
          className="text-sm rounded-md border border-gray-300 dark:border-gray-700 bg-white dark:bg-gray-800 px-2 py-1.5 min-w-0 max-w-md flex-shrink"
          aria-label="Session to play back"
        >
          {eligibleSessions.map((f) => {
            const sid = sessionIdFromFile(f.name)
            return (
              <option key={sid} value={sid}>
                {sessionLabel(f)}
              </option>
            )
          })}
        </select>

        {/* transport */}
        <div className="flex items-center gap-1">
          <button
            type="button"
            onClick={() => seekTo(0)}
            className="p-1.5 rounded border border-gray-300 dark:border-gray-700 hover:bg-gray-100 dark:hover:bg-gray-800 disabled:opacity-40"
            disabled={filteredEvents.length === 0}
            aria-label="Jump to start"
          >
            <IconPlayerSkipBackFilled size={14} />
          </button>
          <button
            type="button"
            onClick={() => step(-1)}
            className="p-1.5 rounded border border-gray-300 dark:border-gray-700 hover:bg-gray-100 dark:hover:bg-gray-800 disabled:opacity-40"
            disabled={filteredEvents.length === 0}
            aria-label="Previous event (k)"
          >
            <IconChevronLeft size={14} />
          </button>
          <button
            type="button"
            onClick={() => setPlaying((p) => !p)}
            className="p-1.5 rounded border border-indigo-300 dark:border-indigo-700 text-indigo-600 dark:text-indigo-300 hover:bg-indigo-50 dark:hover:bg-indigo-900/30 disabled:opacity-40"
            disabled={filteredEvents.length === 0}
            aria-label={playing ? 'Pause (space)' : 'Play (space)'}
          >
            {playing ? <IconPlayerPauseFilled size={14} /> : <IconPlayerPlayFilled size={14} />}
          </button>
          <button
            type="button"
            onClick={() => step(1)}
            className="p-1.5 rounded border border-gray-300 dark:border-gray-700 hover:bg-gray-100 dark:hover:bg-gray-800 disabled:opacity-40"
            disabled={filteredEvents.length === 0}
            aria-label="Next event (j)"
          >
            <IconChevronRight size={14} />
          </button>
          <button
            type="button"
            onClick={() => seekTo(filteredEvents.length - 1)}
            className="p-1.5 rounded border border-gray-300 dark:border-gray-700 hover:bg-gray-100 dark:hover:bg-gray-800 disabled:opacity-40"
            disabled={filteredEvents.length === 0}
            aria-label="Jump to end"
          >
            <IconPlayerSkipForwardFilled size={14} />
          </button>
        </div>

        {/* speed */}
        <div className="flex items-center gap-1" role="radiogroup" aria-label="Playback speed">
          {SPEEDS.map((s) => (
            <button
              key={s.key}
              type="button"
              role="radio"
              aria-checked={speedMult === s.mult}
              onClick={() => setSpeedMult(s.mult)}
              className={`text-xs px-2 py-1 rounded border ${
                speedMult === s.mult
                  ? 'border-indigo-400 bg-indigo-50 text-indigo-700 dark:bg-indigo-900/30 dark:text-indigo-300'
                  : 'border-gray-300 dark:border-gray-700 text-gray-600 dark:text-gray-400 hover:bg-gray-100 dark:hover:bg-gray-800'
              }`}
            >
              {s.label}
            </button>
          ))}
        </div>
      </div>

      {/* ── filter chips ── */}
      <div className="flex flex-wrap items-center gap-1.5" data-testid="playback-filter-chips">
        <button
          ref={firstChipRef}
          type="button"
          onClick={() => setToolFilter(new Set())}
          className={`text-xs px-2.5 py-1 rounded-full border ${
            toolFilter.size === 0
              ? 'border-indigo-400 bg-indigo-50 text-indigo-700 dark:bg-indigo-900/30 dark:text-indigo-300'
              : 'border-gray-300 dark:border-gray-700 text-gray-600 dark:text-gray-400 hover:bg-gray-100 dark:hover:bg-gray-800'
          }`}
        >
          All
        </button>
        {FILTER_CHIP_TOOLS.map((tool) => {
          const active = toolFilter.has(tool)
          const present = presentTools.has(tool)
          return (
            <button
              key={tool}
              type="button"
              onClick={() => toggleTool(tool)}
              className={`text-xs px-2.5 py-1 rounded-full border transition-colors ${
                active
                  ? 'border-indigo-400 bg-indigo-50 text-indigo-700 dark:bg-indigo-900/30 dark:text-indigo-300'
                  : present
                    ? 'border-gray-300 dark:border-gray-700 text-gray-600 dark:text-gray-400 hover:bg-gray-100 dark:hover:bg-gray-800'
                    : 'border-gray-200 dark:border-gray-800 text-gray-400 dark:text-gray-600'
              }`}
              title={present ? `Toggle ${tool}` : `No ${tool} calls in this session`}
            >
              {tool}
            </button>
          )
        })}
        {truncated && (
          <span className="text-[11px] text-amber-700 dark:text-amber-400 ml-1">
            (showing the first {allEvents.length} events — long session)
          </span>
        )}
        <span className="text-[11px] text-gray-400 ml-auto hidden sm:inline">
          j/k step · space play · f filter · 1–3 speed
        </span>
      </div>

      {/* ── scrubber ── */}
      {playbackQuery.isLoading ? (
        <div className="h-14 flex items-center text-xs text-gray-500">Loading events…</div>
      ) : playbackQuery.error ? (
        <div className="p-3 text-sm text-red-600 dark:text-red-400">
          Failed to load playback:{' '}
          {playbackQuery.error instanceof Error ? playbackQuery.error.message : 'Unknown error'}
        </div>
      ) : allEvents.length === 0 ? (
        <EmptyState
          icon={<IconHistory size={36} />}
          title="No tool calls in this session"
          description="This session didn't run any tools — pick another from the dropdown above."
        />
      ) : (
        <>
          <PlaybackScrubber events={filteredEvents} currentIndex={currentIndex} onSeek={seekTo} />

          {/* ── list + detail ── */}
          <div className="grid grid-cols-1 lg:grid-cols-12 gap-4">
            <div className="lg:col-span-5">
              <PlaybackEventList events={filteredEvents} currentIndex={currentIndex} onSelect={seekTo} />
            </div>
            <div className="lg:col-span-7">
              <PlaybackEventDetail event={currentEvent} />
            </div>
          </div>
        </>
      )}
    </div>
  )
}
