import { memo } from 'react'
import {
  AreaChart, Area, XAxis, YAxis, CartesianGrid, Tooltip, ResponsiveContainer,
} from 'recharts'
import { formatTokens } from '../services/format'
import { useChartTheme } from '../components/charts/chartTheme'

// Lazy-loaded chart child of Overview. Kept in its own module so recharts
// (~444KB) is pulled in via Overview's <Suspense> boundary instead of being a
// static import of the Overview chunk — the page shell + KPI cards + table
// paint without waiting on recharts. Markup/behavior is identical to the chart
// that previously rendered inline in Overview.
// #60: token labels come from the shared services/format formatTokens.

// #58 trend-chart date helpers. Accept either an epoch-ms number (the
// time-scaled token axis) or a `YYYY-MM-DD` string; appending `T00:00:00`
// forces local parsing so the label matches the underlying calendar day.
function toLocalDate(value: number | string): Date {
  return typeof value === 'number' ? new Date(value) : new Date(`${value}T00:00:00`)
}
function formatAxisDate(value: number | string): string {
  return toLocalDate(value).toLocaleDateString(undefined, { month: 'short', day: 'numeric' })
}
function formatTooltipDate(value: number | string): string {
  return toLocalDate(value).toLocaleDateString(undefined, { month: 'short', day: 'numeric', year: 'numeric' })
}

interface TokenDatum {
  date: string
  input: number
  output: number
  ts: number
}

interface OverviewTokenChartProps {
  data: TokenDatum[]
}

function OverviewTokenChart({ data }: OverviewTokenChartProps) {
  // #59: chart chrome follows the active theme (was fixed dark hexes).
  const palette = useChartTheme()
  return (
    <ResponsiveContainer width="100%" height={280}>
      <AreaChart data={data}>
        <CartesianGrid strokeDasharray="3 3" stroke={palette.grid} />
        {/* #58: time-scaled axis so idle days are spaced by real elapsed
            time instead of collapsing; minTickGap thins crowded labels. */}
        <XAxis
          dataKey="ts"
          type="number"
          scale="time"
          domain={['dataMin', 'dataMax']}
          tick={palette.tick}
          tickFormatter={formatAxisDate}
          minTickGap={40}
        />
        <YAxis tick={palette.tick} tickFormatter={formatTokens} />
        <Tooltip
          contentStyle={palette.tooltipContent}
          labelFormatter={formatTooltipDate}
          formatter={(value: number) => [value.toLocaleString(), undefined]}
        />
        <Area type="monotone" dataKey="input" stackId="1" stroke="#818CF8" fill="#818CF8" fillOpacity={0.4} name="Input" isAnimationActive={false} />
        <Area type="monotone" dataKey="output" stackId="1" stroke="#34D399" fill="#34D399" fillOpacity={0.4} name="Output" isAnimationActive={false} />
      </AreaChart>
    </ResponsiveContainer>
  )
}

export default memo(OverviewTokenChart)
