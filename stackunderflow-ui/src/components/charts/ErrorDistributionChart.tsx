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

interface ErrorDistributionChartProps {
  errorCategories: Record<string, number>
}

const COLORS = ['#F87171', '#FB923C', '#FBBF24', '#A78BFA', '#818CF8', '#38BDF8', '#34D399', '#F472B6', '#6B7280', '#E879F9']

export default function ErrorDistributionChart({ errorCategories }: ErrorDistributionChartProps) {
  if (!errorCategories || Object.keys(errorCategories).length === 0) return null

  const data = Object.entries(errorCategories)
    .filter(([, count]) => count > 0)
    .map(([category, count]) => ({ category, count }))
    .sort((a, b) => b.count - a.count)

  if (data.length === 0) return null

  const total = data.reduce((sum, d) => sum + d.count, 0)

  return (
    <div className="bg-gray-800/50 rounded-lg p-4 border border-gray-800">
      <h3 className="text-sm font-medium text-gray-300 mb-3">
        Error Categories
        <span className="ml-2 text-xs text-gray-500 font-normal">{total} total</span>
      </h3>
      <ResponsiveContainer width="100%" height={250}>
        <BarChart data={data} layout="vertical" margin={{ left: 20 }}>
          <CartesianGrid strokeDasharray="3 3" stroke="#374151" horizontal={false} />
          <XAxis
            type="number"
            tick={{ fontSize: 10, fill: '#9CA3AF' }}
            tickLine={{ stroke: '#4B5563' }}
            axisLine={{ stroke: '#4B5563' }}
          />
          <YAxis
            type="category"
            dataKey="category"
            tick={{ fontSize: 10, fill: '#9CA3AF' }}
            tickLine={{ stroke: '#4B5563' }}
            axisLine={{ stroke: '#4B5563' }}
            width={130}
          />
          <Tooltip
            contentStyle={{
              backgroundColor: '#1F2937',
              border: '1px solid #374151',
              borderRadius: '6px',
              fontSize: '12px',
            }}
            labelStyle={{ color: '#D1D5DB' }}
            formatter={(value: number) => [
              `${value.toLocaleString()} (${((value / total) * 100).toFixed(1)}%)`,
              'Errors',
            ]}
          />
          <Bar dataKey="count" radius={[0, 4, 4, 0]}>
            {data.map((_entry, index) => (
              <Cell key={index} fill={COLORS[index % COLORS.length]} />
            ))}
          </Bar>
        </BarChart>
      </ResponsiveContainer>
    </div>
  )
}
