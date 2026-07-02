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
import type { SessionCost } from '../../types/api'
import { openSession } from '../../services/navigation'
import { useChartTheme } from '../charts/chartTheme'

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

import { formatCost, formatModelName, formatTokens } from '../../services/format'
import { useCurrency } from '../../services/currency'
import type { CurrencyInfo } from '../../types/api'

function formatDuration(totalSeconds: number): string {
  if (!Number.isFinite(totalSeconds) || totalSeconds < 0) return '0:00:00'
  const s = Math.floor(totalSeconds)
  const h = Math.floor(s / 3600)
  const m = Math.floor((s % 3600) / 60)
  const sec = s % 60
  return `${h}:${m.toString().padStart(2, '0')}:${sec.toString().padStart(2, '0')}`
}

interface ChartDatum {
  session_id: string
  short_id: string
  /** Pre-formatted Y-axis label: "534bba1f · refactor the auth …" */
  label: string
  cost: number
  commands: number
  errors: number
  messages: number
  duration_s: number
  models_used: string[]
  tokens: Record<string, number>
  preview: string
}

interface TooltipPayloadEntry {
  payload?: ChartDatum
}

// #59: the tooltip used hardcoded dark inline styles — Tailwind `dark:`
// variants let the same markup follow the active theme.
function TooltipRow({ label, value, valueClass }: { label: string; value: string; valueClass?: string }) {
  return (
    <div className="flex justify-between gap-3">
      <span className="text-gray-500 dark:text-gray-400">{label}</span>
      <span className={valueClass ?? 'text-gray-900 dark:text-gray-100'}>{value}</span>
    </div>
  )
}

function SessionTooltip({
  active,
  payload,
  currency,
}: {
  active?: boolean
  payload?: TooltipPayloadEntry[]
  currency?: CurrencyInfo | null
}) {
  if (!active || !payload || payload.length === 0) return null
  const p = payload[0]?.payload
  if (!p) return null

  const tokens = p.tokens ?? {}
  const input = Number(tokens.input ?? 0)
  const output = Number(tokens.output ?? 0)
  const cacheRead = Number(tokens.cache_read ?? 0)
  const cacheCreation = Number(tokens.cache_creation ?? 0)

  const preview = p.preview || ''
  const truncated = preview.length > 140 ? preview.slice(0, 140) + '…' : preview

  return (
    <div className="bg-white dark:bg-gray-800 border border-gray-200 dark:border-gray-700 rounded-md text-xs max-w-[360px] px-2.5 py-2 text-gray-700 dark:text-gray-300 shadow-lg">
      <div className="font-mono text-gray-900 dark:text-gray-100 mb-1">{p.short_id}</div>
      {truncated && (
        <div className="text-gray-500 dark:text-gray-400 mb-1.5 italic">{truncated}</div>
      )}
      <TooltipRow
        label="Cost"
        value={formatCost(p.cost, currency)}
        valueClass="text-gray-900 dark:text-gray-100 font-semibold"
      />
      <TooltipRow label="Duration" value={formatDuration(p.duration_s)} />
      <TooltipRow label="Commands" value={p.commands.toLocaleString()} />
      {p.errors > 0 && (
        <TooltipRow
          label="Errors"
          value={p.errors.toLocaleString()}
          valueClass="text-red-600 dark:text-red-400"
        />
      )}
      <div className="border-t border-gray-200 dark:border-gray-700 mt-1.5 pt-1.5">
        <div className="text-[11px] mb-0.5 text-gray-400 dark:text-gray-500">Tokens</div>
        <TooltipRow label="Input" value={formatTokens(input)} />
        <TooltipRow label="Output" value={formatTokens(output)} />
        <TooltipRow label="Cache read" value={formatTokens(cacheRead)} />
        <TooltipRow label="Cache creation" value={formatTokens(cacheCreation)} />
      </div>
      {p.models_used && p.models_used.length > 0 && (
        <div className="border-t border-gray-200 dark:border-gray-700 mt-1.5 pt-1.5">
          <div className="text-[11px] mb-0.5 text-gray-400 dark:text-gray-500">Models</div>
          <div
            className="text-[11px] text-gray-700 dark:text-gray-300 whitespace-normal break-words"
            title={p.models_used.join(', ')}
          >
            {p.models_used.map(formatModelName).join(', ')}
          </div>
        </div>
      )}
    </div>
  )
}

export default function SessionCostBarChart({ data, onSelect }: SessionCostBarChartProps) {
  const { currency } = useCurrency()
  // #59: theme-aware chart chrome (grid/axes) instead of fixed dark hexes.
  const palette = useChartTheme()
  if (!data || data.length === 0) {
    return (
      <div className="bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-4 border border-gray-200 dark:border-gray-800">
        <h3 className="text-sm font-medium text-gray-700 dark:text-gray-300 mb-3">Top Sessions by Cost</h3>
        <div className="text-xs text-gray-500 py-8 text-center">No session cost data yet</div>
      </div>
    )
  }

  const chartData: ChartDatum[] = [...data]
    .sort((a, b) => b.cost - a.cost)
    .slice(0, 10)
    .map((s) => {
      const sid = shortSession(s.session_id)
      const preview = (s.first_prompt_preview ?? '').replace(/\s+/g, ' ').trim()
      const truncated = preview.length > 36 ? preview.slice(0, 36) + '…' : preview
      return {
        session_id: s.session_id,
        short_id: sid,
        label: truncated ? `${sid} · ${truncated}` : sid,
        cost: s.cost,
        commands: s.commands,
        errors: s.errors,
        messages: s.messages,
        duration_s: s.duration_s,
        models_used: s.models_used ?? [],
        tokens: s.tokens ?? {},
        preview: s.first_prompt_preview,
      }
    })

  const maxCost = chartData.reduce((m, d) => (d.cost > m ? d.cost : m), 0)

  // #62: an all-zero-cost dataset would render Y-axis labels next to invisible
  // (zero-width) bars. Show an explicit empty state instead of a ghost chart;
  // this also guards the label-threshold maths below (only runs when maxCost>0).
  if (maxCost <= 0) {
    return (
      <div className="bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-4 border border-gray-200 dark:border-gray-800">
        <h3 className="text-sm font-medium text-gray-700 dark:text-gray-300 mb-3">Top Sessions by Cost</h3>
        <div className="text-xs text-gray-500 py-8 text-center">No cost recorded for these sessions</div>
      </div>
    )
  }

  // Only label bars that are > 10% of the chart max.
  const labelThreshold = maxCost * 0.1

  const handleBarClick = (entry: { session_id?: string } | undefined) => {
    const sid = entry?.session_id
    if (!sid) return
    if (onSelect) {
      onSelect(sid)
      return
    }
    openSession(sid)
  }

  return (
    <div className="bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-4 border border-gray-200 dark:border-gray-800">
      <h3 className="text-sm font-medium text-gray-700 dark:text-gray-300 mb-3">
        Top Sessions by Cost
        <span className="ml-2 text-xs text-gray-500 font-normal">top {chartData.length}</span>
      </h3>
      <ResponsiveContainer width="100%" height={Math.max(260, chartData.length * 32)}>
        <BarChart data={chartData} layout="vertical" margin={{ left: 10, right: 20 }}>
          <CartesianGrid strokeDasharray="3 3" stroke={palette.grid} horizontal={false} />
          <XAxis
            type="number"
            tick={palette.tick}
            tickLine={palette.axisLine}
            axisLine={palette.axisLine}
            tickFormatter={(v: number) => formatCost(v, currency)}
          />
          <YAxis
            type="category"
            dataKey="label"
            tick={{ fontSize: 11, fill: palette.tick.fill }}
            tickLine={palette.axisLine}
            axisLine={palette.axisLine}
            width={320}
            interval={0}
          />
          <Tooltip
            content={<SessionTooltip currency={currency} />}
            cursor={{ fill: 'rgba(75, 85, 99, 0.15)' }}
          />
          <Bar
            dataKey="cost"
            radius={[0, 4, 4, 0]}
            cursor="pointer"
            onClick={handleBarClick}
          >
            {chartData.map((_entry, index) => (
              <Cell key={index} fill={COLORS[index % COLORS.length]} />
            ))}
            <LabelList
              dataKey="cost"
              position="insideRight"
              fill="#F9FAFB"
              fontSize={10}
              fontWeight={600}
              // eslint-disable-next-line @typescript-eslint/no-explicit-any
              formatter={(value: any) => {
                const n = typeof value === 'number' ? value : Number(value)
                if (!Number.isFinite(n) || n <= labelThreshold) return ''
                return formatCost(n, currency)
              }}
            />
          </Bar>
        </BarChart>
      </ResponsiveContainer>
    </div>
  )
}
