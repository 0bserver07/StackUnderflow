import { memo, useMemo } from 'react'
import { useQuery } from '@tanstack/react-query'
import { useParams } from 'react-router-dom'
import { IconAlertTriangle } from '@tabler/icons-react'
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
import type { CurrencyInfo } from '../../types/api'
import { formatCost, formatModelName, formatNumber } from '../../services/format'
import { useCurrency } from '../../services/currency'
import { ChartCard, EmptyChartCard, useChartTheme, CHART_HEIGHT } from '../charts/chartTheme'

// ---------------------------------------------------------------------------
// ByModelCostChart — audit #6.
//
// The Cost tab already stacks daily spend by *token type* (TokenComposition
// Stack). This is its sibling: a stacked daily-spend chart split by *model*,
// backed by `GET /api/cost-data/by-model`.
//
// API shape (routes/cost.py::get_cost_by_model):
//   { period, currency, models: [
//       { model, total_cost, daily: [{ date, cost_usd, message_count }] } ] }
// `models` is sorted by total_cost desc; cost figures are pre-converted into
// the active currency (route applies the FX rate), so we format with the
// payload's own `currency` block.
//
// Unpriced spend: the route computes cost fresh via `compute_cost`, which
// returns $0 for a model with no rate card (base pricer → all-zero breakdown).
// It does NOT echo the stored `cost_source` flag — so the observable signal
// for an unpriced model is "has assistant traffic but total_cost == 0". Those
// models would have `cost_source == 'unknown'` at the ETL layer. We surface
// them in a small, non-alarming banner rather than letting them silently
// vanish (a $0 bar is invisible).
// ---------------------------------------------------------------------------

interface ByModelDailyPoint {
  date: string
  cost_usd: number
  message_count: number
}

interface ByModelEntry {
  model: string
  total_cost: number
  daily: ByModelDailyPoint[]
}

interface ByModelResponse {
  period: string
  models: ByModelEntry[]
  currency: CurrencyInfo
}

/** Periods accepted by `/api/cost-data/by-model` (same set as by-provider). */
export type ByModelPeriod = 'today' | 'week' | 'month' | 'all'

interface ByModelCostChartProps {
  /** Scopes the server-side rollup. Derive from the Cost tab's range filter. */
  period: ByModelPeriod
}

const TITLE = 'Daily Cost by Model'

// Below this, a model's spend reads as "no rate applied" rather than "tiny but
// real" — `compute_cost` returns exactly 0.0 for an unknown model, so a strict
// epsilon cleanly separates unpriced from priced-but-small.
const EPSILON = 1e-9

// Cap distinct stacked series so a multi-provider store doesn't explode the
// legend; the long tail collapses into one "Other" segment.
const MAX_SERIES = 8
const OTHER_KEY = '__other__'

// Theme-stable categorical palette (mirrors the hues used across the chart
// suite). Hoisted so the memoized chart keeps stable fills across renders.
const MODEL_COLORS = [
  '#818CF8', // indigo
  '#34D399', // emerald
  '#F59E0B', // amber
  '#FB923C', // orange
  '#F472B6', // pink
  '#22D3EE', // cyan
  '#A78BFA', // violet
  '#4ADE80', // green
]
const OTHER_COLOR = '#9CA3AF'

const FLAT_RADIUS: [number, number, number, number] = [0, 0, 0, 0]
const TOP_RADIUS: [number, number, number, number] = [4, 4, 0, 0]

interface SeriesDesc {
  key: string
  label: string
  color: string
}

interface ChartRow {
  date: string
  [model: string]: string | number
}

interface UnpricedModel {
  model: string
  messages: number
}

async function fetchCostByModel(period: ByModelPeriod): Promise<ByModelResponse> {
  // `log_path` is resolved server-side from `deps.current_log_path`, so the
  // project identity is folded into the query key (the `name` route param)
  // rather than sent as a param — same contract as CostTab's fetchCostData.
  const res = await fetch(`/api/cost-data/by-model?period=${encodeURIComponent(period)}`)
  if (!res.ok) {
    const text = await res.text().catch(() => '')
    throw new Error(`${res.status} ${res.statusText}${text ? `: ${text}` : ''}`)
  }
  return res.json() as Promise<ByModelResponse>
}

// ── unpriced banner ──────────────────────────────────────────────────────────

function UnpricedBanner({ models }: { models: UnpricedModel[] }) {
  if (models.length === 0) return null
  const totalMsgs = models.reduce((acc, m) => acc + m.messages, 0)
  const names = models
    .slice(0, 3)
    .map((m) => formatModelName(m.model))
    .join(', ')
  const more = models.length > 3 ? ` +${models.length - 3} more` : ''
  return (
    <div className="mb-3 flex items-start gap-2 rounded-md border border-amber-200 dark:border-amber-900/50 bg-amber-50 dark:bg-amber-900/20 px-3 py-2 text-[11px] text-amber-800 dark:text-amber-300">
      <IconAlertTriangle size={13} className="mt-0.5 shrink-0" />
      <span className="leading-snug">
        <span className="font-semibold">
          {models.length} {models.length === 1 ? 'model' : 'models'} unpriced
        </span>
        {' — '}
        {formatNumber(totalMsgs)} {totalMsgs === 1 ? 'message' : 'messages'} from {names}
        {more} {models.length === 1 ? 'has' : 'have'} no rate card, so {models.length === 1 ? 'its' : 'their'}{' '}
        spend isn&rsquo;t reflected in this chart.
      </span>
    </div>
  )
}

// ── transform ────────────────────────────────────────────────────────────────

interface ChartView {
  rows: ChartRow[]
  series: SeriesDesc[]
  unpriced: UnpricedModel[]
  totalPriced: number
  currency: CurrencyInfo | null
}

function buildView(data: ByModelResponse | undefined): ChartView {
  const models = data?.models ?? []
  const currency = data?.currency ?? null

  const priced: ByModelEntry[] = []
  const unpriced: UnpricedModel[] = []
  for (const m of models) {
    if (m.total_cost > EPSILON) {
      priced.push(m)
    } else {
      const messages = m.daily.reduce((acc, d) => acc + (d.message_count ?? 0), 0)
      if (messages > 0) unpriced.push({ model: m.model, messages })
    }
  }

  // `priced` arrives sorted by total_cost desc from the route. Keep the top
  // MAX_SERIES as their own series; fold the rest into one "Other" segment.
  const head = priced.slice(0, MAX_SERIES)
  const tail = priced.slice(MAX_SERIES)

  const series: SeriesDesc[] = head.map((m, i) => ({
    key: m.model,
    label: formatModelName(m.model),
    color: MODEL_COLORS[i % MODEL_COLORS.length]!,
  }))
  if (tail.length > 0) {
    series.push({ key: OTHER_KEY, label: `Other (${tail.length})`, color: OTHER_COLOR })
  }

  // Pivot (model → daily) into (date → per-model cost) rows for the stack.
  const rowByDate = new Map<string, ChartRow>()
  const rowFor = (date: string): ChartRow => {
    let row = rowByDate.get(date)
    if (!row) {
      row = { date }
      rowByDate.set(date, row)
    }
    return row
  }
  const add = (date: string, key: string, cost: number) => {
    const row = rowFor(date)
    row[key] = ((row[key] as number | undefined) ?? 0) + cost
  }
  for (const m of head) {
    for (const d of m.daily) add(d.date, m.model, d.cost_usd)
  }
  for (const m of tail) {
    for (const d of m.daily) add(d.date, OTHER_KEY, d.cost_usd)
  }

  const rows = Array.from(rowByDate.values()).sort((a, b) => a.date.localeCompare(b.date))
  const totalPriced = priced.reduce((acc, m) => acc + m.total_cost, 0)

  return { rows, series, unpriced, totalPriced, currency }
}

// ── chart ────────────────────────────────────────────────────────────────────

function ByModelCostChart({ period }: ByModelCostChartProps) {
  const palette = useChartTheme()
  const { name } = useParams<{ name: string }>()
  // Active currency code is part of the query key so switching currency
  // refetches the pre-converted figures (the route bakes the FX rate in).
  const { currency: activeCurrency } = useCurrency()

  const { data, isLoading, error } = useQuery({
    queryKey: ['costByModel', name ?? null, period, activeCurrency?.code ?? null],
    queryFn: () => fetchCostByModel(period),
    staleTime: 60_000,
  })

  const view = useMemo(() => buildView(data), [data])
  const { rows, series, unpriced, totalPriced, currency } = view

  const costTickFormatter = useMemo(() => (v: number) => formatCost(v, currency), [currency])
  const costTooltipFormatter = useMemo(
    () => (value: number, nm: string) => [formatCost(value, currency), nm] as [string, string],
    [currency],
  )

  if (isLoading) return <EmptyChartCard title={TITLE} message="Loading…" />
  if (error) {
    return (
      <EmptyChartCard
        title={TITLE}
        message={error instanceof Error ? error.message : 'Failed to load by-model spend'}
      />
    )
  }
  if (rows.length === 0 && unpriced.length === 0) return <EmptyChartCard title={TITLE} />

  const titleAccessory =
    rows.length > 0 ? (
      <span className="ml-2 text-xs font-normal text-gray-400 dark:text-gray-500 tabular-nums">
        {formatCost(totalPriced, currency)}
      </span>
    ) : undefined

  return (
    <ChartCard title={TITLE} titleAccessory={titleAccessory}>
      <UnpricedBanner models={unpriced} />
      {rows.length === 0 ? (
        <div
          className="flex items-center justify-center text-xs text-gray-400 dark:text-gray-600"
          style={{ height: CHART_HEIGHT }}
        >
          No priced model spend in this window
        </div>
      ) : (
        <ResponsiveContainer width="100%" height={CHART_HEIGHT}>
          <BarChart data={rows}>
            <CartesianGrid strokeDasharray="3 3" stroke={palette.grid} />
            <XAxis
              dataKey="date"
              tick={palette.tick}
              tickLine={palette.axisLine}
              axisLine={palette.axisLine}
            />
            <YAxis
              tick={palette.tick}
              tickLine={palette.axisLine}
              axisLine={palette.axisLine}
              tickFormatter={costTickFormatter}
            />
            <Tooltip
              contentStyle={palette.tooltipContent}
              labelStyle={palette.tooltipLabel}
              itemStyle={palette.tooltipItem}
              formatter={costTooltipFormatter}
            />
            <Legend wrapperStyle={palette.legend} />
            {series.map((s, i) => (
              <Bar
                key={s.key}
                dataKey={s.key}
                stackId="model"
                fill={s.color}
                name={s.label}
                radius={i === series.length - 1 ? TOP_RADIUS : FLAT_RADIUS}
                isAnimationActive={false}
              />
            ))}
          </BarChart>
        </ResponsiveContainer>
      )}
    </ChartCard>
  )
}

export default memo(ByModelCostChart)
