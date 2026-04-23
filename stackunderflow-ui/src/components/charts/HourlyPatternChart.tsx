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
import type { HourlyPattern } from '../../types/api'

interface HourlyPatternChartProps {
  hourlyPattern: HourlyPattern
}

export default function HourlyPatternChart({ hourlyPattern }: HourlyPatternChartProps) {
  if (!hourlyPattern?.tokens || Object.keys(hourlyPattern.tokens).length === 0) return null

  // Build data for all 24 hours
  const data = Array.from({ length: 24 }, (_, i) => {
    const hourKey = String(i)
    const tokenData = hourlyPattern.tokens[hourKey]
    return {
      hour: i,
      label: `${i}:00`,
      input: tokenData?.input ?? 0,
      output: tokenData?.output ?? 0,
      messages: hourlyPattern.messages?.[hourKey] ?? 0,
    }
  })

  return (
    <div className="bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-4 border border-gray-200 dark:border-gray-800">
      <h3 className="text-sm font-medium text-gray-700 dark:text-gray-300 mb-3">Hourly Token Pattern</h3>
      <ResponsiveContainer width="100%" height={250}>
        <ComposedChart data={data}>
          <CartesianGrid strokeDasharray="3 3" stroke="#374151" />
          <XAxis
            dataKey="label"
            tick={{ fontSize: 10, fill: '#9CA3AF' }}
            tickLine={{ stroke: '#4B5563' }}
            axisLine={{ stroke: '#4B5563' }}
            interval={2}
          />
          <YAxis
            yAxisId="tokens"
            tick={{ fontSize: 10, fill: '#9CA3AF' }}
            tickLine={{ stroke: '#4B5563' }}
            axisLine={{ stroke: '#4B5563' }}
            tickFormatter={(v) => (v >= 1000000 ? `${(v / 1000000).toFixed(1)}M` : v >= 1000 ? `${(v / 1000).toFixed(0)}k` : String(v))}
          />
          <YAxis
            yAxisId="messages"
            orientation="right"
            tick={{ fontSize: 10, fill: '#6B7280' }}
            tickLine={{ stroke: '#4B5563' }}
            axisLine={{ stroke: '#4B5563' }}
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
              const labels: Record<string, string> = {
                input: 'Input Tokens',
                output: 'Output Tokens',
                messages: 'Messages',
              }
              return [value.toLocaleString(), labels[name] ?? name]
            }}
          />
          <Legend
            wrapperStyle={{ fontSize: '11px', color: '#9CA3AF' }}
            formatter={(value: string) => {
              const labels: Record<string, string> = {
                input: 'Input Tokens',
                output: 'Output Tokens',
                messages: 'Messages',
              }
              return labels[value] ?? value
            }}
          />
          <Bar
            yAxisId="tokens"
            dataKey="input"
            stackId="tokens"
            fill="#818CF8"
            fillOpacity={0.8}
            radius={[0, 0, 0, 0]}
            name="input"
          />
          <Bar
            yAxisId="tokens"
            dataKey="output"
            stackId="tokens"
            fill="#34D399"
            fillOpacity={0.8}
            radius={[2, 2, 0, 0]}
            name="output"
          />
          <Line
            yAxisId="messages"
            type="monotone"
            dataKey="messages"
            stroke="#9CA3AF"
            strokeWidth={1.5}
            strokeDasharray="4 2"
            dot={false}
            name="messages"
          />
        </ComposedChart>
      </ResponsiveContainer>
    </div>
  )
}
