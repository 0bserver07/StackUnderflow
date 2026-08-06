/**
 * PlaybackFsPanel — the Playback v2 file-browser-at-time-T side panel.
 *
 * Reconstructs the state of every file the session touched up to the current
 * scrubber timestamp and renders it as a two-column browser:
 *
 *   ┌─ Header: snapshot ts · collapse button ────────────────────────┐
 *   │  ⚠ warnings (if any) ────────────────────────────────────────  │
 *   ├──────────────────────────────┬─────────────────────────────────┤
 *   │ file list                    │ file content                    │
 *   │ (grouped by dir)             │ (monospace + line numbers)      │
 *   └──────────────────────────────┴─────────────────────────────────┘
 *
 * Time integration: the parent (PlaybackTab) hands us `at` — the ISO
 * timestamp of the currently-scrubbed event. We debounce 250ms so a rapid
 * scrub doesn't fire dozens of requests.
 *
 * Bandwidth optimisation: while no file is selected we fetch the snapshot
 * with `include_content=false` (metadata only). When the user clicks a file
 * we issue a follow-up fetch scoped to that one path with
 * `include_content=true`.
 *
 * Spec: stackunderflow/services/playback_fs.py.
 */

import { useEffect, useMemo, useRef, useState } from 'react'
import {
  IconAlertTriangle,
  IconFolder,
  IconFile,
  IconChevronRight,
  IconChevronDown,
  IconLayoutSidebarRightCollapse,
  IconLayoutSidebarRightExpand,
} from '@tabler/icons-react'

import { getPlaybackFsSnapshot, PlaybackFsBadTimestampError } from '../../services/api'
import type { PlaybackFsFileEntry, PlaybackFsSnapshotResponse } from '../../types/api'
import { formatSnapshotTs, groupFilesByDirectory, humanizeBytes } from './playbackFs'

const SCRUB_DEBOUNCE_MS = 250

interface PlaybackFsPanelProps {
  sessionId: string
  /**
   * The current scrubber timestamp (ISO-8601). `null` means there's no event
   * to anchor on (e.g. session loaded but no event selected) — the panel
   * renders its empty / "pick an event" hint in that case.
   */
  at: string | null
  /** Collapse / expand state, controlled by the parent so the toggle button can live in the tab header too. */
  open: boolean
  onToggle: () => void
}

export default function PlaybackFsPanel({ sessionId, at, open, onToggle }: PlaybackFsPanelProps) {
  // ── snapshot state ─────────────────────────────────────────────────────
  const [snapshot, setSnapshot] = useState<PlaybackFsSnapshotResponse | null>(null)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [selectedPath, setSelectedPath] = useState<string | null>(null)
  // We cache the full-content fetch for the active file separately. The
  // metadata snapshot uses include_content=false to keep payload light while
  // scrubbing; this slot fills in once the user picks a file.
  const [contentFile, setContentFile] = useState<{ path: string; entry: PlaybackFsFileEntry } | null>(null)
  const [contentLoading, setContentLoading] = useState(false)
  const [expanded, setExpanded] = useState<Record<string, boolean>>({})

  // Track the most recent fetch so a stale response doesn't overwrite a fresh
  // one when the user scrubs through a few moments rapidly. Strict equality
  // on `at` and `sessionId` is enough — we don't need an AbortController.
  const requestKeyRef = useRef<string>('')

  // ── metadata-only fetch on scrub ───────────────────────────────────────
  useEffect(() => {
    if (!open) return
    if (!sessionId || !at) {
      setSnapshot(null)
      setError(null)
      return
    }
    const key = `${sessionId}|${at}`
    requestKeyRef.current = key
    setLoading(true)
    setError(null)
    const timer = setTimeout(() => {
      getPlaybackFsSnapshot(sessionId, { at, includeContent: false })
        .then((data) => {
          // Drop the response if a newer request superseded us mid-flight.
          if (requestKeyRef.current !== key) return
          setSnapshot(data)
          setLoading(false)
        })
        .catch((err) => {
          if (requestKeyRef.current !== key) return
          if (err instanceof PlaybackFsBadTimestampError) {
            setError('Could not parse this timestamp.')
          } else {
            setError(err instanceof Error ? err.message : String(err))
          }
          setSnapshot(null)
          setLoading(false)
        })
    }, SCRUB_DEBOUNCE_MS)
    return () => clearTimeout(timer)
  }, [sessionId, at, open])

  // ── file-content fetch on selection ────────────────────────────────────
  useEffect(() => {
    if (!open || !sessionId || !at || !selectedPath) {
      setContentFile(null)
      return
    }
    setContentLoading(true)
    const key = `${sessionId}|${at}|${selectedPath}`
    let cancelled = false
    getPlaybackFsSnapshot(sessionId, {
      at,
      paths: [selectedPath],
      includeContent: true,
    })
      .then((data) => {
        if (cancelled) return
        const entry = data.files[selectedPath]
        if (entry) {
          setContentFile({ path: selectedPath, entry })
        } else {
          // File vanished from the snapshot (race with a scrub past its last
          // touch) — leave the row visible but show an empty viewer.
          setContentFile({ path: selectedPath, entry: { byte_count: 0, last_modified_ts: null, operations_applied: [], reconstruction_complete: true } })
        }
        setContentLoading(false)
      })
      .catch(() => {
        if (cancelled) return
        setContentLoading(false)
      })
    return () => {
      cancelled = true
      void key
    }
  }, [sessionId, at, selectedPath, open])

  // Reset the selection when the session changes underneath us — the path
  // is meaningless across sessions.
  useEffect(() => {
    setSelectedPath(null)
    setContentFile(null)
  }, [sessionId])

  // ── derived data ──────────────────────────────────────────────────────
  const groups = useMemo(() => {
    if (!snapshot) return []
    return groupFilesByDirectory(snapshot.files)
  }, [snapshot])

  // Auto-expand directories on the first snapshot so the user doesn't have
  // to click into them to see anything. Subsequent snapshots respect the
  // user's manual toggles.
  useEffect(() => {
    if (groups.length === 0) return
    setExpanded((prev) => {
      const next = { ...prev }
      let changed = false
      for (const g of groups) {
        if (!(g.dir in next)) {
          next[g.dir] = true
          changed = true
        }
      }
      return changed ? next : prev
    })
  }, [groups])

  const fileCount = snapshot ? Object.keys(snapshot.files).length : 0
  const warnings = snapshot?.warnings ?? []

  // ── collapsed pill ────────────────────────────────────────────────────
  if (!open) {
    return (
      <div className="rounded-md border border-gray-200 dark:border-gray-800 p-2 flex items-center justify-between">
        <span className="text-xs text-gray-500">File browser collapsed</span>
        <button
          type="button"
          onClick={onToggle}
          className="text-xs flex items-center gap-1 px-2 py-1 rounded hover:bg-gray-100 dark:hover:bg-gray-800"
          aria-label="Expand file browser"
        >
          <IconLayoutSidebarRightExpand size={14} /> Open
        </button>
      </div>
    )
  }

  // ── expanded panel ────────────────────────────────────────────────────
  return (
    <div
      className="rounded-md border border-gray-200 dark:border-gray-800 bg-white dark:bg-gray-900"
      data-testid="playback-fs-panel"
    >
      {/* Header */}
      <header className="px-3 py-2 border-b border-gray-200 dark:border-gray-800 flex items-center justify-between gap-2">
        <div className="min-w-0">
          <div className="text-[11px] uppercase tracking-wider text-gray-500">
            File browser at this moment
          </div>
          <div className="text-sm font-medium text-gray-800 dark:text-gray-200 truncate">
            {formatSnapshotTs(at)}
            {snapshot && (
              <span className="ml-2 text-xs text-gray-500">
                · {fileCount} file{fileCount === 1 ? '' : 's'}
              </span>
            )}
          </div>
        </div>
        <button
          type="button"
          onClick={onToggle}
          className="text-xs flex items-center gap-1 px-2 py-1 rounded hover:bg-gray-100 dark:hover:bg-gray-800 flex-shrink-0"
          aria-label="Collapse file browser"
        >
          <IconLayoutSidebarRightCollapse size={14} /> Hide
        </button>
      </header>

      {/* Warnings banner */}
      {warnings.length > 0 && (
        <div
          className="px-3 py-2 border-b border-amber-200 dark:border-amber-900/40 bg-amber-50 dark:bg-amber-900/20 text-[12px] text-amber-800 dark:text-amber-300 space-y-0.5"
          data-testid="playback-fs-warnings"
        >
          {warnings.map((w, i) => (
            <div key={i} className="flex items-start gap-1.5">
              <IconAlertTriangle size={12} className="mt-0.5 flex-shrink-0" />
              <span className="break-words">{w}</span>
            </div>
          ))}
        </div>
      )}

      {/* Body */}
      <div className="grid grid-cols-1 lg:grid-cols-12">
        {/* Left: file list */}
        <div className="lg:col-span-5 border-b lg:border-b-0 lg:border-r border-gray-200 dark:border-gray-800 max-h-[28rem] overflow-y-auto">
          {!at ? (
            <div className="p-4 text-xs text-gray-500">
              Pick an event on the timeline to see which files existed at that moment.
            </div>
          ) : loading && !snapshot ? (
            <div className="p-4 text-xs text-gray-500">Loading file list…</div>
          ) : error ? (
            <div className="p-4 text-xs text-red-600 dark:text-red-400">{error}</div>
          ) : fileCount === 0 ? (
            <div className="p-4 text-xs text-gray-500" data-testid="playback-fs-empty">
              No file operations in this session before {formatSnapshotTs(at)}.
            </div>
          ) : (
            <FileTree
              groups={groups}
              expanded={expanded}
              onToggleDir={(dir) =>
                setExpanded((prev) => ({ ...prev, [dir]: !(prev[dir] ?? true) }))
              }
              selectedPath={selectedPath}
              onSelect={(p) => setSelectedPath(p)}
            />
          )}
        </div>

        {/* Right: file content */}
        <div className="lg:col-span-7">
          <FileViewer
            file={contentFile}
            loading={contentLoading}
            selectedPath={selectedPath}
          />
        </div>
      </div>
    </div>
  )
}

// ---------------------------------------------------------------------------
// FileTree — flat-list-grouped-by-directory left column. Collapsible per
// directory; click a file to select it.
// ---------------------------------------------------------------------------

interface FileTreeProps {
  groups: ReturnType<typeof groupFilesByDirectory>
  expanded: Record<string, boolean>
  onToggleDir: (dir: string) => void
  selectedPath: string | null
  onSelect: (path: string) => void
}

function FileTree({ groups, expanded, onToggleDir, selectedPath, onSelect }: FileTreeProps) {
  return (
    <ul className="text-xs" role="tree" aria-label="Files at this moment">
      {groups.map((g) => {
        const open = expanded[g.dir] ?? true
        const isRoot = g.dir === ''
        return (
          <li key={g.dir || '(root)'} role="treeitem" aria-expanded={open}>
            {!isRoot && (
              <button
                type="button"
                onClick={() => onToggleDir(g.dir)}
                className="w-full flex items-center gap-1 px-2 py-1 text-left text-gray-600 dark:text-gray-400 hover:bg-gray-50 dark:hover:bg-gray-800/40"
                aria-label={`${open ? 'Collapse' : 'Expand'} ${g.dir}`}
              >
                {open ? <IconChevronDown size={12} /> : <IconChevronRight size={12} />}
                <IconFolder size={12} className="text-gray-400" />
                <span className="truncate font-mono">{g.dir}</span>
                <span className="ml-auto text-[10px] text-gray-400 flex-shrink-0">
                  {g.files.length}
                </span>
              </button>
            )}
            {open && (
              <ul role="group">
                {g.files.map((f) => {
                  const isSelected = f.path === selectedPath
                  const incomplete = !f.entry.reconstruction_complete
                  const risk = f.entry.risk
                  const showRiskBadge = !!risk && risk.reverted_count > 0
                  const riskTooltip = risk
                    ? `${risk.reverted_count} reverted, ${risk.failed_count} failed, ${risk.worked_count} worked over ${risk.total_sessions} sessions`
                    : ''
                  return (
                    <li key={f.path} role="treeitem" aria-selected={isSelected}>
                      <button
                        type="button"
                        onClick={() => onSelect(f.path)}
                        className={`w-full text-left flex items-center gap-1.5 px-2 py-1 ${
                          isRoot ? '' : 'pl-6'
                        } ${
                          isSelected
                            ? 'bg-indigo-50 dark:bg-indigo-900/20 text-indigo-700 dark:text-indigo-300'
                            : 'hover:bg-gray-50 dark:hover:bg-gray-800/40 text-gray-700 dark:text-gray-300'
                        }`}
                        data-testid="playback-fs-file-row"
                        data-path={f.path}
                      >
                        <IconFile size={12} className="flex-shrink-0 text-gray-400" />
                        <span className="font-mono truncate flex-1" title={f.path}>
                          {f.basename}
                        </span>
                        {showRiskBadge && (
                          <span
                            className="text-[10px] font-semibold tabular-nums px-1 rounded bg-rose-100 dark:bg-rose-900/40 text-rose-700 dark:text-rose-300 flex-shrink-0"
                            data-testid="playback-fs-risk-badge"
                            title={riskTooltip}
                            aria-label={`Risk: ${riskTooltip}`}
                          >
                            {risk!.reverted_count}↩
                          </span>
                        )}
                        {incomplete && (
                          <IconAlertTriangle
                            size={11}
                            className="text-amber-500 flex-shrink-0"
                            aria-label="reconstruction incomplete"
                          />
                        )}
                        <span className="text-[10px] tabular-nums text-gray-400 flex-shrink-0">
                          {humanizeBytes(f.entry.byte_count)}
                        </span>
                        <span className="text-[10px] tabular-nums text-gray-400 flex-shrink-0 hidden sm:inline">
                          {f.entry.operations_applied.length} op
                          {f.entry.operations_applied.length === 1 ? '' : 's'}
                        </span>
                      </button>
                    </li>
                  )
                })}
              </ul>
            )}
          </li>
        )
      })}
    </ul>
  )
}

// ---------------------------------------------------------------------------
// FileViewer — right pane. Monospace + line numbers, no syntax highlighting
// (would need a per-file language guess + a heavier dep budget).
// ---------------------------------------------------------------------------

interface FileViewerProps {
  file: { path: string; entry: PlaybackFsFileEntry } | null
  loading: boolean
  selectedPath: string | null
}

function FileViewer({ file, loading, selectedPath }: FileViewerProps) {
  if (!selectedPath) {
    return (
      <div className="p-6 text-center text-xs text-gray-500" data-testid="playback-fs-viewer-empty">
        Pick a file on the left to view its reconstructed contents.
      </div>
    )
  }
  if (loading && !file) {
    return (
      <div className="p-6 text-center text-xs text-gray-500">Loading file…</div>
    )
  }
  if (!file) {
    return (
      <div className="p-6 text-center text-xs text-gray-500">No content available.</div>
    )
  }
  const content = file.entry.content ?? ''
  const lines = content.length === 0 ? [''] : content.split('\n')
  return (
    <div data-testid="playback-fs-viewer">
      <div className="px-3 py-1.5 border-b border-gray-200 dark:border-gray-800 text-xs text-gray-600 dark:text-gray-400 flex items-center justify-between gap-2">
        <span className="font-mono truncate" title={file.path}>{file.path}</span>
        <span className="text-[11px] text-gray-500 flex-shrink-0">
          {humanizeBytes(file.entry.byte_count)}
          {file.entry.operations_applied.length > 0 && (
            <> · {file.entry.operations_applied.join(', ')}</>
          )}
        </span>
      </div>
      {!file.entry.reconstruction_complete && (
        <div className="px-3 py-1.5 text-[11px] text-amber-700 dark:text-amber-400 bg-amber-50/60 dark:bg-amber-900/10 border-b border-amber-200 dark:border-amber-900/30 flex items-center gap-1.5">
          <IconAlertTriangle size={11} />
          Reconstruction incomplete — no Read/Write seeded the file body.
        </div>
      )}
      <pre className="text-[12px] font-mono leading-relaxed text-gray-800 dark:text-gray-200 max-h-[28rem] overflow-auto p-3">
        <code>
          {lines.map((ln, i) => (
            <div key={i} className="flex">
              <span className="select-none w-10 text-right pr-3 text-gray-400 flex-shrink-0 tabular-nums">
                {i + 1}
              </span>
              <span className="whitespace-pre-wrap break-words flex-1">{ln || ' '}</span>
            </div>
          ))}
        </code>
      </pre>
    </div>
  )
}
