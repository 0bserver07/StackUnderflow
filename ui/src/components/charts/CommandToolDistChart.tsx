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

interface CommandToolDistChartProps {
  toolCountDist: Record<string, number>
}

const COLORS = ['#818CF8', '#34D399', '#F59E0B', '#F87171', '#A78BFA', '#38BDF8']
const BAR_RADIUS: [number, number, number, number] = [4, 4, 0, 0]
const pctTickFormatter = (v: number) => `${v}%`
const labelFormatter = (label: string) => `${label} tool${label === '1' ? '' : 's'}`
// eslint-disable-next-line @typescript-eslint/no-explicit-any
const tooltipFormatter = (_value: number, _name: string, props: any) => {
  const { percentage, count } = props?.payload ?? {}
  return [`${percentage ?? _value}% (${(count ?? 0).toLocaleString()} commands)`, 'Share'] as [string, string]
}

function CommandToolDistChart({ toolCountDist }: CommandToolDistChartProps) {
  const palette = useChartTheme()

  const data = useMemo(() => {
    if (!toolCountDist || Object.keys(toolCountDist).length === 0) return []

    // Bucket raw distribution into 0, 1, 2, 3, 4, 5+
    const buckets: Record<string, number> = { '0': 0, '1': 0, '2': 0, '3': 0, '4': 0, '5+': 0 }
    for (const [toolCount, cmdCount] of Object.entries(toolCountDist)) {
      const n = parseInt(toolCount, 10)
      if (isNaN(n)) continue
      if (n >= 5) {
        buckets['5+'] = (buckets['5+'] ?? 0) + cmdCount
      } else {
        const key = String(n)
        buckets[key] = (buckets[key] ?? 0) + cmdCount
      }
    }

    const total = Object.values(buckets).reduce((s, v) => s + v, 0)
    if (total === 0) return []

    return Object.entries(buckets).map(([tools, count]) => ({
      tools,
      count,
      percentage: parseFloat(((count / total) * 100).toFixed(1)),
    }))
  }, [toolCountDist])

  if (data.length === 0) return <EmptyChartCard title="Commands by Tool Count" />

  return (
    <ChartCard title="Commands by Tool Count">
      <ResponsiveContainer width="100%" height={CHART_HEIGHT}>
        <BarChart data={data}>
          <CartesianGrid strokeDasharray="3 3" stroke={palette.grid} />
          <XAxis
            dataKey="tools"
            tick={palette.tick}
            tickLine={palette.axisLine}
            axisLine={palette.axisLine}
            label={{
              value: 'Tools Used',
              position: 'insideBottom',
              offset: -2,
              style: { fontSize: 10, fill: palette.tickMuted.fill },
            }}
          />
          <YAxis
            tick={palette.tick}
            tickLine={palette.axisLine}
            axisLine={palette.axisLine}
            tickFormatter={pctTickFormatter}
            domain={[0, 'auto']}
          />
          <Tooltip
            contentStyle={palette.tooltipContent}
            labelStyle={palette.tooltipLabel}
            labelFormatter={labelFormatter}
            formatter={tooltipFormatter}
          />
          <Bar dataKey="percentage" radius={BAR_RADIUS} isAnimationActive={false}>
            {data.map((entry, index) => (
              <Cell key={entry.tools} fill={COLORS[index % COLORS.length]} />
            ))}
          </Bar>
        </BarChart>
      </ResponsiveContainer>
    </ChartCard>
  )
}

export default memo(CommandToolDistChart)
