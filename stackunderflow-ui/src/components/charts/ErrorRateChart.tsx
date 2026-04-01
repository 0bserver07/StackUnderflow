import {
  ComposedChart,
  Bar,
  Line,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
  Legend,
} from 'recharts'
import type { DailyData } from '../../types/api'

interface ErrorRateChartProps {
  dailyStats: Record<string, DailyData>
}

function formatNumber(v: number): string {
  if (v >= 1_000_000) return `${(v / 1_000_000).toFixed(1)}M`
  if (v >= 1_000) return `${(v / 1_000).toFixed(0)}K`
  return String(v)
}

export default function ErrorRateChart({ dailyStats }: ErrorRateChartProps) {
  if (!dailyStats || Object.keys(dailyStats).length === 0) return null

  const data = Object.entries(dailyStats)
    .map(([date, d]) => ({
      date,
      error_rate: parseFloat((d.error_rate * 100).toFixed(1)),
      errors: d.errors,
      messages: d.messages,
    }))
    .sort((a, b) => a.date.localeCompare(b.date))

  if (data.length === 0) return null

  return (
    <div className="bg-gray-800/50 rounded-lg p-4 border border-gray-800">
      <h3 className="text-sm font-medium text-gray-300 mb-3">Error Rate Over Time</h3>
      <ResponsiveContainer width="100%" height={280}>
        <ComposedChart data={data}>
          <CartesianGrid strokeDasharray="3 3" stroke="#374151" />
          <XAxis
            dataKey="date"
            tick={{ fontSize: 10, fill: '#9CA3AF' }}
            tickLine={{ stroke: '#4B5563' }}
            axisLine={{ stroke: '#4B5563' }}
          />
          <YAxis
            yAxisId="left"
            tick={{ fontSize: 10, fill: '#9CA3AF' }}
            tickLine={{ stroke: '#4B5563' }}
            axisLine={{ stroke: '#4B5563' }}
            tickFormatter={(v) => `${v}%`}
            domain={[0, 'auto']}
          />
          <YAxis
            yAxisId="right"
            orientation="right"
            tick={{ fontSize: 10, fill: '#9CA3AF' }}
            tickLine={{ stroke: '#4B5563' }}
            axisLine={{ stroke: '#4B5563' }}
            tickFormatter={formatNumber}
          />
          <Tooltip
            contentStyle={{
              backgroundColor: '#1F2937',
              border: '1px solid #374151',
              borderRadius: '6px',
              fontSize: '12px',
            }}
            labelStyle={{ color: '#D1D5DB' }}
            formatter={(value: number, name: string) => {
              if (name === 'Error Rate') return [`${value}%`, name]
              return [value.toLocaleString(), name]
            }}
          />
          <Legend
            wrapperStyle={{ fontSize: '11px' }}
            formatter={(value: string) => <span className="text-gray-400">{value}</span>}
          />
          <Bar
            yAxisId="right"
            dataKey="messages"
            name="Messages"
            fill="#818CF8"
            fillOpacity={0.4}
            radius={[2, 2, 0, 0]}
          />
          <Bar
            yAxisId="right"
            dataKey="errors"
            name="Errors"
            fill="#F87171"
            fillOpacity={0.7}
            radius={[2, 2, 0, 0]}
          />
          <Line
            yAxisId="left"
            type="monotone"
            dataKey="error_rate"
            name="Error Rate"
            stroke="#F59E0B"
            strokeWidth={2}
            dot={{ r: 3, fill: '#F59E0B' }}
            activeDot={{ r: 5 }}
          />
        </ComposedChart>
      </ResponsiveContainer>
    </div>
  )
}
