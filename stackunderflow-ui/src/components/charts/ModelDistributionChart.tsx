import {
  PieChart,
  Pie,
  Cell,
  Tooltip,
  ResponsiveContainer,
  Legend,
} from 'recharts'
import type { ModelData } from '../../types/api'

interface ModelDistributionChartProps {
  modelStats: Record<string, ModelData>
}

const COLORS = ['#818CF8', '#34D399', '#F59E0B', '#F87171', '#A78BFA', '#38BDF8']

export default function ModelDistributionChart({ modelStats }: ModelDistributionChartProps) {
  if (!modelStats || Object.keys(modelStats).length === 0) return null

  const data = Object.entries(modelStats).map(([model, stat]) => ({
    name: model,
    value: stat.input_tokens + stat.output_tokens + stat.cache_read_tokens + stat.cache_creation_tokens,
  }))

  if (data.length === 0) return null

  return (
    <div className="bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-4 border border-gray-200 dark:border-gray-800">
      <h3 className="text-sm font-medium text-gray-700 dark:text-gray-300 mb-3">Token Distribution by Model</h3>
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
            formatter={(value: number) => [value.toLocaleString(), 'Tokens']}
          />
          <Legend
            wrapperStyle={{ fontSize: '11px', color: '#9CA3AF' }}
            formatter={(value: string) => <span className="text-gray-600 dark:text-gray-400">{value}</span>}
          />
        </PieChart>
      </ResponsiveContainer>
    </div>
  )
}
