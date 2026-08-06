import { memo, useMemo } from 'react'
import {
  BarChart,
  Bar,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
  Cell,
  ReferenceLine,
} from 'recharts'
import { IconArrowsExchange } from '@tabler/icons-react'
import type { CurrencyInfo, WhatIfResponse } from '../../types/api'
import { formatCost, formatModelName, formatTokens } from '../../services/format'
import Badge from '../common/Badge'

// ---------------------------------------------------------------------------
// WhatIfComparison — cross-provider what-if (audit #7p2).
//
// Reprices the project's *actual* token workload against a curated candidate
// set (`GET /api/whatif`). Shows a horizontal bar of each candidate's repriced
// cost with a reference line at what was actually spent, plus a delta table.
// Memoized: the parent BudgetsTab owns the query, so this only re-renders when
// the data or currency identity changes.
// ---------------------------------------------------------------------------

interface WhatIfComparisonProps {
  data: WhatIfResponse
  currency: CurrencyInfo | null
}

interface ChartDatum {
  label: string
  model: string
  cost: number
  isActualModel: boolean
}

const BAR_CHEAPER = '#34D399' // green — cheaper than actual
const BAR_PRICIER = '#F87171' // red — pricier than actual
const ACTUAL_LINE = '#A78BFA' // violet — the actual-spend reference

function DeltaCell({ delta, pct, currency }: { delta: number; pct: number | null; currency: CurrencyInfo | null }) {
  const cheaper = delta < 0
  const sign = cheaper ? '−' : '+'
  const color = cheaper ? 'text-green-600 dark:text-green-400' : 'text-red-600 dark:text-red-400'
  return (
    <span className={`tabular-nums ${color}`}>
      {sign}
      {formatCost(Math.abs(delta), currency)}
      {pct !== null && (
        <span className="text-gray-400 ml-1">
          ({sign}
          {Math.abs(pct).toFixed(0)}%)
        </span>
      )}
    </span>
  )
}

function CandidateTooltip({
  active,
  payload,
  currency,
  actualCost,
}: {
  active?: boolean
  payload?: { payload?: ChartDatum }[]
  currency?: CurrencyInfo | null
  actualCost: number
}) {
  if (!active || !payload || payload.length === 0) return null
  const p = payload[0]?.payload
  if (!p) return null
  const delta = p.cost - actualCost
  const cheaper = delta < 0
  return (
    <div
      style={{
        backgroundColor: '#1F2937',
        border: '1px solid #374151',
        borderRadius: '6px',
        fontSize: '12px',
        padding: '8px 10px',
        color: '#D1D5DB',
      }}
    >
      <div style={{ color: '#F3F4F6', fontWeight: 600, marginBottom: 4 }}>{p.label}</div>
      <div style={{ display: 'flex', justifyContent: 'space-between', gap: 16 }}>
        <span style={{ color: '#9CA3AF' }}>Repriced</span>
        <span style={{ color: '#F3F4F6' }}>{formatCost(p.cost, currency)}</span>
      </div>
      <div style={{ display: 'flex', justifyContent: 'space-between', gap: 16 }}>
        <span style={{ color: '#9CA3AF' }}>vs actual</span>
        <span style={{ color: cheaper ? '#34D399' : '#F87171' }}>
          {cheaper ? '−' : '+'}
          {formatCost(Math.abs(delta), currency)}
        </span>
      </div>
    </div>
  )
}

function WhatIfComparison({ data, currency }: WhatIfComparisonProps) {
  const actualCost = data.actual.cost_usd

  const chartData = useMemo<ChartDatum[]>(() => {
    const actualModels = new Set(data.actual.models)
    return data.candidates.map((c) => ({
      label: c.label,
      model: c.model,
      cost: c.cost_usd,
      isActualModel: actualModels.has(c.model),
    }))
  }, [data])

  const hasWorkload = data.tokens.total > 0

  return (
    <div className="bg-white dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-800 p-4 space-y-4">
      <div className="flex items-center gap-2 flex-wrap">
        <IconArrowsExchange size={16} className="text-indigo-500" />
        <h3 className="text-sm font-semibold text-gray-800 dark:text-gray-200">
          Cross-provider what-if
        </h3>
        <span className="text-xs text-gray-500">
          same {formatTokens(data.tokens.total)} tokens, repriced on each model
        </span>
      </div>

      {!hasWorkload ? (
        <div className="text-xs text-gray-500 py-8 text-center">
          No token usage recorded yet — nothing to reprice.
        </div>
      ) : (
        <>
          <div className="flex items-center justify-between text-xs text-gray-600 dark:text-gray-400 flex-wrap gap-2">
            <span>
              Actually spent{' '}
              <span className="font-semibold text-gray-900 dark:text-gray-100 tabular-nums">
                {formatCost(actualCost, currency)}
              </span>
              {data.actual.models.length > 0 && (
                <span className="text-gray-500">
                  {' '}
                  on {data.actual.models.map(formatModelName).join(', ')}
                </span>
              )}
            </span>
            {data.cheapest && (
              <span className="flex items-center gap-1">
                Cheapest:
                <Badge color="green" size="sm">
                  {data.cheapest.label} · {formatCost(data.cheapest.cost_usd, currency)}
                </Badge>
              </span>
            )}
          </div>

          <ResponsiveContainer width="100%" height={Math.max(220, chartData.length * 30)}>
            <BarChart data={chartData} layout="vertical" margin={{ left: 10, right: 24 }}>
              <CartesianGrid strokeDasharray="3 3" stroke="#374151" horizontal={false} />
              <XAxis
                type="number"
                tick={{ fontSize: 10, fill: '#9CA3AF' }}
                tickLine={{ stroke: '#4B5563' }}
                axisLine={{ stroke: '#4B5563' }}
                tickFormatter={(v: number) => formatCost(v, currency)}
              />
              <YAxis
                type="category"
                dataKey="label"
                tick={{ fontSize: 11, fill: '#9CA3AF' }}
                tickLine={{ stroke: '#4B5563' }}
                axisLine={{ stroke: '#4B5563' }}
                width={150}
                interval={0}
              />
              <Tooltip
                content={<CandidateTooltip currency={currency} actualCost={actualCost} />}
                cursor={{ fill: 'rgba(75, 85, 99, 0.15)' }}
              />
              {actualCost > 0 && (
                <ReferenceLine
                  x={actualCost}
                  stroke={ACTUAL_LINE}
                  strokeDasharray="4 3"
                  label={{
                    value: 'actual',
                    position: 'top',
                    fill: ACTUAL_LINE,
                    fontSize: 10,
                  }}
                />
              )}
              <Bar dataKey="cost" radius={[0, 4, 4, 0]} isAnimationActive={false}>
                {chartData.map((d, index) => (
                  <Cell key={index} fill={d.cost < actualCost ? BAR_CHEAPER : BAR_PRICIER} />
                ))}
              </Bar>
            </BarChart>
          </ResponsiveContainer>

          <div className="overflow-x-auto rounded-lg border border-gray-200 dark:border-gray-800">
            <table className="w-full text-sm">
              <thead className="bg-gray-50 dark:bg-gray-800/60">
                <tr>
                  <th className="px-3 py-2 text-left text-[10px] uppercase tracking-wider text-gray-500 font-medium">
                    Model
                  </th>
                  <th className="px-3 py-2 text-left text-[10px] uppercase tracking-wider text-gray-500 font-medium">
                    Provider
                  </th>
                  <th className="px-3 py-2 text-right text-[10px] uppercase tracking-wider text-gray-500 font-medium">
                    Repriced
                  </th>
                  <th className="px-3 py-2 text-right text-[10px] uppercase tracking-wider text-gray-500 font-medium">
                    vs actual
                  </th>
                </tr>
              </thead>
              <tbody>
                {data.candidates.map((c) => (
                  <tr
                    key={`${c.provider}:${c.model}`}
                    className="border-t border-gray-200 dark:border-gray-800 hover:bg-gray-50 dark:hover:bg-gray-800/40"
                  >
                    <td className="px-3 py-2 text-xs text-gray-800 dark:text-gray-200" title={c.model}>
                      {c.label}
                    </td>
                    <td className="px-3 py-2 text-xs text-gray-500">{c.provider}</td>
                    <td className="px-3 py-2 text-sm tabular-nums text-gray-900 dark:text-gray-100 text-right font-medium">
                      {formatCost(c.cost_usd, currency)}
                    </td>
                    <td className="px-3 py-2 text-xs text-right whitespace-nowrap">
                      <DeltaCell delta={c.delta_usd} pct={c.delta_pct} currency={currency} />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          <p className="text-[11px] text-gray-400 leading-relaxed">
            Token counts are held fixed and repriced at each model's own rate card. A
            different model may tokenize the same work differently or need more/fewer
            output tokens — this is a rate-card swap, not a re-run.
          </p>
        </>
      )}
    </div>
  )
}

export default memo(WhatIfComparison)
