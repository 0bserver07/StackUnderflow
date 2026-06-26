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
import type { HourlyPattern } from '../../types/api'
import { formatTokens } from '../../services/format'
import { ChartCard, EmptyChartCard, useChartTheme, CHART_HEIGHT } from './chartTheme'

interface HourlyPatternChartProps {
  hourlyPattern: HourlyPattern
}

// #34: include cache tokens. The hourly aggregate carries cache_creation /
// cache_read but the chart used to plot only input+output, understating volume.
const TOOLTIP_LABELS: Record<string, string> = {
  input: 'Input Tokens',
  output: 'Output Tokens',
  cache_read: 'Cache Read',
  cache_creation: 'Cache Creation',
  messages: 'Messages',
}
const FLAT_RADIUS: [number, number, number, number] = [0, 0, 0, 0]
const TOP_RADIUS: [number, number, number, number] = [2, 2, 0, 0]
const tokenTickFormatter = (v: number) => formatTokens(v)
const tooltipFormatter = (value: number, name: string) =>
  [value.toLocaleString(), TOOLTIP_LABELS[name] ?? name] as [string, string]
const legendFormatter = (value: string) => TOOLTIP_LABELS[value] ?? value

function HourlyPatternChart({ hourlyPattern }: HourlyPatternChartProps) {
  const palette = useChartTheme()

  const data = useMemo(() => {
    if (!hourlyPattern?.tokens || Object.keys(hourlyPattern.tokens).length === 0) return []
    return Array.from({ length: 24 }, (_, i) => {
      const hourKey = String(i)
      const tokenData = hourlyPattern.tokens[hourKey]
      return {
        hour: i,
        label: `${i}:00`,
        input: tokenData?.input ?? 0,
        output: tokenData?.output ?? 0,
        cache_read: tokenData?.cache_read ?? 0,
        cache_creation: tokenData?.cache_creation ?? 0,
        messages: hourlyPattern.messages?.[hourKey] ?? 0,
      }
    })
  }, [hourlyPattern])

  if (data.length === 0) return <EmptyChartCard title="Hourly Token Pattern" />

  return (
    <ChartCard title="Hourly Token Pattern">
      <ResponsiveContainer width="100%" height={CHART_HEIGHT}>
        <ComposedChart data={data}>
          <CartesianGrid strokeDasharray="3 3" stroke={palette.grid} />
          <XAxis
            dataKey="label"
            tick={palette.tick}
            tickLine={palette.axisLine}
            axisLine={palette.axisLine}
            interval={2}
          />
          <YAxis
            yAxisId="tokens"
            tick={palette.tick}
            tickLine={palette.axisLine}
            axisLine={palette.axisLine}
            tickFormatter={tokenTickFormatter}
          />
          <YAxis
            yAxisId="messages"
            orientation="right"
            tick={palette.tickMuted}
            tickLine={palette.axisLine}
            axisLine={palette.axisLine}
          />
          <Tooltip
            contentStyle={palette.tooltipContent}
            labelStyle={palette.tooltipLabel}
            formatter={tooltipFormatter}
          />
          <Legend wrapperStyle={palette.legend} formatter={legendFormatter} />
          <Bar
            yAxisId="tokens"
            dataKey="input"
            stackId="tokens"
            fill="#818CF8"
            fillOpacity={0.8}
            radius={FLAT_RADIUS}
            name="input"
            isAnimationActive={false}
          />
          <Bar
            yAxisId="tokens"
            dataKey="output"
            stackId="tokens"
            fill="#34D399"
            fillOpacity={0.8}
            radius={FLAT_RADIUS}
            name="output"
            isAnimationActive={false}
          />
          <Bar
            yAxisId="tokens"
            dataKey="cache_read"
            stackId="tokens"
            fill="#F59E0B"
            fillOpacity={0.8}
            radius={FLAT_RADIUS}
            name="cache_read"
            isAnimationActive={false}
          />
          <Bar
            yAxisId="tokens"
            dataKey="cache_creation"
            stackId="tokens"
            fill="#FB923C"
            fillOpacity={0.8}
            radius={TOP_RADIUS}
            name="cache_creation"
            isAnimationActive={false}
          />
          <Line
            yAxisId="messages"
            type="monotone"
            dataKey="messages"
            stroke={palette.neutralLine}
            strokeWidth={1.5}
            strokeDasharray="4 2"
            dot={false}
            name="messages"
            isAnimationActive={false}
          />
        </ComposedChart>
      </ResponsiveContainer>
    </ChartCard>
  )
}

export default memo(HourlyPatternChart)
