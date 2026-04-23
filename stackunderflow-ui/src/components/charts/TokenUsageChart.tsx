import {
  AreaChart,
  Area,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
} from 'recharts'
import type { DailyData } from '../../types/api'

interface TokenUsageChartProps {
  dailyStats: Record<string, DailyData>
}

export default function TokenUsageChart({ dailyStats }: TokenUsageChartProps) {
  if (!dailyStats || Object.keys(dailyStats).length === 0) return null

  const data = Object.entries(dailyStats)
    .map(([date, d]) => ({
      date,
      input: d.tokens.input,
      output: d.tokens.output,
      cache_read: d.tokens.cache_read,
      cache_creation: d.tokens.cache_creation,
    }))
    .sort((a, b) => a.date.localeCompare(b.date))

  return (
    <div className="bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-4 border border-gray-200 dark:border-gray-800">
      <h3 className="text-sm font-medium text-gray-700 dark:text-gray-300 mb-3">Daily Token Usage</h3>
      <ResponsiveContainer width="100%" height={250}>
        <AreaChart data={data}>
          <CartesianGrid strokeDasharray="3 3" stroke="#374151" />
          <XAxis
            dataKey="date"
            tick={{ fontSize: 10, fill: '#9CA3AF' }}
            tickLine={{ stroke: '#4B5563' }}
            axisLine={{ stroke: '#4B5563' }}
          />
          <YAxis
            tick={{ fontSize: 10, fill: '#9CA3AF' }}
            tickLine={{ stroke: '#4B5563' }}
            axisLine={{ stroke: '#4B5563' }}
            tickFormatter={(v) => (v >= 1000 ? `${(v / 1000).toFixed(0)}k` : String(v))}
          />
          <Tooltip
            contentStyle={{
              backgroundColor: '#1F2937',
              border: '1px solid #374151',
              borderRadius: '6px',
              fontSize: '12px',
            }}
            labelStyle={{ color: '#D1D5DB' }}
            itemStyle={{ color: '#D1D5DB' }}
            formatter={(value: number) => [value.toLocaleString(), undefined]}
          />
          <Area
            type="monotone"
            dataKey="input"
            stackId="1"
            stroke="#818CF8"
            fill="#818CF8"
            fillOpacity={0.4}
            name="Input Tokens"
          />
          <Area
            type="monotone"
            dataKey="output"
            stackId="1"
            stroke="#34D399"
            fill="#34D399"
            fillOpacity={0.4}
            name="Output Tokens"
          />
          <Area
            type="monotone"
            dataKey="cache_read"
            stackId="1"
            stroke="#F59E0B"
            fill="#F59E0B"
            fillOpacity={0.4}
            name="Cache Read"
          />
          <Area
            type="monotone"
            dataKey="cache_creation"
            stackId="1"
            stroke="#FB923C"
            fill="#FB923C"
            fillOpacity={0.4}
            name="Cache Creation"
          />
        </AreaChart>
      </ResponsiveContainer>
    </div>
  )
}
