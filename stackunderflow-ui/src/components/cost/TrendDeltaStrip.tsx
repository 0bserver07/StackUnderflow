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

/**
 * Horizontal strip of 4 tiles — each shows current week value + delta vs prior week.
 * Colored arrows: red for regressions (cost/errors going up), green for improvements.
 */
export default function TrendDeltaStrip({ trends }: TrendDeltaStripProps) {
  if (!trends || !trends.current_week || !trends.prior_week || !trends.delta_pct) {
    return (
      <div className="bg-gray-800/50 rounded-lg p-4 border border-gray-800">
        <div className="text-xs text-gray-500">No trend data yet — need at least two weeks of activity.</div>
      </div>
    )
  }

  const { current_week, delta_pct } = trends

  return (
    <div className="bg-gray-800/50 rounded-lg border border-gray-800">
      <div className="grid grid-cols-2 md:grid-cols-4 divide-x divide-gray-800">
        {TILES.map((tile) => {
          const current = current_week[tile.key] ?? 0
          const delta = delta_pct[tile.key] ?? 0
          return (
            <div key={tile.key} className="px-4 py-3">
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
            </div>
          )
        })}
      </div>
    </div>
  )
}
