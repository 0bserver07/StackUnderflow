/**
 * Shared chart chrome: theme-aware palette + the card/placeholder frame every
 * chart renders into.
 *
 * Why this exists
 * ---------------
 * 1. #59 — every chart used to hardcode dark-theme axis/grid/tooltip colors, so
 *    light mode rendered near-invisible gridlines and dark tooltips. The palette
 *    here is driven by the active `dark` class on `<html>` (the same signal
 *    `useTheme` toggles), so charts recolor their SVG chrome when the theme flips.
 * 2. #55 — charts used to `return null` while empty, collapsing their grid cell
 *    and reflowing every sibling when data arrived. `EmptyChartCard` renders a
 *    fixed-height placeholder so the grid never reflows.
 * 3. #8 — the palette objects (tooltip/tick/axis style literals) are hoisted to
 *    module scope, so a memoized chart re-using them keeps stable prop identities
 *    across renders.
 *
 * The two palettes are frozen module constants; `useChartTheme` returns one of
 * them by reference, so passing e.g. `palette.tooltipContent` to <Tooltip> never
 * creates a fresh object per render.
 */

import { useSyncExternalStore } from 'react'
import type { CSSProperties, ReactNode } from 'react'

export interface ChartPalette {
  /** True when the dark theme is active. */
  isDark: boolean
  /** CartesianGrid stroke. */
  grid: string
  /** Primary axis tick style ({ fontSize, fill }). */
  tick: { fontSize: number; fill: string }
  /** Muted/secondary axis tick style (right axes, axis labels). */
  tickMuted: { fontSize: number; fill: string }
  /** tickLine + axisLine stroke style. */
  axisLine: { stroke: string }
  /** <Tooltip> contentStyle. */
  tooltipContent: CSSProperties
  /** <Tooltip> labelStyle. */
  tooltipLabel: CSSProperties
  /** <Tooltip> itemStyle. */
  tooltipItem: CSSProperties
  /** <Legend> wrapperStyle. */
  legend: CSSProperties
  /** Stroke for neutral (non-categorical) reference lines, e.g. the messages line. */
  neutralLine: string
  /** High-contrast SVG text fill (e.g. a donut's center metric). */
  textStrong: string
}

const DARK_PALETTE: ChartPalette = {
  isDark: true,
  grid: '#374151',
  tick: { fontSize: 10, fill: '#9CA3AF' },
  tickMuted: { fontSize: 10, fill: '#6B7280' },
  axisLine: { stroke: '#4B5563' },
  tooltipContent: {
    backgroundColor: '#1F2937',
    border: '1px solid #374151',
    borderRadius: '6px',
    fontSize: '12px',
  },
  tooltipLabel: { color: '#D1D5DB' },
  tooltipItem: { color: '#D1D5DB' },
  legend: { fontSize: '11px', color: '#9CA3AF' },
  neutralLine: '#9CA3AF',
  textStrong: '#F3F4F6',
}

const LIGHT_PALETTE: ChartPalette = {
  isDark: false,
  grid: '#E5E7EB',
  tick: { fontSize: 10, fill: '#6B7280' },
  tickMuted: { fontSize: 10, fill: '#9CA3AF' },
  axisLine: { stroke: '#D1D5DB' },
  tooltipContent: {
    backgroundColor: '#FFFFFF',
    border: '1px solid #E5E7EB',
    borderRadius: '6px',
    fontSize: '12px',
    color: '#111827',
  },
  tooltipLabel: { color: '#374151' },
  tooltipItem: { color: '#374151' },
  legend: { fontSize: '11px', color: '#6B7280' },
  neutralLine: '#6B7280',
  textStrong: '#111827',
}

// ── theme subscription ───────────────────────────────────────────────────────
// A single shared MutationObserver watches the `class` attribute on <html> and
// fans out to every subscribing chart via useSyncExternalStore. One observer for
// the whole app rather than one per chart instance.

function readIsDark(): boolean {
  if (typeof document === 'undefined') return true
  return document.documentElement.classList.contains('dark')
}

let isDarkSnapshot = readIsDark()
const listeners = new Set<() => void>()
let observer: MutationObserver | null = null

function handleMutation(): void {
  const next = readIsDark()
  if (next !== isDarkSnapshot) {
    isDarkSnapshot = next
    listeners.forEach((l) => l())
  }
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  if (!observer && typeof document !== 'undefined' && typeof MutationObserver !== 'undefined') {
    observer = new MutationObserver(handleMutation)
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ['class'] })
  }
  return () => {
    listeners.delete(listener)
    if (listeners.size === 0 && observer) {
      observer.disconnect()
      observer = null
    }
  }
}

function getSnapshot(): boolean {
  return isDarkSnapshot
}

function getServerSnapshot(): boolean {
  return true
}

/**
 * Returns the palette for the currently active theme. Re-renders the calling
 * chart whenever the `dark` class on <html> is toggled.
 */
export function useChartTheme(): ChartPalette {
  const isDark = useSyncExternalStore(subscribe, getSnapshot, getServerSnapshot)
  return isDark ? DARK_PALETTE : LIGHT_PALETTE
}

// ── card frame ───────────────────────────────────────────────────────────────

const CARD_CLASS =
  'bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-4 border border-gray-200 dark:border-gray-800'
const TITLE_CLASS = 'text-sm font-medium text-gray-700 dark:text-gray-300 mb-3'

/** Default chart body height — matches the historical ResponsiveContainer height. */
export const CHART_HEIGHT = 280

interface ChartCardProps {
  title: string
  /** Optional node rendered inline after the title (e.g. a "N total" badge). */
  titleAccessory?: ReactNode
  children: ReactNode
}

/** The standard chart card: themed surface + title + body. */
export function ChartCard({ title, titleAccessory, children }: ChartCardProps) {
  return (
    <div className={CARD_CLASS}>
      <h3 className={TITLE_CLASS}>
        {title}
        {titleAccessory}
      </h3>
      {children}
    </div>
  )
}

interface EmptyChartCardProps {
  title: string
  titleAccessory?: ReactNode
  /** Body height; defaults to CHART_HEIGHT so the empty slot matches a rendered chart. */
  height?: number
  message?: string
}

/**
 * Fixed-height placeholder shown when a chart has no data. Renders the same card
 * chrome and body height as a populated chart so the surrounding grid never
 * reflows when data resolves (#55).
 */
export function EmptyChartCard({
  title,
  titleAccessory,
  height = CHART_HEIGHT,
  message = 'No data',
}: EmptyChartCardProps) {
  return (
    <ChartCard title={title} titleAccessory={titleAccessory}>
      <div
        className="flex items-center justify-center text-xs text-gray-400 dark:text-gray-600"
        style={{ height }}
      >
        {message}
      </div>
    </ChartCard>
  )
}
