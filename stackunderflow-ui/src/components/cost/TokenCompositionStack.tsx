import { useMemo, useState } from 'react'
import {
  BarChart,
  Bar,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
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

type RangeKey = '7d' | '30d' | 'all'

const RANGES: { key: RangeKey; label: string }[] = [
  { key: '7d', label: '7d' },
  { key: '30d', label: '30d' },
  { key: 'all', label: 'All' },
]

function formatTokens(v: number): string {
  if (v >= 1_000_000) return `${(v / 1_000_000).toFixed(1)}M`
  if (v >= 1_000) return `${(v / 1_000).toFixed(0)}K`
  return String(v)
}

interface DailyRow {
  date: string
  input: number
  output: number
  cache_read: number
  cache_creation: number
}

interface RichTooltipProps {
  active?: boolean
  label?: string
  payload?: Array<{ dataKey?: string; value?: number; payload?: DailyRow }>
}

function RichTooltip({ active, label, payload }: RichTooltipProps) {
  if (!active || !payload || payload.length === 0) return null
  const row = payload[0]?.payload
  if (!row) return null
  const total =
    (row.input ?? 0) +
    (row.output ?? 0) +
    (row.cache_read ?? 0) +
    (row.cache_creation ?? 0)

  return (
    <div
      className="bg-gray-50 dark:bg-gray-900 border border-gray-300 dark:border-gray-700 rounded-md p-2.5 text-xs shadow-lg"
      data-testid="token-stack-tooltip"
    >
      <div className="text-gray-800 dark:text-gray-200 font-medium mb-1.5">{label}</div>
      <div className="space-y-1">
        {SERIES.map((s) => {
          const value = (row[s.key as keyof DailyRow] as number) ?? 0
          const pct = total > 0 ? (value / total) * 100 : 0
          return (
            <div key={s.key} className="flex items-center gap-2">
              <span
                className="inline-block w-2.5 h-2.5 rounded-sm"
                style={{ backgroundColor: s.color }}
              />
              <span className="text-gray-600 dark:text-gray-400 flex-1">{s.label}</span>
              <span className="text-gray-800 dark:text-gray-200 tabular-nums">
                {value.toLocaleString()}
              </span>
              <span className="text-gray-500 tabular-nums w-10 text-right">
                {pct.toFixed(1)}%
              </span>
            </div>
          )
        })}
      </div>
      <div className="mt-1.5 pt-1.5 border-t border-gray-300 dark:border-gray-700 flex items-center justify-between">
        <span className="text-gray-600 dark:text-gray-400">Total</span>
        <span className="text-gray-900 dark:text-gray-100 font-medium tabular-nums">
          {total.toLocaleString()}
        </span>
      </div>
    </div>
  )
}

export default function TokenCompositionStack({ daily }: TokenCompositionStackProps) {
  const [range, setRange] = useState<RangeKey>('all')
  // isolated series key — when set, only that series renders
  const [isolated, setIsolated] = useState<string | null>(null)

  const allData: DailyRow[] = useMemo(() => {
    if (!daily) return []
    return Object.entries(daily)
      .map(([date, bucket]) => ({
        date,
        input: bucket.input ?? 0,
        output: bucket.output ?? 0,
        cache_read: bucket.cache_read ?? 0,
        cache_creation: bucket.cache_creation ?? 0,
      }))
      .sort((a, b) => a.date.localeCompare(b.date))
  }, [daily])

  const filteredData: DailyRow[] = useMemo(() => {
    if (range === 'all') return allData
    const take = range === '7d' ? 7 : 30
    return allData.slice(-take)
  }, [allData, range])

  if (!daily || Object.keys(daily).length === 0) {
    return (
      <div
        className="bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-4 border border-gray-200 dark:border-gray-800"
        data-testid="token-composition-stack"
      >
        <h3 className="text-sm font-medium text-gray-700 dark:text-gray-300 mb-3">Daily Token Composition</h3>
        <div className="text-xs text-gray-500 py-8 text-center">No daily token data yet</div>
      </div>
    )
  }

  const visibleSeries = isolated ? SERIES.filter((s) => s.key === isolated) : SERIES

  const toggleIsolate = (key: string) => {
    setIsolated((cur) => (cur === key ? null : key))
  }

  return (
    <div
      className="bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-4 border border-gray-200 dark:border-gray-800"
      data-testid="token-composition-stack"
    >
      <div className="flex items-center justify-between mb-3 gap-2 flex-wrap">
        <h3 className="text-sm font-medium text-gray-700 dark:text-gray-300">Daily Token Composition</h3>
        <div
          className="flex items-center gap-1"
          role="group"
          aria-label="Date range filter"
          data-testid="token-stack-range"
        >
          {RANGES.map((r) => {
            const active = range === r.key
            return (
              <button
                key={r.key}
                type="button"
                onClick={() => setRange(r.key)}
                data-testid={`token-stack-range-${r.key}`}
                aria-pressed={active}
                className={`inline-flex items-center px-2 py-0.5 text-[11px] font-medium rounded-full border transition-colors ${
                  active
                    ? 'bg-indigo-100 text-indigo-800 border-indigo-300 dark:bg-indigo-900/50 dark:text-indigo-200 dark:border-indigo-700'
                    : 'bg-gray-100/90 dark:bg-gray-800/80 text-gray-600 dark:text-gray-400 border-gray-300 dark:border-gray-700 hover:text-gray-800 dark:hover:text-gray-200 hover:border-gray-400 dark:hover:border-gray-600'
                }`}
              >
                {r.label}
              </button>
            )
          })}
        </div>
      </div>

      {filteredData.length === 0 ? (
        <div className="text-xs text-gray-500 py-8 text-center">
          No data in this range
        </div>
      ) : (
        <ResponsiveContainer width="100%" height={260}>
          <BarChart data={filteredData}>
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
              cursor={{ fill: 'rgba(99, 102, 241, 0.08)' }}
              content={<RichTooltip />}
            />
            {visibleSeries.map((s, idx) => (
              <Bar
                key={s.key}
                dataKey={s.key}
                stackId="tokens"
                fill={s.color}
                name={s.key}
                radius={idx === visibleSeries.length - 1 ? [4, 4, 0, 0] : [0, 0, 0, 0]}
              />
            ))}
          </BarChart>
        </ResponsiveContainer>
      )}

      <div
        className="mt-3 flex items-center justify-center gap-3 flex-wrap"
        role="group"
        aria-label="Series legend — click to isolate"
        data-testid="token-stack-legend"
      >
        {SERIES.map((s) => {
          const dim = isolated !== null && isolated !== s.key
          return (
            <button
              key={s.key}
              type="button"
              onClick={() => toggleIsolate(s.key)}
              data-testid={`token-stack-legend-${s.key}`}
              aria-pressed={isolated === s.key}
              title={
                isolated === s.key
                  ? 'Click to show all series'
                  : `Click to isolate ${s.label}`
              }
              className={`inline-flex items-center gap-1.5 text-[11px] transition-opacity ${
                dim ? 'opacity-40 hover:opacity-70' : 'opacity-100'
              } text-gray-600 dark:text-gray-400 hover:text-gray-800 dark:hover:text-gray-200`}
            >
              <span
                className="inline-block w-2.5 h-2.5 rounded-sm"
                style={{ backgroundColor: s.color }}
              />
              <span>{s.label}</span>
            </button>
          )
        })}
        {isolated !== null && (
          <button
            type="button"
            onClick={() => setIsolated(null)}
            data-testid="token-stack-legend-reset"
            className="text-[11px] text-indigo-700 dark:text-indigo-300 hover:text-indigo-800 dark:hover:text-indigo-200 underline underline-offset-2"
          >
            reset
          </button>
        )}
      </div>
    </div>
  )
}
