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
import { ChartCard, EmptyChartCard, useChartTheme } from './chartTheme'

interface ToolUsageBarChartProps {
  toolStats: Record<string, number>
}

const COLORS = [
  '#818CF8', '#34D399', '#F59E0B', '#F87171', '#A78BFA',
  '#38BDF8', '#FB923C', '#E879F9', '#2DD4BF', '#FCD34D',
]
const CHART_MARGIN = { left: 20 }
const BAR_RADIUS: [number, number, number, number] = [0, 4, 4, 0]
const countTickFormatter = (v: number) => formatNumber(v)
const tooltipFormatter = (value: number) => [value.toLocaleString(), 'Uses'] as [string, string]

function ToolUsageBarChart({ toolStats }: ToolUsageBarChartProps) {
  const palette = useChartTheme()

  const { data, leftMargin } = useMemo(() => {
    if (!toolStats) return { data: [], leftMargin: 20 }
    const rows = Object.entries(toolStats)
      .map(([name, count]) => ({ name, count }))
      .sort((a, b) => b.count - a.count)
      .slice(0, 10)
    // Compute left margin based on longest tool name.
    const maxLabelLen = rows.length ? Math.max(...rows.map((d) => d.name.length)) : 0
    return { data: rows, leftMargin: Math.min(maxLabelLen * 6, 160) }
  }, [toolStats])

  if (data.length === 0) return <EmptyChartCard title="Top Tools by Usage" />

  return (
    <ChartCard title="Top Tools by Usage">
      <ResponsiveContainer width="100%" height={Math.min(420, Math.max(280, data.length * 28))}>
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
            dataKey="name"
            tick={palette.tick}
            tickLine={palette.axisLine}
            axisLine={palette.axisLine}
            width={leftMargin}
          />
          <Tooltip
            contentStyle={palette.tooltipContent}
            labelStyle={palette.tooltipLabel}
            formatter={tooltipFormatter}
          />
          <Bar dataKey="count" radius={BAR_RADIUS} isAnimationActive={false}>
            {data.map((entry, index) => (
              <Cell key={entry.name} fill={COLORS[index % COLORS.length]} />
            ))}
          </Bar>
        </BarChart>
      </ResponsiveContainer>
    </ChartCard>
  )
}

export default memo(ToolUsageBarChart)
