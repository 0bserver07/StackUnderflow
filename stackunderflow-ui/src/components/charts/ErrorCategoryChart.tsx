import { memo, useMemo } from 'react'
import {
  PieChart,
  Pie,
  Cell,
  Tooltip,
  ResponsiveContainer,
  Legend,
} from 'recharts'
import { ChartCard, EmptyChartCard, useChartTheme, CHART_HEIGHT } from './chartTheme'

interface ErrorCategoryChartProps {
  errorCategories: Record<string, number>
}

const COLORS = [
  '#F87171', '#F59E0B', '#818CF8', '#34D399', '#A78BFA',
  '#38BDF8', '#FB923C', '#E879F9', '#2DD4BF', '#FCD34D',
]
// eslint-disable-next-line @typescript-eslint/no-explicit-any
const tooltipFormatter = (_value: number, _name: string, props: any) => {
  const { value, percentage } = props?.payload ?? {}
  return [`${(value ?? _value).toLocaleString()} (${percentage ?? 0}%)`, 'Errors'] as [string, string]
}
const legendFormatter = (value: string) => (
  <span className="text-gray-600 dark:text-gray-400">{value}</span>
)

function ErrorCategoryChart({ errorCategories }: ErrorCategoryChartProps) {
  const palette = useChartTheme()

  const data = useMemo(() => {
    if (!errorCategories) return []
    const total = Object.values(errorCategories).reduce((s, v) => s + v, 0)
    if (total === 0) return []
    return Object.entries(errorCategories)
      .filter(([, count]) => count > 0)
      .map(([category, count]) => ({
        name: category,
        value: count,
        percentage: parseFloat(((count / total) * 100).toFixed(1)),
      }))
      .sort((a, b) => b.value - a.value)
  }, [errorCategories])

  if (data.length === 0) return <EmptyChartCard title="Error Categories" />

  return (
    <ChartCard title="Error Categories">
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
              <Cell key={entry.name} fill={COLORS[index % COLORS.length]} />
            ))}
          </Pie>
          <Tooltip contentStyle={palette.tooltipContent} formatter={tooltipFormatter} />
          <Legend wrapperStyle={palette.legend} formatter={legendFormatter} />
        </PieChart>
      </ResponsiveContainer>
    </ChartCard>
  )
}

export default memo(ErrorCategoryChart)
