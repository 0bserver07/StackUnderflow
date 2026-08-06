/**
 * Context Replay tab — the context-window analog of Playback (issue #96).
 *
 * Where Playback steps through a session's *tool calls*, this steps through the
 * *context the model was working from*. Pick a session, drag the scrubber to a
 * `seq` cutoff, and see the ordered message sequence that had accumulated as
 * the model's context up to that turn — each turn's role, a content preview,
 * its tool calls, and a running token total so you can watch the context grow.
 *
 *   ┌───────────────────────────────────────────────────────────────┐
 *   │ session picker · ◀ ▶ · turn N of M                             │
 *   ├───────────────────────────────────────────────────────────────┤
 *   │ ≈ 12,340 / 48,900 tokens (25%)   ▓▓▓▓▓░░░░░░░░░░░░░░░░░         │
 *   │ ░░░░░░░░░●░░░░░░░░░░░░░░░░  ← seq scrubber                      │
 *   ├───────────────────────────────────────────────────────────────┤
 *   │ #0  [user]       implement the feature            120 tok       │
 *   │ #1  [assistant]  [Edit a.py]                      260 tok       │
 *   │ …                                                              │
 *   └───────────────────────────────────────────────────────────────┘
 *
 * URL state: `?session=<id>&seq=<n>` — shareable, back-button friendly (reuses
 * the generic session+seq helpers the Playback tab already defines).
 *
 * The full timeline is fetched once per session (via GET /api/context-replay,
 * no `at`) and sliced client-side as you scrub — each event carries its own
 * prefix-sum `cumulative_tokens`, so the meter is a pure lookup. MVP semantics:
 * "the session's message sequence up to `seq`" (see services/context_replay.py;
 * harness-side context eviction is a later refinement). Read-only + advisory.
 */

import { useEffect, useMemo, useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import {
  IconStack2,
  IconChevronLeft,
  IconChevronRight,
  IconPlayerSkipBackFilled,
  IconPlayerSkipForwardFilled,
} from '@tabler/icons-react'

import { getContextReplay, getJsonlFiles, readPlaybackSelection } from '../../services/api'
import LoadingSpinner from '../common/LoadingSpinner'
import EmptyState from '../common/EmptyState'
import { formatTokens } from '../../services/format'
import type { ContextReplayEvent, JsonlFile } from '../../types/api'

function sessionIdFromFile(name: string): string {
  return name.endsWith('.jsonl') ? name.slice(0, -'.jsonl'.length) : name
}

function shortId(id: string, n = 12): string {
  return id.length > n ? `${id.slice(0, n)}…` : id
}

function sessionLabel(f: JsonlFile): string {
  const sid = sessionIdFromFile(f.name)
  const title = (f.title || '').trim()
  return title ? `${title.slice(0, 60)} — ${shortId(sid)}` : shortId(sid)
}

function roleClasses(role: string): string {
  if (role === 'user') {
    return 'border-emerald-400 dark:border-emerald-500 text-emerald-700 dark:text-emerald-300'
  }
  if (role === 'assistant') {
    return 'border-indigo-400 dark:border-indigo-500 text-indigo-700 dark:text-indigo-300'
  }
  return 'border-gray-300 dark:border-gray-600 text-gray-600 dark:text-gray-400'
}

interface ContextReplayTabProps {
  projectName: string
}

export default function ContextReplayTab({ projectName }: ContextReplayTabProps) {
  const initial = readPlaybackSelection(typeof window !== 'undefined' ? window.location.search : '')
  const [selectedSession, setSelectedSession] = useState<string | null>(initial.session)
  // `position` is a 0-based index into the event list (the scrubber value);
  // `null` means "not set yet" → default to the end (the full context).
  const [position, setPosition] = useState<number | null>(null)

  // ── data ──────────────────────────────────────────────────────────────

  const filesQuery = useQuery({
    queryKey: ['contextreplay', 'sessions', projectName],
    queryFn: () => getJsonlFiles(projectName),
  })

  const sessions: JsonlFile[] = useMemo(() => {
    const files = filesQuery.data?.files ?? []
    return files.slice().sort((a, b) => (b.modified ?? 0) - (a.modified ?? 0))
  }, [filesQuery.data])

  // Default to the most-recent session when none is in the URL (or the URL
  // points at one that no longer exists).
  useEffect(() => {
    if (sessions.length === 0) return
    const known = new Set(sessions.map((f) => sessionIdFromFile(f.name)))
    if (!selectedSession || !known.has(selectedSession)) {
      setSelectedSession(sessionIdFromFile(sessions[0]!.name))
      setPosition(null)
    }
  }, [sessions, selectedSession])

  const replayQuery = useQuery({
    queryKey: ['contextreplay', 'events', selectedSession],
    queryFn: () => getContextReplay(selectedSession!),
    enabled: !!selectedSession,
  })

  const events: ContextReplayEvent[] = replayQuery.data?.events ?? []
  const totalTokens = replayQuery.data?.total_tokens ?? 0
  const warnings = replayQuery.data?.warnings ?? []

  // Initialise the scrubber from the URL `?seq=` once events load.
  useEffect(() => {
    if (events.length === 0 || position !== null || initial.seq === null) return
    const idx = events.findIndex((e) => e.seq === initial.seq)
    if (idx >= 0) setPosition(idx)
  }, [events, position, initial.seq])

  // Resolve the effective scrubber index: default to the end (full context).
  const currentIndex = useMemo(() => {
    if (events.length === 0) return -1
    if (position === null) return events.length - 1
    return Math.min(events.length - 1, Math.max(0, position))
  }, [events.length, position])

  const currentEvent = currentIndex >= 0 ? events[currentIndex] ?? null : null
  const visibleEvents = currentIndex >= 0 ? events.slice(0, currentIndex + 1) : []
  const usedTokens = currentEvent?.cumulative_tokens ?? 0
  const pct = totalTokens > 0 ? Math.min(100, Math.round((usedTokens / totalTokens) * 100)) : 0

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
    if (target !== current) window.history.replaceState({}, '', target)
  }, [selectedSession, currentEvent])

  const seekTo = (i: number) => {
    if (events.length === 0) return
    setPosition(Math.min(events.length - 1, Math.max(0, i)))
  }

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
  if (sessions.length === 0) {
    return (
      <EmptyState
        icon={<IconStack2 size={40} />}
        title="No sessions yet in this project"
        description="Once a Claude Code session runs here, you'll be able to scrub through the context the model saw at each turn."
      />
    )
  }

  return (
    <div className="space-y-4" data-testid="context-replay-tab">
      {/* ── controls row ── */}
      <div className="flex flex-col lg:flex-row lg:items-center gap-3">
        <select
          value={selectedSession ?? ''}
          onChange={(e) => {
            setSelectedSession(e.target.value || null)
            setPosition(null)
          }}
          className="text-sm rounded-md border border-gray-300 dark:border-gray-700 bg-white dark:bg-gray-800 px-2 py-1.5 min-w-0 max-w-md flex-shrink"
          aria-label="Session to replay context for"
        >
          {sessions.map((f) => {
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
            disabled={events.length === 0}
            aria-label="Jump to first turn"
          >
            <IconPlayerSkipBackFilled size={14} />
          </button>
          <button
            type="button"
            onClick={() => seekTo(currentIndex - 1)}
            className="p-1.5 rounded border border-gray-300 dark:border-gray-700 hover:bg-gray-100 dark:hover:bg-gray-800 disabled:opacity-40"
            disabled={events.length === 0 || currentIndex <= 0}
            aria-label="Previous turn"
          >
            <IconChevronLeft size={14} />
          </button>
          <button
            type="button"
            onClick={() => seekTo(currentIndex + 1)}
            className="p-1.5 rounded border border-gray-300 dark:border-gray-700 hover:bg-gray-100 dark:hover:bg-gray-800 disabled:opacity-40"
            disabled={events.length === 0 || currentIndex >= events.length - 1}
            aria-label="Next turn"
          >
            <IconChevronRight size={14} />
          </button>
          <button
            type="button"
            onClick={() => seekTo(events.length - 1)}
            className="p-1.5 rounded border border-gray-300 dark:border-gray-700 hover:bg-gray-100 dark:hover:bg-gray-800 disabled:opacity-40"
            disabled={events.length === 0}
            aria-label="Jump to last turn (full context)"
          >
            <IconPlayerSkipForwardFilled size={14} />
          </button>
          {events.length > 0 && (
            <span className="text-xs text-gray-500 dark:text-gray-400 ml-2 tabular-nums">
              turn {currentIndex + 1} of {events.length}
            </span>
          )}
        </div>
      </div>

      {/* ── body ── */}
      {replayQuery.isLoading ? (
        <div className="h-14 flex items-center text-xs text-gray-500">Reconstructing context…</div>
      ) : replayQuery.error ? (
        <div className="p-3 text-sm text-red-600 dark:text-red-400">
          Failed to reconstruct context:{' '}
          {replayQuery.error instanceof Error ? replayQuery.error.message : 'Unknown error'}
        </div>
      ) : events.length === 0 ? (
        <EmptyState
          icon={<IconStack2 size={36} />}
          title="No messages in this session"
          description={
            warnings.length > 0
              ? warnings.join(' · ')
              : "This session has no recorded messages — pick another from the dropdown above."
          }
        />
      ) : (
        <>
          {/* ── token meter ── */}
          <div className="space-y-1.5">
            <div className="flex items-center justify-between text-xs">
              <span className="text-gray-600 dark:text-gray-300 tabular-nums">
                ≈ {formatTokens(usedTokens)} / {formatTokens(totalTokens)} context tokens
                <span className="text-gray-400 dark:text-gray-500"> ({pct}%)</span>
              </span>
              <span className="text-[11px] text-gray-400 dark:text-gray-500">
                estimate · chars/4 of each turn's text + tool payload
              </span>
            </div>
            <div className="h-2 w-full rounded-full bg-gray-200 dark:bg-gray-700 overflow-hidden">
              <div
                className="h-full rounded-full bg-indigo-500 dark:bg-indigo-400 transition-all"
                style={{ width: `${pct}%` }}
              />
            </div>
            {/* ── seq scrubber ── */}
            <input
              type="range"
              min={0}
              max={Math.max(0, events.length - 1)}
              value={currentIndex < 0 ? 0 : currentIndex}
              onChange={(e) => seekTo(Number(e.target.value))}
              className="w-full accent-indigo-500 dark:accent-indigo-400"
              aria-label="Context cutoff (seq)"
            />
          </div>

          {/* ── event list up to the cutoff ── */}
          <ul className="space-y-1.5" data-testid="context-replay-events">
            {visibleEvents.map((ev, i) => {
              const isCurrent = i === visibleEvents.length - 1
              return (
                <li
                  key={ev.seq}
                  className={`rounded-md border-l-2 pl-3 pr-2 py-1.5 ${roleClasses(ev.role)} ${
                    isCurrent
                      ? 'bg-indigo-50/70 dark:bg-indigo-900/20'
                      : 'bg-gray-50 dark:bg-gray-800/40'
                  }`}
                >
                  <div className="flex items-center justify-between gap-2 text-xs">
                    <span className="font-medium tabular-nums">
                      #{ev.seq} <span className="uppercase tracking-wide">{ev.role}</span>
                    </span>
                    <span className="text-gray-500 dark:text-gray-400 tabular-nums">
                      {formatTokens(ev.cumulative_tokens)} tok
                    </span>
                  </div>
                  {ev.tool_calls.length > 0 && (
                    <div className="mt-0.5 text-[11px] text-gray-500 dark:text-gray-400 truncate">
                      tools: {ev.tool_calls.join(', ')}
                    </div>
                  )}
                  {ev.content_preview && (
                    <div className="mt-0.5 text-xs text-gray-700 dark:text-gray-300 line-clamp-2 whitespace-pre-wrap break-words">
                      {ev.content_preview}
                    </div>
                  )}
                </li>
              )
            })}
          </ul>
        </>
      )}
    </div>
  )
}
