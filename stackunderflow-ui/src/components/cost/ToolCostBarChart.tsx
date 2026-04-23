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
import type { ToolCost } from '../../types/api'

interface ToolCostBarChartProps {
  data: Record<string, ToolCost>
}

const COLORS = [
  '#818CF8', '#34D399', '#F59E0B', '#F87171', '#A78BFA',
  '#38BDF8', '#FB923C', '#E879F9', '#2DD4BF', '#FCD34D',
]

function formatCost(cost: number): string {
  if (cost >= 100) return `$${cost.toFixed(0)}`
  if (cost >= 1) return `$${cost.toFixed(2)}`
  return `$${cost.toFixed(4)}`
}

function formatTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`
  if (n >= 1_000) return `${(n / 1_000).toFixed(0)}K`
  return String(n)
}

export default function ToolCostBarChart({ data }: ToolCostBarChartProps) {
  if (!data || Object.keys(data).length === 0) {
    return (
      <div className="bg-gray-800/50 rounded-lg p-4 border border-gray-800">
        <h3 className="text-sm font-medium text-gray-300 mb-3">Tools by Cost</h3>
        <div className="text-xs text-gray-500 py-8 text-center">No tool cost data yet</div>
      </div>
    )
  }

  const chartData = Object.entries(data)
    .map(([name, stat]) => ({
      name,
      cost: stat.cost ?? 0,
      calls: stat.calls ?? 0,
      tokens:
        (stat.input_tokens ?? 0) +
        (stat.output_tokens ?? 0) +
        (stat.cache_read_tokens ?? 0) +
        (stat.cache_creation_tokens ?? 0),
    }))
    .filter((d) => d.cost > 0 || d.calls > 0)
    .sort((a, b) => b.cost - a.cost)
    .slice(0, 12)

  if (chartData.length === 0) {
    return (
      <div className="bg-gray-800/50 rounded-lg p-4 border border-gray-800">
        <h3 className="text-sm font-medium text-gray-300 mb-3">Tools by Cost</h3>
        <div className="text-xs text-gray-500 py-8 text-center">No tool cost data yet</div>
      </div>
    )
  }

  const maxLabelLen = Math.max(...chartData.map((d) => d.name.length))
  const leftMargin = Math.min(maxLabelLen * 6, 160)

  return (
    <div className="bg-gray-800/50 rounded-lg p-4 border border-gray-800">
      <h3 className="text-sm font-medium text-gray-300 mb-3">Tools by Cost</h3>
      <ResponsiveContainer width="100%" height={Math.max(260, chartData.length * 32)}>
        <BarChart data={chartData} layout="vertical" margin={{ left: 20 }}>
          <CartesianGrid strokeDasharray="3 3" stroke="#374151" horizontal={false} />
          <XAxis
            type="number"
            tick={{ fontSize: 10, fill: '#9CA3AF' }}
            tickLine={{ stroke: '#4B5563' }}
            axisLine={{ stroke: '#4B5563' }}
            tickFormatter={formatCost}
          />
          <YAxis
            type="category"
            dataKey="name"
            tick={{ fontSize: 10, fill: '#9CA3AF' }}
            tickLine={{ stroke: '#4B5563' }}
            axisLine={{ stroke: '#4B5563' }}
            width={leftMargin}
          />
          <Tooltip
            contentStyle={{
              backgroundColor: '#1F2937',
              border: '1px solid #374151',
              borderRadius: '6px',
              fontSize: '12px',
            }}
            labelStyle={{ color: '#D1D5DB' }}
            // eslint-disable-next-line @typescript-eslint/no-explicit-any
            formatter={(value: number, _name: string, props: any) => {
              const p = props?.payload
              if (!p) return [formatCost(value), 'Cost']
              return [
                `${formatCost(value)} · ${p.calls} calls · ${formatTokens(p.tokens)} tokens`,
                'Cost',
              ]
            }}
          />
          <Bar dataKey="cost" radius={[0, 4, 4, 0]}>
            {chartData.map((_entry, index) => (
              <Cell key={index} fill={COLORS[index % COLORS.length]} />
            ))}
          </Bar>
        </BarChart>
      </ResponsiveContainer>
    </div>
  )
}
