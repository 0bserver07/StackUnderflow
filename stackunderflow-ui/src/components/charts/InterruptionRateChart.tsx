import { memo, useMemo } from 'react'
import {
  ComposedChart,
  Bar,
  Line,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
  Legend,
} from 'recharts'
import type { DailyData } from '../../types/api'
import { formatNumber } from '../../services/format'
import { ChartCard, EmptyChartCard, useChartTheme, CHART_HEIGHT } from './chartTheme'

interface InterruptionRateChartProps {
  dailyStats: Record<string, DailyData>
}

const BAR_RADIUS: [number, number, number, number] = [2, 2, 0, 0]
const ACTIVE_DOT = { r: 5 }
const LINE_DOT = { r: 3, fill: '#F59E0B' }
const pctTickFormatter = (v: number) => `${v}%`
const countTickFormatter = (v: number) => formatNumber(v)
const tooltipFormatter = (value: number, name: string) => {
  if (name === 'Interruption Rate') return [`${value}%`, name] as [string, string]
  return [value.toLocaleString(), name] as [string, string]
}
const legendFormatter = (value: string) => (
  <span className="text-gray-600 dark:text-gray-400">{value}</span>
)

function InterruptionRateChart({ dailyStats }: InterruptionRateChartProps) {
  const palette = useChartTheme()

  const data = useMemo(() => {
    if (!dailyStats) return []
    return Object.entries(dailyStats)
      .map(([date, d]) => ({
        date,
        interruption_rate: parseFloat(d.interruption_rate.toFixed(1)),
        user_commands: d.user_commands,
        interrupted_commands: d.interrupted_commands,
      }))
      .sort((a, b) => a.date.localeCompare(b.date))
  }, [dailyStats])

  if (data.length === 0) return <EmptyChartCard title="Interruption Rate Over Time" />

  return (
    <ChartCard title="Interruption Rate Over Time">
      <ResponsiveContainer width="100%" height={CHART_HEIGHT}>
        <ComposedChart data={data}>
          <CartesianGrid strokeDasharray="3 3" stroke={palette.grid} />
          <XAxis
            dataKey="date"
            tick={palette.tick}
            tickLine={palette.axisLine}
            axisLine={palette.axisLine}
          />
          <YAxis
            yAxisId="left"
            tick={palette.tick}
            tickLine={palette.axisLine}
            axisLine={palette.axisLine}
            tickFormatter={pctTickFormatter}
            domain={[0, 'auto']}
          />
          <YAxis
            yAxisId="right"
            orientation="right"
            tick={palette.tick}
            tickLine={palette.axisLine}
            axisLine={palette.axisLine}
            tickFormatter={countTickFormatter}
          />
          <Tooltip
            contentStyle={palette.tooltipContent}
            labelStyle={palette.tooltipLabel}
            formatter={tooltipFormatter}
          />
          <Legend wrapperStyle={palette.legend} formatter={legendFormatter} />
          <Bar
            yAxisId="right"
            dataKey="user_commands"
            name="User Commands"
            fill="#818CF8"
            fillOpacity={0.5}
            radius={BAR_RADIUS}
            isAnimationActive={false}
          />
          <Bar
            yAxisId="right"
            dataKey="interrupted_commands"
            name="Interrupted"
            fill="#F87171"
            fillOpacity={0.7}
            radius={BAR_RADIUS}
            isAnimationActive={false}
          />
          <Line
            yAxisId="left"
            type="monotone"
            dataKey="interruption_rate"
            name="Interruption Rate"
            stroke="#F59E0B"
            strokeWidth={2}
            dot={LINE_DOT}
            activeDot={ACTIVE_DOT}
            isAnimationActive={false}
          />
        </ComposedChart>
      </ResponsiveContainer>
    </ChartCard>
  )
}

export default memo(InterruptionRateChart)
