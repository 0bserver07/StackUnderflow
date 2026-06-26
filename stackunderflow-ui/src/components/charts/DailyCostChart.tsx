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
const costTickFormatter = (v: number) => `$${v.toFixed(2)}`
const costTooltipFormatter = (value: number, name: string) =>
  [`$${value.toFixed(4)}`, TOOLTIP_LABELS[name] ?? name] as [string, string]
const legendFormatter = (value: string) => LEGEND_LABELS[value] ?? value

function DailyCostChart({ dailyStats }: DailyCostChartProps) {
  const palette = useChartTheme()

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
