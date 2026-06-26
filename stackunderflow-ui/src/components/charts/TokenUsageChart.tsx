import { memo, useMemo } from 'react'
import {
  AreaChart,
  Area,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
  Legend,
} from 'recharts'
import type { DailyData } from '../../types/api'
import { formatTokens } from '../../services/format'
import { ChartCard, EmptyChartCard, useChartTheme, CHART_HEIGHT } from './chartTheme'

interface TokenUsageChartProps {
  dailyStats: Record<string, DailyData>
}

const tokenTickFormatter = (v: number) => formatTokens(v)
// #54: include the series name in the tooltip (the old formatter dropped it,
// returning `undefined`, so 4 stacked series were indistinguishable on hover).
const tokenTooltipFormatter = (value: number, name: string) =>
  [value.toLocaleString(), name] as [string, string]

function TokenUsageChart({ dailyStats }: TokenUsageChartProps) {
  const palette = useChartTheme()

  const data = useMemo(() => {
    if (!dailyStats) return []
    return Object.entries(dailyStats)
      .map(([date, d]) => ({
        date,
        input: d.tokens.input,
        output: d.tokens.output,
        cache_read: d.tokens.cache_read,
        cache_creation: d.tokens.cache_creation,
      }))
      .sort((a, b) => a.date.localeCompare(b.date))
  }, [dailyStats])

  if (data.length === 0) return <EmptyChartCard title="Daily Token Usage" />

  return (
    <ChartCard title="Daily Token Usage">
      <ResponsiveContainer width="100%" height={CHART_HEIGHT}>
        <AreaChart data={data}>
          <CartesianGrid strokeDasharray="3 3" stroke={palette.grid} />
          <XAxis
            dataKey="date"
            tick={palette.tick}
            tickLine={palette.axisLine}
            axisLine={palette.axisLine}
          />
          {/* #54: input/output (small) and cache (huge) get separate scales so
              the non-cache series don't collapse into the axis. */}
          <YAxis
            yAxisId="io"
            tick={palette.tick}
            tickLine={palette.axisLine}
            axisLine={palette.axisLine}
            tickFormatter={tokenTickFormatter}
          />
          <YAxis
            yAxisId="cache"
            orientation="right"
            tick={palette.tickMuted}
            tickLine={palette.axisLine}
            axisLine={palette.axisLine}
            tickFormatter={tokenTickFormatter}
          />
          <Tooltip
            contentStyle={palette.tooltipContent}
            labelStyle={palette.tooltipLabel}
            itemStyle={palette.tooltipItem}
            formatter={tokenTooltipFormatter}
          />
          <Legend wrapperStyle={palette.legend} />
          <Area
            yAxisId="io"
            type="monotone"
            dataKey="input"
            stackId="io"
            stroke="#818CF8"
            fill="#818CF8"
            fillOpacity={0.4}
            name="Input Tokens"
            isAnimationActive={false}
          />
          <Area
            yAxisId="io"
            type="monotone"
            dataKey="output"
            stackId="io"
            stroke="#34D399"
            fill="#34D399"
            fillOpacity={0.4}
            name="Output Tokens"
            isAnimationActive={false}
          />
          <Area
            yAxisId="cache"
            type="monotone"
            dataKey="cache_read"
            stackId="cache"
            stroke="#F59E0B"
            fill="#F59E0B"
            fillOpacity={0.25}
            name="Cache Read"
            isAnimationActive={false}
          />
          <Area
            yAxisId="cache"
            type="monotone"
            dataKey="cache_creation"
            stackId="cache"
            stroke="#FB923C"
            fill="#FB923C"
            fillOpacity={0.25}
            name="Cache Creation"
            isAnimationActive={false}
          />
        </AreaChart>
      </ResponsiveContainer>
    </ChartCard>
  )
}

export default memo(TokenUsageChart)
