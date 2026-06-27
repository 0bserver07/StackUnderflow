import { memo } from 'react'
import {
  BarChart, Bar, XAxis, YAxis, CartesianGrid, Tooltip, ResponsiveContainer,
} from 'recharts'
import { formatCost } from '../services/format'
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
  return (
    <ResponsiveContainer width="100%" height={200}>
      <BarChart data={data}>
        <CartesianGrid strokeDasharray="3 3" stroke="#374151" />
        {/* #58: bars stay on a categorical date axis (so width renders
            reliably), but a compact tick formatter + minTickGap +
            preserveStartEnd fix the overcrowded labels. */}
        <XAxis
          dataKey="date"
          tick={{ fontSize: 10, fill: '#9CA3AF' }}
          tickFormatter={formatAxisDate}
          minTickGap={40}
          interval="preserveStartEnd"
        />
        {/* #58: Y axis now goes through formatCost (was a raw `${symbol}${v}`). */}
        <YAxis tick={{ fontSize: 10, fill: '#9CA3AF' }} tickFormatter={(v: number) => formatCost(v, currency)} />
        <Tooltip
          contentStyle={{ backgroundColor: '#1F2937', border: '1px solid #374151', borderRadius: '6px', fontSize: '12px' }}
          labelFormatter={formatTooltipDate}
          formatter={(v: number) => [formatCost(v, currency), 'Cost']}
        />
        <Bar dataKey="cost" fill="#818CF8" radius={[2, 2, 0, 0]} isAnimationActive={false} />
      </BarChart>
    </ResponsiveContainer>
  )
}

export default memo(OverviewCostChart)
