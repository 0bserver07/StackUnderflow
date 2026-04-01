import {
  PieChart,
  Pie,
  Cell,
  Tooltip,
  ResponsiveContainer,
  Legend,
} from 'recharts'

interface ErrorCategoryChartProps {
  errorCategories: Record<string, number>
}

const COLORS = [
  '#F87171', '#F59E0B', '#818CF8', '#34D399', '#A78BFA',
  '#38BDF8', '#FB923C', '#E879F9', '#2DD4BF', '#FCD34D',
]

export default function ErrorCategoryChart({ errorCategories }: ErrorCategoryChartProps) {
  if (!errorCategories || Object.keys(errorCategories).length === 0) return null

  const total = Object.values(errorCategories).reduce((s, v) => s + v, 0)
  if (total === 0) return null

  const data = Object.entries(errorCategories)
    .filter(([, count]) => count > 0)
    .map(([category, count]) => ({
      name: category,
      value: count,
      percentage: parseFloat(((count / total) * 100).toFixed(1)),
    }))
    .sort((a, b) => b.value - a.value)

  if (data.length === 0) return null

  return (
    <div className="bg-gray-800/50 rounded-lg p-4 border border-gray-800">
      <h3 className="text-sm font-medium text-gray-300 mb-3">Error Categories</h3>
      <ResponsiveContainer width="100%" height={280}>
        <PieChart>
          <Pie
            data={data}
            cx="50%"
            cy="50%"
            innerRadius={55}
            outerRadius={90}
            paddingAngle={2}
            dataKey="value"
          >
            {data.map((_entry, index) => (
              <Cell key={index} fill={COLORS[index % COLORS.length]} />
            ))}
          </Pie>
          <Tooltip
            contentStyle={{
              backgroundColor: '#1F2937',
              border: '1px solid #374151',
              borderRadius: '6px',
              fontSize: '12px',
            }}
            // eslint-disable-next-line @typescript-eslint/no-explicit-any
            formatter={(_value: number, _name: string, props: any) => {
              const { value, percentage } = props?.payload ?? {}
              return [`${(value ?? _value).toLocaleString()} (${percentage ?? 0}%)`, 'Errors']
            }}
          />
          <Legend
            wrapperStyle={{ fontSize: '11px', color: '#9CA3AF' }}
            formatter={(value: string) => <span className="text-gray-400">{value}</span>}
          />
        </PieChart>
      </ResponsiveContainer>
    </div>
  )
}
