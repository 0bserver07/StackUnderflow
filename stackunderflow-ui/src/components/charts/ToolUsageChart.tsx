import { memo, useMemo } from 'react'
import {
  BarChart,
  Bar,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
  Cell,
} from 'recharts'
import { formatNumber } from '../../services/format'
import { ChartCard, EmptyChartCard, useChartTheme, CHART_HEIGHT } from './chartTheme'

interface ToolUsageChartProps {
  toolStats: Record<string, number>
}

const COLORS = ['#818CF8', '#34D399', '#F59E0B', '#F87171', '#A78BFA', '#38BDF8', '#FB923C', '#E879F9', '#4ADE80', '#FBBF24']
const CHART_MARGIN = { left: 20 }
const BAR_RADIUS: [number, number, number, number] = [0, 4, 4, 0]
const countTickFormatter = (v: number) => formatNumber(v)
const tooltipFormatter = (value: number) => [value.toLocaleString(), 'Uses'] as [string, string]

function ToolUsageChart({ toolStats }: ToolUsageChartProps) {
  const palette = useChartTheme()

  const data = useMemo(() => {
    if (!toolStats) return []
    return Object.entries(toolStats)
      .map(([tool, count]) => ({ tool, count }))
      .sort((a, b) => b.count - a.count)
      .slice(0, 10)
  }, [toolStats])

  if (data.length === 0) return <EmptyChartCard title="Top Tool Usage" />

  return (
    <ChartCard title="Top Tool Usage">
      <ResponsiveContainer width="100%" height={CHART_HEIGHT}>
        <BarChart data={data} layout="vertical" margin={CHART_MARGIN}>
          <CartesianGrid strokeDasharray="3 3" stroke={palette.grid} horizontal={false} />
          <XAxis
            type="number"
            tick={palette.tick}
            tickLine={palette.axisLine}
            axisLine={palette.axisLine}
            tickFormatter={countTickFormatter}
          />
          <YAxis
            type="category"
            dataKey="tool"
            tick={palette.tick}
            tickLine={palette.axisLine}
            axisLine={palette.axisLine}
            width={120}
          />
          <Tooltip
            contentStyle={palette.tooltipContent}
            labelStyle={palette.tooltipLabel}
            formatter={tooltipFormatter}
          />
          <Bar dataKey="count" radius={BAR_RADIUS} isAnimationActive={false}>
            {data.map((entry, index) => (
              <Cell key={entry.tool} fill={COLORS[index % COLORS.length]} />
            ))}
          </Bar>
        </BarChart>
      </ResponsiveContainer>
    </ChartCard>
  )
}

export default memo(ToolUsageChart)
