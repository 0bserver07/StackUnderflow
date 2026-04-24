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

interface CommandToolDistChartProps {
  toolCountDist: Record<string, number>
}

const COLORS = ['#818CF8', '#34D399', '#F59E0B', '#F87171', '#A78BFA', '#38BDF8']

export default function CommandToolDistChart({ toolCountDist }: CommandToolDistChartProps) {
  if (!toolCountDist || Object.keys(toolCountDist).length === 0) return null

  // Bucket raw distribution into 0, 1, 2, 3, 4, 5+
  const buckets: Record<string, number> = { '0': 0, '1': 0, '2': 0, '3': 0, '4': 0, '5+': 0 }

  for (const [toolCount, cmdCount] of Object.entries(toolCountDist)) {
    const n = parseInt(toolCount, 10)
    if (isNaN(n)) continue
    if (n >= 5) {
      buckets['5+'] = (buckets['5+'] ?? 0) + cmdCount
    } else {
      const key = String(n)
      buckets[key] = (buckets[key] ?? 0) + cmdCount
    }
  }

  const total = Object.values(buckets).reduce((s, v) => s + v, 0)
  if (total === 0) return null

  const data = Object.entries(buckets).map(([tools, count]) => ({
    tools,
    count,
    percentage: parseFloat(((count / total) * 100).toFixed(1)),
  }))

  return (
    <div className="bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-4 border border-gray-200 dark:border-gray-800">
      <h3 className="text-sm font-medium text-gray-700 dark:text-gray-300 mb-3">Commands by Tool Count</h3>
      <ResponsiveContainer width="100%" height={250}>
        <BarChart data={data}>
          <CartesianGrid strokeDasharray="3 3" stroke="#374151" />
          <XAxis
            dataKey="tools"
            tick={{ fontSize: 10, fill: '#9CA3AF' }}
            tickLine={{ stroke: '#4B5563' }}
            axisLine={{ stroke: '#4B5563' }}
            label={{
              value: 'Tools Used',
              position: 'insideBottom',
              offset: -2,
              style: { fontSize: 10, fill: '#6B7280' },
            }}
          />
          <YAxis
            tick={{ fontSize: 10, fill: '#9CA3AF' }}
            tickLine={{ stroke: '#4B5563' }}
            axisLine={{ stroke: '#4B5563' }}
            tickFormatter={(v) => `${v}%`}
            domain={[0, 'auto']}
          />
          <Tooltip
            contentStyle={{
              backgroundColor: '#1F2937',
              border: '1px solid #374151',
              borderRadius: '6px',
              fontSize: '12px',
            }}
            labelStyle={{ color: '#D1D5DB' }}
            labelFormatter={(label) => `${label} tool${label === '1' ? '' : 's'}`}
            // eslint-disable-next-line @typescript-eslint/no-explicit-any
            formatter={(_value: number, _name: string, props: any) => {
              const { percentage, count } = props?.payload ?? {}
              return [`${percentage ?? _value}% (${(count ?? 0).toLocaleString()} commands)`, 'Share']
            }}
          />
          <Bar dataKey="percentage" radius={[4, 4, 0, 0]}>
            {data.map((_entry, index) => (
              <Cell key={index} fill={COLORS[index % COLORS.length]} />
            ))}
          </Bar>
        </BarChart>
      </ResponsiveContainer>
    </div>
  )
}
