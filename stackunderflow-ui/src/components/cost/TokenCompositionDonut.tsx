import { useState } from 'react'
import {
  PieChart,
  Pie,
  Cell,
  Tooltip,
  ResponsiveContainer,
  Legend,
  Label,
  Sector,
} from 'recharts'

interface TokenCompositionDonutProps {
  totals: Record<string, number>
}

const SERIES: { key: string; color: string; label: string }[] = [
  { key: 'input', color: '#818CF8', label: 'Input' },
  { key: 'output', color: '#34D399', label: 'Output' },
  { key: 'cache_read', color: '#F59E0B', label: 'Cache Read' },
  { key: 'cache_creation', color: '#FB923C', label: 'Cache Creation' },
]

// Slices below this share of total have no inline % label (avoids overlap on thin arcs).
const LABEL_MIN_PCT = 0.03

function formatTokens(v: number): string {
  if (v >= 1_000_000_000) return `${(v / 1_000_000_000).toFixed(1)}B`
  if (v >= 1_000_000) return `${(v / 1_000_000).toFixed(1)}M`
  if (v >= 1_000) return `${(v / 1_000).toFixed(1)}K`
  return v.toLocaleString()
}

// Inline % label renderer. Places text at the radial midpoint of each slice,
// but only when the slice is wide enough to show it cleanly.
function renderSliceLabel(props: {
  cx?: number
  cy?: number
  midAngle?: number
  innerRadius?: number
  outerRadius?: number
  percent?: number
}) {
  const { cx, cy, midAngle, innerRadius, outerRadius, percent } = props
  if (
    cx == null ||
    cy == null ||
    midAngle == null ||
    innerRadius == null ||
    outerRadius == null ||
    percent == null
  ) {
    return null
  }
  if (percent < LABEL_MIN_PCT) return null

  const RADIAN = Math.PI / 180
  // Mid-ring position so text sits inside the donut band.
  const radius = innerRadius + (outerRadius - innerRadius) * 0.55
  const x = cx + radius * Math.cos(-midAngle * RADIAN)
  const y = cy + radius * Math.sin(-midAngle * RADIAN)

  return (
    <text
      x={x}
      y={y}
      fill="#FFFFFF"
      textAnchor="middle"
      dominantBaseline="central"
      fontSize={11}
      fontWeight={600}
      style={{ pointerEvents: 'none' }}
    >
      {`${(percent * 100).toFixed(0)}%`}
    </text>
  )
}

// Hover shape — pulls the active slice outward.
function renderActiveShape(props: {
  cx?: number
  cy?: number
  innerRadius?: number
  outerRadius?: number
  startAngle?: number
  endAngle?: number
  fill?: string
}) {
  const { cx, cy, innerRadius, outerRadius, startAngle, endAngle, fill } = props
  if (
    cx == null ||
    cy == null ||
    innerRadius == null ||
    outerRadius == null ||
    startAngle == null ||
    endAngle == null
  ) {
    return <g />
  }
  return (
    <Sector
      cx={cx}
      cy={cy}
      innerRadius={innerRadius}
      outerRadius={outerRadius + 6}
      startAngle={startAngle}
      endAngle={endAngle}
      fill={fill}
    />
  )
}

export default function TokenCompositionDonut({ totals }: TokenCompositionDonutProps) {
  const [activeIndex, setActiveIndex] = useState<number | undefined>(undefined)

  const grandTotal = SERIES.reduce((sum, s) => sum + (totals?.[s.key] ?? 0), 0)

  if (!totals || grandTotal === 0) {
    return (
      <div
        className="bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-4 border border-gray-200 dark:border-gray-800"
        data-testid="token-composition-donut-empty"
      >
        <h3 className="text-sm font-medium text-gray-700 dark:text-gray-300 mb-3">Token Composition</h3>
        <div className="text-xs text-gray-500 py-8 text-center">No token data yet</div>
      </div>
    )
  }

  const data = SERIES
    .map((s) => ({
      name: s.key,
      label: s.label,
      color: s.color,
      value: totals[s.key] ?? 0,
    }))
    .filter((d) => d.value > 0)

  return (
    <div
      className="bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-4 border border-gray-200 dark:border-gray-800"
      data-testid="token-composition-donut"
    >
      <h3 className="text-sm font-medium text-gray-700 dark:text-gray-300 mb-3">
        Token Composition
        <span className="ml-2 text-xs text-gray-500 font-normal">
          {formatTokens(grandTotal)} total
        </span>
      </h3>
      <ResponsiveContainer width="100%" height={360}>
        <PieChart>
          <Pie
            data={data}
            cx="50%"
            cy="50%"
            innerRadius={85}
            outerRadius={135}
            paddingAngle={2}
            dataKey="value"
            nameKey="label"
            label={renderSliceLabel}
            labelLine={false}
            activeIndex={activeIndex}
            activeShape={renderActiveShape}
            onMouseEnter={(_, idx) => setActiveIndex(idx)}
            onMouseLeave={() => setActiveIndex(undefined)}
            isAnimationActive={false}
          >
            {data.map((entry, index) => {
              const dimmed = activeIndex !== undefined && activeIndex !== index
              return (
                <Cell
                  key={index}
                  fill={entry.color}
                  opacity={dimmed ? 0.35 : 1}
                  style={{ transition: 'opacity 150ms ease' }}
                  data-testid={`token-composition-slice-${entry.name}`}
                />
              )
            })}
            <Label
              position="center"
              content={(props) => {
                const viewBox = (props as { viewBox?: { cx?: number; cy?: number } }).viewBox
                if (!viewBox || viewBox.cx == null || viewBox.cy == null) return null
                const { cx, cy } = viewBox
                return (
                  <g data-testid="token-composition-center">
                    <text
                      x={cx}
                      y={cy - 6}
                      textAnchor="middle"
                      dominantBaseline="central"
                      fill="#F3F4F6"
                      fontSize={20}
                      fontWeight={700}
                    >
                      {formatTokens(grandTotal)}
                    </text>
                    <text
                      x={cx}
                      y={cy + 14}
                      textAnchor="middle"
                      dominantBaseline="central"
                      fill="#9CA3AF"
                      fontSize={10}
                      letterSpacing="0.06em"
                    >
                      tokens
                    </text>
                  </g>
                )
              }}
            />
          </Pie>
          <Tooltip
            contentStyle={{
              backgroundColor: '#1F2937',
              border: '1px solid #374151',
              borderRadius: '6px',
              fontSize: '12px',
            }}
            labelStyle={{ color: '#D1D5DB' }}
            formatter={(value: number) => {
              const pct = grandTotal > 0 ? (value / grandTotal) * 100 : 0
              return [`${value.toLocaleString()} (${pct.toFixed(1)}%)`, 'Tokens']
            }}
          />
          <Legend
            wrapperStyle={{ fontSize: '11px', color: '#9CA3AF' }}
            content={(props) => {
              const payload = (props as { payload?: Array<{ color?: string }> }).payload
              if (!payload || payload.length === 0) return null
              return (
                <ul
                  className="flex flex-wrap justify-center gap-x-4 gap-y-1 mt-2"
                  data-testid="token-composition-legend"
                >
                  {payload.map((entry, i) => {
                    const datum = data[i]
                    if (!datum) return null
                    const pct = grandTotal > 0 ? (datum.value / grandTotal) * 100 : 0
                    const dimmed = activeIndex !== undefined && activeIndex !== i
                    return (
                      <li
                        key={i}
                        className="flex items-center gap-1.5 text-xs transition-opacity"
                        style={{ opacity: dimmed ? 0.45 : 1 }}
                        data-testid={`token-composition-legend-item-${datum.name}`}
                      >
                        <span
                          aria-hidden="true"
                          className="inline-block w-2.5 h-2.5 rounded-sm flex-shrink-0"
                          style={{ backgroundColor: entry.color ?? datum.color }}
                        />
                        <span className="text-gray-600 dark:text-gray-400">{datum.label}</span>
                        <span className="text-gray-800 dark:text-gray-200 font-medium tabular-nums">
                          {formatTokens(datum.value)}
                        </span>
                        <span className="text-gray-500 tabular-nums">
                          ({pct.toFixed(1)}%)
                        </span>
                      </li>
                    )
                  })}
                </ul>
              )
            }}
          />
        </PieChart>
      </ResponsiveContainer>
    </div>
  )
}
