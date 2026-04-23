import {
  IconTrendingUp,
  IconTrendingDown,
  IconMinus,
  IconCurrencyDollar,
  IconAlertTriangle,
  IconTool,
  IconHash,
} from '@tabler/icons-react'
import type { Trends, TrendMetrics } from '../../types/api'

interface TrendDeltaStripProps {
  trends: Trends | null | undefined
  /**
   * End of the trend window (i.e. the most recent timestamp covered by `current_week`).
   * Used to label the tile tooltips with their date ranges.
   * If omitted, falls back to `new Date()` (i.e. "today" in the user's locale).
   * Accepts an ISO 8601 string (e.g. `overview.date_range.end`) or a `Date`.
   */
  endDate?: string | Date | null
  /**
   * Optional click handler invoked with the metric key when a tile is clicked.
   * Independent of the dispatched `stackunderflow:filter-window` custom event,
   * which always fires regardless of whether this prop is supplied.
   */
  onTileClick?: (metric: keyof TrendMetrics) => void
}

interface TileSpec {
  key: keyof TrendMetrics
  label: string
  icon: React.ReactNode
  format: (n: number) => string
  // For these metrics, a rise is bad (red). All four here are "per command" costs/errors/complexity.
  riseIsBad: boolean
}

const TILES: TileSpec[] = [
  {
    key: 'cost_per_command',
    label: 'Cost / Cmd',
    icon: <IconCurrencyDollar size={14} />,
    format: (n) => `$${n.toFixed(4)}`,
    riseIsBad: true,
  },
  {
    key: 'errors_per_command',
    label: 'Errors / Cmd',
    icon: <IconAlertTriangle size={14} />,
    format: (n) => n.toFixed(2),
    riseIsBad: true,
  },
  {
    key: 'tools_per_command',
    label: 'Tools / Cmd',
    icon: <IconTool size={14} />,
    format: (n) => n.toFixed(1),
    riseIsBad: true,
  },
  {
    key: 'tokens_per_command',
    label: 'Tokens / Cmd',
    icon: <IconHash size={14} />,
    format: (n) => {
      if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`
      if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`
      return n.toFixed(0)
    },
    riseIsBad: true,
  },
]

function deltaIcon(delta: number, riseIsBad: boolean) {
  if (Math.abs(delta) < 0.5) {
    return <IconMinus size={12} className="text-gray-500" />
  }
  const up = delta > 0
  const bad = up === riseIsBad
  const className = bad ? 'text-red-400' : 'text-green-400'
  return up
    ? <IconTrendingUp size={12} className={className} />
    : <IconTrendingDown size={12} className={className} />
}

function deltaColor(delta: number, riseIsBad: boolean): string {
  if (Math.abs(delta) < 0.5) return 'text-gray-500'
  const bad = (delta > 0) === riseIsBad
  return bad ? 'text-red-400' : 'text-green-400'
}

function parseEndDate(input: string | Date | null | undefined): Date {
  if (input instanceof Date) return Number.isNaN(input.getTime()) ? new Date() : input
  if (typeof input === 'string') {
    const d = new Date(input)
    if (!Number.isNaN(d.getTime())) return d
  }
  return new Date()
}

const MONTH_DAY = new Intl.DateTimeFormat('en-US', { month: 'short', day: 'numeric' })
const MONTH_DAY_YEAR = new Intl.DateTimeFormat('en-US', { month: 'short', day: 'numeric', year: 'numeric' })

/** "Apr 16 – Apr 23, 2026" (year suffixed once; same year is implied for the start). */
function formatRange(start: Date, end: Date): string {
  // Same calendar year: drop year from start label.
  if (start.getFullYear() === end.getFullYear()) {
    return `${MONTH_DAY.format(start)} – ${MONTH_DAY_YEAR.format(end)}`
  }
  return `${MONTH_DAY_YEAR.format(start)} – ${MONTH_DAY_YEAR.format(end)}`
}

interface Windows {
  currentLabel: string
  priorLabel: string
}

function computeWindows(end: Date): Windows {
  // Mirrors backend `_trends`: current = (end-7d, end], prior = (end-14d, end-7d].
  const day = 24 * 60 * 60 * 1000
  const curStart = new Date(end.getTime() - 7 * day)
  const priorStart = new Date(end.getTime() - 14 * day)
  const priorEnd = curStart
  return {
    currentLabel: formatRange(curStart, end),
    priorLabel: formatRange(priorStart, priorEnd),
  }
}

/**
 * Horizontal strip of 4 tiles — each shows current week value + delta vs prior week.
 * Colored arrows: red for regressions (cost/errors going up), green for improvements.
 *
 * Hover a tile for a tooltip with the raw current/prior values and window date ranges.
 * Click a tile to (a) call the optional `onTileClick(metric)` prop, and
 * (b) dispatch a `stackunderflow:filter-window` custom event with
 * `detail: {window: 'current-week', metric}` so the parent dashboard can apply a filter.
 */
export default function TrendDeltaStrip({ trends, endDate, onTileClick }: TrendDeltaStripProps) {
  if (!trends || !trends.current_week || !trends.prior_week || !trends.delta_pct) {
    return (
      <div
        data-testid="trend-delta-strip-empty"
        className="bg-gray-800/50 rounded-lg p-4 border border-gray-800"
      >
        <div className="text-xs text-gray-500">No trend data yet — need at least two weeks of activity.</div>
      </div>
    )
  }

  const { current_week, prior_week, delta_pct } = trends
  const end = parseEndDate(endDate)
  const { currentLabel, priorLabel } = computeWindows(end)

  const handleActivate = (metric: keyof TrendMetrics) => {
    onTileClick?.(metric)
    if (typeof window !== 'undefined' && typeof CustomEvent === 'function') {
      window.dispatchEvent(
        new CustomEvent('stackunderflow:filter-window', {
          detail: { window: 'current-week', metric },
        }),
      )
    }
  }

  return (
    <div
      data-testid="trend-delta-strip"
      className="bg-gray-800/50 rounded-lg border border-gray-800"
    >
      <div className="grid grid-cols-2 md:grid-cols-4 divide-x divide-gray-800">
        {TILES.map((tile) => {
          const current = current_week[tile.key] ?? 0
          const prior = prior_week[tile.key] ?? 0
          const delta = delta_pct[tile.key] ?? 0
          const titleText =
            `${tile.label}\n` +
            `Current (${currentLabel}): ${tile.format(current)}\n` +
            `Prior   (${priorLabel}): ${tile.format(prior)}\n` +
            `Click to filter dashboard to current week`
          return (
            <button
              key={tile.key}
              type="button"
              data-testid={`trend-tile-${tile.key}`}
              onClick={() => handleActivate(tile.key)}
              title={titleText}
              aria-label={`${tile.label}, current ${tile.format(current)}, prior ${tile.format(prior)}. Click to filter to current week.`}
              className="relative group text-left px-4 py-3 cursor-pointer hover:bg-gray-800/40 focus:bg-gray-800/40 focus:outline-none focus-visible:ring-1 focus-visible:ring-gray-600 transition-colors"
            >
              <div className="flex items-center gap-1.5 text-[10px] text-gray-500 uppercase tracking-wider">
                <span className="text-gray-600">{tile.icon}</span>
                {tile.label}
              </div>
              <div className="flex items-baseline gap-2 mt-1">
                <span className="text-xl font-semibold text-gray-100 tabular-nums">
                  {tile.format(current)}
                </span>
                <span className={`inline-flex items-center gap-0.5 text-xs font-medium tabular-nums ${deltaColor(delta, tile.riseIsBad)}`}>
                  {deltaIcon(delta, tile.riseIsBad)}
                  {Math.abs(delta) < 0.05 ? '0%' : `${delta > 0 ? '+' : ''}${delta.toFixed(1)}%`}
                </span>
              </div>
              <div className="text-[10px] text-gray-500 mt-0.5">vs. prior 7 days</div>

              {/* Styled floating tooltip — shown on hover/focus, hidden otherwise.
                  Positioned above the tile and pointer-events-none so it never blocks the click. */}
              <div
                role="tooltip"
                data-testid={`trend-tile-tooltip-${tile.key}`}
                className="pointer-events-none absolute left-1/2 -translate-x-1/2 bottom-full mb-2 z-20 w-56 opacity-0 group-hover:opacity-100 group-focus:opacity-100 group-focus-within:opacity-100 transition-opacity"
              >
                <div className="rounded-md border border-gray-700 bg-gray-900/95 shadow-lg px-3 py-2 text-[11px] leading-snug text-gray-200">
                  <div className="font-semibold text-gray-100 mb-1">{tile.label}</div>
                  <div className="flex justify-between gap-2">
                    <span className="text-gray-400">Current</span>
                    <span className="tabular-nums text-gray-100">{tile.format(current)}</span>
                  </div>
                  <div className="text-[10px] text-gray-500 mb-1">{currentLabel}</div>
                  <div className="flex justify-between gap-2">
                    <span className="text-gray-400">Prior</span>
                    <span className="tabular-nums text-gray-100">{tile.format(prior)}</span>
                  </div>
                  <div className="text-[10px] text-gray-500 mb-1.5">{priorLabel}</div>
                  <div className="border-t border-gray-800 pt-1 text-[10px] text-gray-500">
                    Click to filter to current week
                  </div>
                </div>
              </div>
            </button>
          )
        })}
      </div>
    </div>
  )
}
