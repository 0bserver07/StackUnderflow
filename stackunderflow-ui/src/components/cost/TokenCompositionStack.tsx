import {
  BarChart,
  Bar,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
  Legend,
} from 'recharts'

interface TokenCompositionStackProps {
  daily: Record<string, Record<string, number>>
}

const SERIES: { key: string; color: string; label: string }[] = [
  { key: 'input', color: '#818CF8', label: 'Input' },
  { key: 'output', color: '#34D399', label: 'Output' },
  { key: 'cache_read', color: '#F59E0B', label: 'Cache Read' },
  { key: 'cache_creation', color: '#FB923C', label: 'Cache Creation' },
]

function formatTokens(v: number): string {
  if (v >= 1_000_000) return `${(v / 1_000_000).toFixed(1)}M`
  if (v >= 1_000) return `${(v / 1_000).toFixed(0)}K`
  return String(v)
}

export default function TokenCompositionStack({ daily }: TokenCompositionStackProps) {
  if (!daily || Object.keys(daily).length === 0) {
    return (
      <div className="bg-gray-800/50 rounded-lg p-4 border border-gray-800">
        <h3 className="text-sm font-medium text-gray-300 mb-3">Daily Token Composition</h3>
        <div className="text-xs text-gray-500 py-8 text-center">No daily token data yet</div>
      </div>
    )
  }

  const data = Object.entries(daily)
    .map(([date, bucket]) => ({
      date,
      input: bucket.input ?? 0,
      output: bucket.output ?? 0,
      cache_read: bucket.cache_read ?? 0,
      cache_creation: bucket.cache_creation ?? 0,
    }))
    .sort((a, b) => a.date.localeCompare(b.date))

  return (
    <div className="bg-gray-800/50 rounded-lg p-4 border border-gray-800">
      <h3 className="text-sm font-medium text-gray-300 mb-3">Daily Token Composition</h3>
      <ResponsiveContainer width="100%" height={260}>
        <BarChart data={data}>
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
            tickFormatter={formatTokens}
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
              const series = SERIES.find((s) => s.key === name)
              return [value.toLocaleString(), series?.label ?? name]
            }}
          />
          <Legend
            wrapperStyle={{ fontSize: '11px', color: '#9CA3AF' }}
            formatter={(value: string) => {
              const series = SERIES.find((s) => s.key === value)
              return series?.label ?? value
            }}
          />
          {SERIES.map((s, idx) => (
            <Bar
              key={s.key}
              dataKey={s.key}
              stackId="tokens"
              fill={s.color}
              name={s.key}
              radius={idx === SERIES.length - 1 ? [4, 4, 0, 0] : [0, 0, 0, 0]}
            />
          ))}
        </BarChart>
      </ResponsiveContainer>
    </div>
  )
}
