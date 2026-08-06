import { memo, useMemo } from 'react'
import {
  BarChart,
  Bar,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
  Legend,
} from 'recharts'
import type { DailyData } from '../../types/api'
import { formatCost } from '../../services/format'
import { useCurrency } from '../../services/currency'
import { ChartCard, EmptyChartCard, useChartTheme, CHART_HEIGHT } from './chartTheme'

interface DailyCostChartProps {
  dailyStats: Record<string, DailyData>
}

// Hoisted so identities stay stable across renders of the memoized chart.
const TOOLTIP_LABELS: Record<string, string> = {
  input_cost: 'Input Cost',
  output_cost: 'Output Cost',
  cache_cost: 'Cache Cost',
}
const LEGEND_LABELS: Record<string, string> = {
  input_cost: 'Input',
  output_cost: 'Output',
  cache_cost: 'Cache',
}
const FLAT_RADIUS: [number, number, number, number] = [0, 0, 0, 0]
const TOP_RADIUS: [number, number, number, number] = [4, 4, 0, 0]
const legendFormatter = (value: string) => LEGEND_LABELS[value] ?? value

function DailyCostChart({ dailyStats }: DailyCostChartProps) {
  const palette = useChartTheme()
  // #21: the backend pre-converts these amounts into the active currency, but
  // the axis/tooltip used a hardcoded `$` — an EUR/GBP store rendered
  // converted numbers behind the wrong symbol. formatCost carries the symbol.
  const { currency } = useCurrency()
  const costTickFormatter = useMemo(
    () => (v: number) => formatCost(v, currency),
    [currency],
  )
  const costTooltipFormatter = useMemo(
    () =>
      (value: number, name: string) =>
        [formatCost(value, currency), TOOLTIP_LABELS[name] ?? name] as [string, string],
    [currency],
  )

  const data = useMemo(() => {
    if (!dailyStats) return []
    return Object.entries(dailyStats)
      .map(([date, d]) => {
        let inputCost = 0
        let outputCost = 0
        let cacheCost = 0

        if (d.cost.by_model) {
          for (const modelCost of Object.values(d.cost.by_model)) {
            inputCost += modelCost.input_cost
            outputCost += modelCost.output_cost
            cacheCost += modelCost.cache_creation_cost + modelCost.cache_read_cost
          }
        }

        return {
          date,
          input_cost: inputCost,
          output_cost: outputCost,
          cache_cost: cacheCost,
        }
      })
      .sort((a, b) => a.date.localeCompare(b.date))
  }, [dailyStats])

  if (data.length === 0) return <EmptyChartCard title="Daily Cost" />

  return (
    <ChartCard title="Daily Cost">
      <ResponsiveContainer width="100%" height={CHART_HEIGHT}>
        <BarChart data={data}>
          <CartesianGrid strokeDasharray="3 3" stroke={palette.grid} />
          <XAxis
            dataKey="date"
            tick={palette.tick}
            tickLine={palette.axisLine}
            axisLine={palette.axisLine}
          />
          <YAxis
            tick={palette.tick}
            tickLine={palette.axisLine}
            axisLine={palette.axisLine}
            tickFormatter={costTickFormatter}
          />
          <Tooltip
            contentStyle={palette.tooltipContent}
            labelStyle={palette.tooltipLabel}
            formatter={costTooltipFormatter}
          />
          <Legend wrapperStyle={palette.legend} formatter={legendFormatter} />
          <Bar
            dataKey="input_cost"
            stackId="cost"
            fill="#818CF8"
            radius={FLAT_RADIUS}
            name="input_cost"
            isAnimationActive={false}
          />
          <Bar
            dataKey="output_cost"
            stackId="cost"
            fill="#34D399"
            radius={FLAT_RADIUS}
            name="output_cost"
            isAnimationActive={false}
          />
          <Bar
            dataKey="cache_cost"
            stackId="cost"
            fill="#F59E0B"
            radius={TOP_RADIUS}
            name="cache_cost"
            isAnimationActive={false}
          />
        </BarChart>
      </ResponsiveContainer>
    </ChartCard>
  )
}

export default memo(DailyCostChart)
