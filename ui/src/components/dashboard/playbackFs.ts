/**
 * Pure helpers for the Playback v2 side panel (file-browser-at-time-T).
 *
 * Lives separately from the component so the test suite (no DOM runner) can
 * unit-test the formatting + tree-building logic without rendering React.
 *
 * Spec: python-legacy: services/playback_fs.py (backend contract).
 */

import type { PlaybackFsFileEntry } from '../../types/api'

// ---------------------------------------------------------------------------
// Byte-count humaniser. Same shape as PlaybackEventDetail's fmtBytes, kept
// here so the panel doesn't reach across component boundaries.
// ---------------------------------------------------------------------------

export function humanizeBytes(n: number | null | undefined): string {
  if (n === null || n === undefined || Number.isNaN(n)) return '—'
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`
  return `${(n / (1024 * 1024)).toFixed(1)} MB`
}

// ---------------------------------------------------------------------------
// Compact ISO-8601 formatter used by the panel header. Falls back to the raw
// string when parsing fails so a malformed timestamp doesn't crash the panel.
// ---------------------------------------------------------------------------

export function formatSnapshotTs(iso: string | null): string {
  if (!iso) return '—'
  try {
    const d = new Date(iso)
    if (Number.isNaN(d.getTime())) return iso
    return d.toLocaleString(undefined, {
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

// ---------------------------------------------------------------------------
// File-tree shape. We render a *flat-list grouped by directory* layout:
// directories collapsed by default but expanded eagerly when there are few
// entries. Each node carries its file metadata directly so the row renderer
// doesn't have to look anything up.
// ---------------------------------------------------------------------------

export interface FileNode {
  /** Final path segment (the file's basename). */
  basename: string
  /** Full path relative to the session cwd (key in the `files` map). */
  path: string
  /** The metadata entry exactly as the route returned it. */
  entry: PlaybackFsFileEntry
}

export interface DirectoryGroup {
  /** Directory path relative to cwd; empty string for files in the root. */
  dir: string
  files: FileNode[]
}

/**
 * Group files by their parent directory. Returns an array of {dir, files}
 * sorted alphabetically by directory then by basename. Root-level files
 * are grouped under `dir: ""` and sorted first.
 *
 * This is the layout the side panel renders: directory headers are collapsed
 * in the UI; the data we hand back is flat-but-grouped so the renderer can
 * draw section labels.
 */
export function groupFilesByDirectory(
  files: Record<string, PlaybackFsFileEntry>,
): DirectoryGroup[] {
  const byDir = new Map<string, FileNode[]>()
  for (const [path, entry] of Object.entries(files)) {
    const idx = path.lastIndexOf('/')
    const dir = idx >= 0 ? path.slice(0, idx) : ''
    const basename = idx >= 0 ? path.slice(idx + 1) : path
    const bucket = byDir.get(dir) ?? []
    bucket.push({ basename, path, entry })
    byDir.set(dir, bucket)
  }
  const out: DirectoryGroup[] = []
  // Sort: root directory first (empty string), then alphabetical.
  const dirs = Array.from(byDir.keys()).sort((a, b) => {
    if (a === '' && b !== '') return -1
    if (b === '' && a !== '') return 1
    return a.localeCompare(b)
  })
  for (const dir of dirs) {
    const files = (byDir.get(dir) ?? []).slice().sort((a, b) =>
      a.basename.localeCompare(b.basename),
    )
    out.push({ dir, files })
  }
  return out
}

// ---------------------------------------------------------------------------
// Debounce. We roll a tiny one rather than pull a util library — only one
// caller, one variant. `cancel()` is exposed so the panel can drop a pending
// fetch when the user picks a different session.
// ---------------------------------------------------------------------------

export interface Debounced<A extends unknown[]> {
  (...args: A): void
  cancel: () => void
}

export function debounce<A extends unknown[]>(
  fn: (...args: A) => void,
  ms: number,
): Debounced<A> {
  let timer: ReturnType<typeof setTimeout> | null = null
  const wrapped = ((...args: A) => {
    if (timer !== null) clearTimeout(timer)
    timer = setTimeout(() => {
      timer = null
      fn(...args)
    }, ms)
  }) as Debounced<A>
  wrapped.cancel = () => {
    if (timer !== null) {
      clearTimeout(timer)
      timer = null
    }
  }
  return wrapped
}
