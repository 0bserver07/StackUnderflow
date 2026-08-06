import { memo } from 'react'
import {
  BarChart, Bar, XAxis, YAxis, CartesianGrid, Tooltip, ResponsiveContainer,
} from 'recharts'
import { formatCost } from '../services/format'
import { useChartTheme } from '../components/charts/chartTheme'
import type { CurrencyInfo } from '../types/api'

// Lazy-loaded chart child of Overview — see OverviewTokenChart for the why.
// Markup/behavior is identical to the bar chart that previously rendered
// inline in Overview (currency-aware axis/tooltip, categorical date axis).

// #58 trend-chart date helpers. Bars stay on a categorical `date` axis; the
// `T00:00:00` suffix forces local parsing so labels match the calendar day.
function toLocalDate(value: number | string): Date {
  return typeof value === 'number' ? new Date(value) : new Date(`${value}T00:00:00`)
}
function formatAxisDate(value: number | string): string {
  return toLocalDate(value).toLocaleDateString(undefined, { month: 'short', day: 'numeric' })
}
function formatTooltipDate(value: number | string): string {
  return toLocalDate(value).toLocaleDateString(undefined, { month: 'short', day: 'numeric', year: 'numeric' })
}

interface CostDatum {
  date: string
  cost: number
  by_model?: Record<string, number>
}

interface OverviewCostChartProps {
  data: CostDatum[]
  currency: CurrencyInfo | null
}

function OverviewCostChart({ data, currency }: OverviewCostChartProps) {
  // #59: chart chrome follows the active theme (was fixed dark hexes).
  const palette = useChartTheme()
  return (
    <ResponsiveContainer width="100%" height={200}>
      <BarChart data={data}>
        <CartesianGrid strokeDasharray="3 3" stroke={palette.grid} />
        {/* #58: bars stay on a categorical date axis (so width renders
            reliably), but a compact tick formatter + minTickGap +
            preserveStartEnd fix the overcrowded labels. */}
        <XAxis
          dataKey="date"
          tick={palette.tick}
          tickFormatter={formatAxisDate}
          minTickGap={40}
          interval="preserveStartEnd"
        />
        {/* #58: Y axis now goes through formatCost (was a raw `${symbol}${v}`). */}
        <YAxis tick={palette.tick} tickFormatter={(v: number) => formatCost(v, currency)} />
        <Tooltip
          contentStyle={palette.tooltipContent}
          labelFormatter={formatTooltipDate}
          formatter={(v: number) => [formatCost(v, currency), 'Cost']}
        />
        <Bar dataKey="cost" fill="#818CF8" radius={[2, 2, 0, 0]} isAnimationActive={false} />
      </BarChart>
    </ResponsiveContainer>
  )
}

export default memo(OverviewCostChart)
