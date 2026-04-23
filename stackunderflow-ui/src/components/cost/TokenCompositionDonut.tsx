import {
  PieChart,
  Pie,
  Cell,
  Tooltip,
  ResponsiveContainer,
  Legend,
} from 'recharts'

interface TokenCompositionDonutProps {
  totals: Record<string, number>
}

const SERIES: { key: string; color: string; label: string }[] = [
  { key: 'input', color: '#818CF8', label: 'Input' },
  { key: 'output', color: '#34D399', label: 'Output' },
  { key: 'cache_read', color: '#F59E0B', label: 'Cache Read' },
  { key: 'cache_creation', color: '#FB923C', label: 'Cache Creation' },
]

function formatTokens(v: number): string {
  if (v >= 1_000_000) return `${(v / 1_000_000).toFixed(1)}M`
  if (v >= 1_000) return `${(v / 1_000).toFixed(1)}K`
  return v.toLocaleString()
}

export default function TokenCompositionDonut({ totals }: TokenCompositionDonutProps) {
  const grandTotal = SERIES.reduce((sum, s) => sum + (totals?.[s.key] ?? 0), 0)

  if (!totals || grandTotal === 0) {
    return (
      <div className="bg-gray-800/50 rounded-lg p-4 border border-gray-800">
        <h3 className="text-sm font-medium text-gray-300 mb-3">Token Composition</h3>
        <div className="text-xs text-gray-500 py-8 text-center">No token data yet</div>
      </div>
    )
  }

  const data = SERIES
    .map((s) => ({
      name: s.key,
      label: s.label,
      color: s.color,
      value: totals[s.key] ?? 0,
    }))
    .filter((d) => d.value > 0)

  return (
    <div className="bg-gray-800/50 rounded-lg p-4 border border-gray-800">
      <h3 className="text-sm font-medium text-gray-300 mb-3">
        Token Composition
        <span className="ml-2 text-xs text-gray-500 font-normal">{formatTokens(grandTotal)} total</span>
      </h3>
      <ResponsiveContainer width="100%" height={260}>
        <PieChart>
          <Pie
            data={data}
            cx="50%"
            cy="50%"
            innerRadius={60}
            outerRadius={95}
            paddingAngle={2}
            dataKey="value"
            nameKey="label"
          >
            {data.map((entry, index) => (
              <Cell key={index} fill={entry.color} />
            ))}
          </Pie>
          <Tooltip
            contentStyle={{
              backgroundColor: '#1F2937',
              border: '1px solid #374151',
              borderRadius: '6px',
              fontSize: '12px',
            }}
            labelStyle={{ color: '#D1D5DB' }}
            formatter={(value: number) => {
              const pct = grandTotal > 0 ? (value / grandTotal) * 100 : 0
              return [`${value.toLocaleString()} (${pct.toFixed(1)}%)`, 'Tokens']
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
