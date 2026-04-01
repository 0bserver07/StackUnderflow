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

interface ToolUsageBarChartProps {
  toolStats: Record<string, number>
}

const COLORS = [
  '#818CF8', '#34D399', '#F59E0B', '#F87171', '#A78BFA',
  '#38BDF8', '#FB923C', '#E879F9', '#2DD4BF', '#FCD34D',
]

function formatNumber(v: number): string {
  if (v >= 1_000_000) return `${(v / 1_000_000).toFixed(1)}M`
  if (v >= 1_000) return `${(v / 1_000).toFixed(0)}K`
  return String(v)
}

export default function ToolUsageBarChart({ toolStats }: ToolUsageBarChartProps) {
  if (!toolStats || Object.keys(toolStats).length === 0) return null

  const data = Object.entries(toolStats)
    .map(([name, count]) => ({ name, count }))
    .sort((a, b) => b.count - a.count)
    .slice(0, 10)

  if (data.length === 0) return null

  // Compute left margin based on longest tool name
  const maxLabelLen = Math.max(...data.map((d) => d.name.length))
  const leftMargin = Math.min(maxLabelLen * 6, 160)

  return (
    <div className="bg-gray-800/50 rounded-lg p-4 border border-gray-800">
      <h3 className="text-sm font-medium text-gray-300 mb-3">Top Tools by Usage</h3>
      <ResponsiveContainer width="100%" height={Math.max(250, data.length * 32)}>
        <BarChart data={data} layout="vertical" margin={{ left: 20 }}>
          <CartesianGrid strokeDasharray="3 3" stroke="#374151" horizontal={false} />
          <XAxis
            type="number"
            tick={{ fontSize: 10, fill: '#9CA3AF' }}
            tickLine={{ stroke: '#4B5563' }}
            axisLine={{ stroke: '#4B5563' }}
            tickFormatter={formatNumber}
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
            formatter={(value: number) => [value.toLocaleString(), 'Uses']}
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
