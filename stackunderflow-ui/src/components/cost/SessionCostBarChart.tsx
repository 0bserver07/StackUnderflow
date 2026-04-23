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
import type { SessionCost } from '../../types/api'

interface SessionCostBarChartProps {
  data: SessionCost[]
  onSelect?: (sessionId: string) => void
}

const COLORS = [
  '#818CF8', '#34D399', '#F59E0B', '#F87171', '#A78BFA',
  '#38BDF8', '#FB923C', '#E879F9', '#2DD4BF', '#FCD34D',
]

function shortSession(sid: string): string {
  // Session ids are long uuids — show first 8 chars
  return sid.length > 8 ? sid.slice(0, 8) : sid
}

function formatCost(cost: number): string {
  if (cost >= 100) return `$${cost.toFixed(0)}`
  if (cost >= 1) return `$${cost.toFixed(2)}`
  return `$${cost.toFixed(4)}`
}

export default function SessionCostBarChart({ data, onSelect }: SessionCostBarChartProps) {
  if (!data || data.length === 0) {
    return (
      <div className="bg-gray-800/50 rounded-lg p-4 border border-gray-800">
        <h3 className="text-sm font-medium text-gray-300 mb-3">Top Sessions by Cost</h3>
        <div className="text-xs text-gray-500 py-8 text-center">No session cost data yet</div>
      </div>
    )
  }

  const chartData = [...data]
    .sort((a, b) => b.cost - a.cost)
    .slice(0, 10)
    .map((s) => ({
      session_id: s.session_id,
      short_id: shortSession(s.session_id),
      cost: s.cost,
      commands: s.commands,
      errors: s.errors,
      preview: s.first_prompt_preview,
    }))

  return (
    <div className="bg-gray-800/50 rounded-lg p-4 border border-gray-800">
      <h3 className="text-sm font-medium text-gray-300 mb-3">
        Top Sessions by Cost
        <span className="ml-2 text-xs text-gray-500 font-normal">top {chartData.length}</span>
      </h3>
      <ResponsiveContainer width="100%" height={Math.max(260, chartData.length * 32)}>
        <BarChart data={chartData} layout="vertical" margin={{ left: 10, right: 20 }}>
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
            dataKey="short_id"
            tick={{ fontSize: 10, fill: '#9CA3AF', fontFamily: 'monospace' }}
            tickLine={{ stroke: '#4B5563' }}
            axisLine={{ stroke: '#4B5563' }}
            width={80}
          />
          <Tooltip
            contentStyle={{
              backgroundColor: '#1F2937',
              border: '1px solid #374151',
              borderRadius: '6px',
              fontSize: '12px',
              maxWidth: 360,
            }}
            labelStyle={{ color: '#D1D5DB' }}
            // eslint-disable-next-line @typescript-eslint/no-explicit-any
            labelFormatter={(_label, payload: any) => {
              const p = payload?.[0]?.payload
              if (!p) return _label
              return `${p.short_id} · ${p.commands} cmds · ${p.errors} errs`
            }}
            // eslint-disable-next-line @typescript-eslint/no-explicit-any
            formatter={(value: number, _name: string, props: any) => {
              const preview = props?.payload?.preview ?? ''
              const truncated = preview.length > 100 ? preview.slice(0, 100) + '…' : preview
              return [formatCost(value), truncated || 'Cost']
            }}
          />
          <Bar
            dataKey="cost"
            radius={[0, 4, 4, 0]}
            cursor={onSelect ? 'pointer' : undefined}
            onClick={(e: { session_id?: string }) => {
              if (onSelect && e?.session_id) onSelect(e.session_id)
            }}
          >
            {chartData.map((_entry, index) => (
              <Cell key={index} fill={COLORS[index % COLORS.length]} />
            ))}
          </Bar>
        </BarChart>
      </ResponsiveContainer>
    </div>
  )
}
