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
import { ChartCard, EmptyChartCard, useChartTheme, CHART_HEIGHT } from './chartTheme'

interface ErrorDistributionChartProps {
  errorCategories: Record<string, number>
}

const COLORS = ['#F87171', '#FB923C', '#FBBF24', '#A78BFA', '#818CF8', '#38BDF8', '#34D399', '#F472B6', '#6B7280', '#E879F9']
const CHART_MARGIN = { left: 20 }
const BAR_RADIUS: [number, number, number, number] = [0, 4, 4, 0]

function ErrorDistributionChart({ errorCategories }: ErrorDistributionChartProps) {
  const palette = useChartTheme()

  const { data, total } = useMemo(() => {
    if (!errorCategories) return { data: [], total: 0 }
    const rows = Object.entries(errorCategories)
      .filter(([, count]) => count > 0)
      .map(([category, count]) => ({ category, count }))
      .sort((a, b) => b.count - a.count)
    return { data: rows, total: rows.reduce((sum, d) => sum + d.count, 0) }
  }, [errorCategories])

  if (data.length === 0) return <EmptyChartCard title="Error Categories" />

  // Tooltip needs the running total; created here (not hoisted) so the share %
  // tracks the current dataset. Identity only changes when `total` changes.
  const tooltipFormatter = (value: number) =>
    [`${value.toLocaleString()} (${((value / total) * 100).toFixed(1)}%)`, 'Errors'] as [string, string]

  return (
    <ChartCard
      title="Error Categories"
      titleAccessory={<span className="ml-2 text-xs text-gray-500 font-normal">{total} total</span>}
    >
      <ResponsiveContainer width="100%" height={CHART_HEIGHT}>
        <BarChart data={data} layout="vertical" margin={CHART_MARGIN}>
          <CartesianGrid strokeDasharray="3 3" stroke={palette.grid} horizontal={false} />
          <XAxis
            type="number"
            tick={palette.tick}
            tickLine={palette.axisLine}
            axisLine={palette.axisLine}
          />
          <YAxis
            type="category"
            dataKey="category"
            tick={palette.tick}
            tickLine={palette.axisLine}
            axisLine={palette.axisLine}
            width={130}
          />
          <Tooltip
            contentStyle={palette.tooltipContent}
            labelStyle={palette.tooltipLabel}
            formatter={tooltipFormatter}
          />
          <Bar dataKey="count" radius={BAR_RADIUS} isAnimationActive={false}>
            {data.map((entry, index) => (
              <Cell key={entry.category} fill={COLORS[index % COLORS.length]} />
            ))}
          </Bar>
        </BarChart>
      </ResponsiveContainer>
    </ChartCard>
  )
}

export default memo(ErrorDistributionChart)
