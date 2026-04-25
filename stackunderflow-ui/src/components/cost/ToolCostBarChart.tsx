import { useMemo, useState } from 'react'
import {
  BarChart,
  Bar,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
  Cell,
  LabelList,
} from 'recharts'
import type { ToolCost } from '../../types/api'

type SortKey = 'cost' | 'calls' | 'tokens'

interface ToolCostBarChartProps {
  data: Record<string, ToolCost>
  /**
   * Optional click handler. When omitted, a `stackunderflow:filter-tool`
   * CustomEvent is still dispatched on `window` so parent tabs can listen.
   */
  onToolClick?: (name: string) => void
}

interface ChartRow {
  name: string
  cost: number
  calls: number
  tokens: number
  input: number
  output: number
  cacheRead: number
  cacheCreation: number
  pctOfTotal: number
  label: string
}

const COLORS = [
  '#818CF8', '#34D399', '#F59E0B', '#F87171', '#A78BFA',
  '#38BDF8', '#FB923C', '#E879F9', '#2DD4BF', '#FCD34D',
]

const SORTS: { key: SortKey; label: string }[] = [
  { key: 'cost', label: 'Cost' },
  { key: 'calls', label: 'Calls' },
  { key: 'tokens', label: 'Tokens' },
]

import { formatCost } from '../../services/format'

function formatShort(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`
  return String(n)
}

function formatPct(p: number): string {
  if (!isFinite(p) || p <= 0) return '—'
  if (p < 0.1) return '<0.1%'
  if (p < 10) return `${p.toFixed(1)}%`
  return `${p.toFixed(0)}%`
}

export default function ToolCostBarChart({ data, onToolClick }: ToolCostBarChartProps) {
  const [sortKey, setSortKey] = useState<SortKey>('cost')

  const chartData = useMemo<ChartRow[]>(() => {
    if (!data) return []
    const rows = Object.entries(data)
      .map(([name, stat]) => {
        const input = stat.input_tokens ?? 0
        const output = stat.output_tokens ?? 0
        const cacheRead = stat.cache_read_tokens ?? 0
        const cacheCreation = stat.cache_creation_tokens ?? 0
        return {
          name,
          cost: stat.cost ?? 0,
          calls: stat.calls ?? 0,
          tokens: input + output + cacheRead + cacheCreation,
          input,
          output,
          cacheRead,
          cacheCreation,
        }
      })
      .filter((d) => d.cost > 0 || d.calls > 0 || d.tokens > 0)

    const totalForSort = rows.reduce((s, r) => s + (r[sortKey] || 0), 0)
    const sorted = [...rows]
      .sort((a, b) => (b[sortKey] || 0) - (a[sortKey] || 0))
      .slice(0, 12)

    return sorted.map<ChartRow>((r) => {
      const raw = r[sortKey] || 0
      const pct = totalForSort > 0 ? (raw / totalForSort) * 100 : 0
      const primary = sortKey === 'cost' ? formatCost(r.cost) : formatShort(raw)
      return {
        ...r,
        pctOfTotal: pct,
        label: `${primary} · ${formatPct(pct)}`,
      }
    })
  }, [data, sortKey])

  const hasData = chartData.length > 0

  const headerTitle = `Tools by ${SORTS.find((s) => s.key === sortKey)?.label ?? 'Cost'}`

  const header = (
    <div className="flex items-center justify-between mb-3 gap-3">
      <h3 className="text-sm font-medium text-gray-700 dark:text-gray-300">{headerTitle}</h3>
      <div
        role="tablist"
        aria-label="Sort tools"
        className="inline-flex items-center rounded-md border border-gray-300 dark:border-gray-700 bg-gray-50/80 dark:bg-gray-900/60 p-0.5 text-[11px]"
      >
        {SORTS.map((s) => {
          const active = s.key === sortKey
          return (
            <button
              key={s.key}
              role="tab"
              aria-selected={active}
              type="button"
              onClick={() => setSortKey(s.key)}
              className={[
                'px-2.5 py-1 rounded transition-colors',
                active
                  ? 'bg-indigo-500/25 text-indigo-200'
                  : 'text-gray-600 dark:text-gray-400 hover:text-gray-800 dark:hover:text-gray-200',
              ].join(' ')}
            >
              {s.label}
            </button>
          )
        })}
      </div>
    </div>
  )

  if (!data || Object.keys(data).length === 0 || !hasData) {
    return (
      <div className="bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-4 border border-gray-200 dark:border-gray-800">
        {header}
        <div className="text-xs text-gray-500 py-8 text-center">No tool cost data yet</div>
      </div>
    )
  }

  const dataKey: SortKey = sortKey
  const maxLabelLen = Math.max(...chartData.map((d) => d.name.length))
  const leftMargin = Math.min(maxLabelLen * 6, 160)

  const handleBarClick = (payload: unknown) => {
    // Recharts may pass the row object directly or wrapped in a payload prop.
    let name: string | undefined
    if (payload && typeof payload === 'object') {
      const p = payload as { name?: string; payload?: { name?: string } }
      name = p.name ?? p.payload?.name
    }
    if (!name) return
    if (onToolClick) {
      onToolClick(name)
    }
    if (typeof window !== 'undefined' && typeof CustomEvent !== 'undefined') {
      window.dispatchEvent(
        new CustomEvent('stackunderflow:filter-tool', { detail: { tool: name } }),
      )
    }
  }

  const xTickFormatter =
    sortKey === 'cost' ? (v: number) => formatCost(v) : (v: number) => formatShort(v)

  const sortLabelLower = (SORTS.find((s) => s.key === sortKey)?.label ?? 'cost').toLowerCase()

  return (
    <div className="bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-4 border border-gray-200 dark:border-gray-800">
      {header}
      <ResponsiveContainer width="100%" height={Math.max(260, chartData.length * 32)}>
        <BarChart data={chartData} layout="vertical" margin={{ left: 20, right: 88 }}>
          <CartesianGrid strokeDasharray="3 3" stroke="#374151" horizontal={false} />
          <XAxis
            type="number"
            tick={{ fontSize: 10, fill: '#9CA3AF' }}
            tickLine={{ stroke: '#4B5563' }}
            axisLine={{ stroke: '#4B5563' }}
            tickFormatter={xTickFormatter}
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
            cursor={{ fill: 'rgba(129, 140, 248, 0.08)' }}
            contentStyle={{
              backgroundColor: '#1F2937',
              border: '1px solid #374151',
              borderRadius: '6px',
              fontSize: '12px',
              whiteSpace: 'pre-line',
            }}
            labelStyle={{ color: '#D1D5DB' }}
            // eslint-disable-next-line @typescript-eslint/no-explicit-any
            formatter={(_value: number, _name: string, props: any) => {
              const p = props?.payload as ChartRow | undefined
              if (!p) return ['', '']
              const lines = [
                `${formatCost(p.cost)} · ${formatPct(p.pctOfTotal)} of ${sortLabelLower}`,
                `calls: ${formatShort(p.calls)}`,
                `input: ${formatShort(p.input)} · output: ${formatShort(p.output)}`,
                `cache read: ${formatShort(p.cacheRead)} · cache creation: ${formatShort(p.cacheCreation)}`,
              ]
              return [lines.join('\n'), 'Details']
            }}
          />
          <Bar
            dataKey={dataKey}
            radius={[0, 4, 4, 0]}
            cursor="pointer"
            onClick={handleBarClick}
          >
            {chartData.map((_entry, index) => (
              <Cell key={index} fill={COLORS[index % COLORS.length]} />
            ))}
            <LabelList
              dataKey="label"
              position="right"
              style={{ fill: '#D1D5DB', fontSize: 10 }}
            />
          </Bar>
        </BarChart>
      </ResponsiveContainer>
      <p className="mt-2 text-[10px] text-gray-500">
        Click a bar to filter by that tool. % is share of total {sortLabelLower} across all tools.
      </p>
    </div>
  )
}
