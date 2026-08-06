import { memo, useMemo } from 'react'
import {
  PieChart,
  Pie,
  Cell,
  Tooltip,
  ResponsiveContainer,
  Legend,
} from 'recharts'
import type { ModelData } from '../../types/api'
import { formatModelName } from '../../services/format'
import { ChartCard, EmptyChartCard, useChartTheme, CHART_HEIGHT } from './chartTheme'

interface ModelDistributionChartProps {
  modelStats: Record<string, ModelData>
}

// #56: ≥10 distinct colors so a multi-model store doesn't wrap onto a 6-color
// cycle (two different models rendering the same hue).
const COLORS = [
  '#818CF8', '#34D399', '#F59E0B', '#F87171', '#A78BFA', '#38BDF8',
  '#FB923C', '#E879F9', '#4ADE80', '#FBBF24', '#2DD4BF', '#F472B6',
]
const OTHER_COLOR = '#9CA3AF'
const OTHER_KEY = '__other__'
// Keep the slice count bounded; everything past the top-N folds into "Other".
const TOP_N = 9

const tooltipFormatter = (value: number) => [value.toLocaleString(), 'Tokens'] as [string, string]

function ModelDistributionChart({ modelStats }: ModelDistributionChartProps) {
  const palette = useChartTheme()

  const data = useMemo(() => {
    if (!modelStats) return []
    const entries = Object.entries(modelStats)
      .map(([model, stat]) => ({
        name: formatModelName(model),
        fullName: model,
        value:
          stat.input_tokens +
          stat.output_tokens +
          stat.cache_read_tokens +
          stat.cache_creation_tokens,
      }))
      .sort((a, b) => b.value - a.value)

    if (entries.length <= TOP_N) return entries

    const top = entries.slice(0, TOP_N)
    const otherValue = entries.slice(TOP_N).reduce((s, d) => s + d.value, 0)
    if (otherValue > 0) {
      top.push({ name: 'Other', fullName: OTHER_KEY, value: otherValue })
    }
    return top
  }, [modelStats])

  if (data.length === 0) return <EmptyChartCard title="Token Distribution by Model" />

  return (
    <ChartCard title="Token Distribution by Model">
      <ResponsiveContainer width="100%" height={CHART_HEIGHT}>
        <PieChart>
          <Pie
            data={data}
            cx="50%"
            cy="50%"
            innerRadius={55}
            outerRadius={90}
            paddingAngle={2}
            dataKey="value"
            isAnimationActive={false}
          >
            {data.map((entry, index) => (
              <Cell
                key={entry.fullName}
                fill={entry.fullName === OTHER_KEY ? OTHER_COLOR : COLORS[index % COLORS.length]}
              />
            ))}
          </Pie>
          <Tooltip contentStyle={palette.tooltipContent} formatter={tooltipFormatter} />
          <Legend
            wrapperStyle={palette.legend}
            formatter={(value: string) => <span className="text-gray-600 dark:text-gray-400">{value}</span>}
          />
        </PieChart>
      </ResponsiveContainer>
    </ChartCard>
  )
}

export default memo(ModelDistributionChart)
