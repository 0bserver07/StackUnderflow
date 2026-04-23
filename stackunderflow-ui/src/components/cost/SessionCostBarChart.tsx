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

// ---------------------------------------------------------------------------
// Navigation helper — concurrently landing at `services/navigation` (spec §A8).
// The module may not yet exist in this worktree, so we load it optimistically
// at runtime and fall back to a local CustomEvent dispatcher otherwise.
// TODO(c-sesscost): once `services/navigation.ts` merges into this branch,
// swap this block for `import { openSession } from '../../services/navigation'`.
// ---------------------------------------------------------------------------
let navOpenSession: ((sessionId: string) => void) | null = null
if (typeof window !== 'undefined') {
  // Path stored in a variable so bundlers don't resolve it statically.
  const navPath = '../../services/navigation'
  // eslint-disable-next-line @typescript-eslint/ban-ts-comment
  // @ts-ignore — optimistic import; module may not exist at build time
  import(/* @vite-ignore */ navPath)
    .then((mod: { openSession?: (id: string) => void }) => {
      if (mod && typeof mod.openSession === 'function') {
        navOpenSession = mod.openSession
      }
    })
    .catch(() => {
      /* navigation service not yet available — fallback handles this */
    })
}

function fallbackOpenSession(sessionId: string): void {
  // Minimal shim matching navigation.openSession contract:
  // update URL + dispatch `stackunderflow:nav` CustomEvent.
  if (typeof window === 'undefined') return
  try {
    const url = new URL(window.location.href)
    url.search = ''
    url.searchParams.set('tab', 'sessions')
    url.searchParams.set('session', sessionId)
    const next = `${url.pathname}${url.search}${url.hash}`
    const current = `${window.location.pathname}${window.location.search}${window.location.hash}`
    if (next !== current) {
      window.history.pushState({}, '', next)
    }
  } catch {
    /* URL mutation best-effort */
  }
  window.dispatchEvent(
    new CustomEvent('stackunderflow:nav', {
      detail: { tab: 'sessions', session: sessionId },
    }),
  )
}

function resolveOpenSession(sessionId: string): void {
  if (navOpenSession) {
    navOpenSession(sessionId)
    return
  }
  fallbackOpenSession(sessionId)
}

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

function formatDuration(totalSeconds: number): string {
  if (!Number.isFinite(totalSeconds) || totalSeconds < 0) return '0:00:00'
  const s = Math.floor(totalSeconds)
  const h = Math.floor(s / 3600)
  const m = Math.floor((s % 3600) / 60)
  const sec = s % 60
  return `${h}:${m.toString().padStart(2, '0')}:${sec.toString().padStart(2, '0')}`
}

function formatTokens(n: number): string {
  if (!Number.isFinite(n)) return '0'
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`
  return n.toLocaleString()
}

interface ChartDatum {
  session_id: string
  short_id: string
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

function SessionTooltip({
  active,
  payload,
}: {
  active?: boolean
  payload?: TooltipPayloadEntry[]
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
    <div
      style={{
        backgroundColor: '#1F2937',
        border: '1px solid #374151',
        borderRadius: '6px',
        fontSize: '12px',
        maxWidth: 360,
        padding: '8px 10px',
        color: '#D1D5DB',
      }}
    >
      <div style={{ fontFamily: 'monospace', color: '#F3F4F6', marginBottom: 4 }}>
        {p.short_id}
      </div>
      {truncated && (
        <div style={{ color: '#9CA3AF', marginBottom: 6, fontStyle: 'italic' }}>
          {truncated}
        </div>
      )}
      <div style={{ display: 'flex', justifyContent: 'space-between', gap: 12 }}>
        <span style={{ color: '#9CA3AF' }}>Cost</span>
        <span style={{ color: '#F3F4F6', fontWeight: 600 }}>{formatCost(p.cost)}</span>
      </div>
      <div style={{ display: 'flex', justifyContent: 'space-between', gap: 12 }}>
        <span style={{ color: '#9CA3AF' }}>Duration</span>
        <span style={{ color: '#F3F4F6' }}>{formatDuration(p.duration_s)}</span>
      </div>
      <div style={{ display: 'flex', justifyContent: 'space-between', gap: 12 }}>
        <span style={{ color: '#9CA3AF' }}>Commands</span>
        <span style={{ color: '#F3F4F6' }}>{p.commands.toLocaleString()}</span>
      </div>
      {p.errors > 0 && (
        <div style={{ display: 'flex', justifyContent: 'space-between', gap: 12 }}>
          <span style={{ color: '#9CA3AF' }}>Errors</span>
          <span style={{ color: '#F87171' }}>{p.errors.toLocaleString()}</span>
        </div>
      )}
      <div
        style={{
          borderTop: '1px solid #374151',
          marginTop: 6,
          paddingTop: 6,
          color: '#9CA3AF',
        }}
      >
        <div style={{ fontSize: '11px', marginBottom: 2, color: '#6B7280' }}>Tokens</div>
        <div style={{ display: 'flex', justifyContent: 'space-between' }}>
          <span>Input</span>
          <span style={{ color: '#D1D5DB' }}>{formatTokens(input)}</span>
        </div>
        <div style={{ display: 'flex', justifyContent: 'space-between' }}>
          <span>Output</span>
          <span style={{ color: '#D1D5DB' }}>{formatTokens(output)}</span>
        </div>
        <div style={{ display: 'flex', justifyContent: 'space-between' }}>
          <span>Cache read</span>
          <span style={{ color: '#D1D5DB' }}>{formatTokens(cacheRead)}</span>
        </div>
        <div style={{ display: 'flex', justifyContent: 'space-between' }}>
          <span>Cache creation</span>
          <span style={{ color: '#D1D5DB' }}>{formatTokens(cacheCreation)}</span>
        </div>
      </div>
      {p.models_used && p.models_used.length > 0 && (
        <div
          style={{
            borderTop: '1px solid #374151',
            marginTop: 6,
            paddingTop: 6,
            color: '#9CA3AF',
          }}
        >
          <div style={{ fontSize: '11px', marginBottom: 2, color: '#6B7280' }}>Models</div>
          <div
            style={{
              color: '#D1D5DB',
              fontFamily: 'monospace',
              fontSize: '11px',
              whiteSpace: 'normal',
              wordBreak: 'break-word',
            }}
          >
            {p.models_used.join(', ')}
          </div>
        </div>
      )}
    </div>
  )
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

  const chartData: ChartDatum[] = [...data]
    .sort((a, b) => b.cost - a.cost)
    .slice(0, 10)
    .map((s) => ({
      session_id: s.session_id,
      short_id: shortSession(s.session_id),
      cost: s.cost,
      commands: s.commands,
      errors: s.errors,
      messages: s.messages,
      duration_s: s.duration_s,
      models_used: s.models_used ?? [],
      tokens: s.tokens ?? {},
      preview: s.first_prompt_preview,
    }))

  const maxCost = chartData.reduce((m, d) => (d.cost > m ? d.cost : m), 0)
  // Only label bars that are > 10% of the chart max.
  const labelThreshold = maxCost * 0.1

  const handleBarClick = (entry: { session_id?: string } | undefined) => {
    const sid = entry?.session_id
    if (!sid) return
    if (onSelect) {
      onSelect(sid)
      return
    }
    resolveOpenSession(sid)
  }

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
            content={<SessionTooltip />}
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
                return formatCost(n)
              }}
            />
          </Bar>
        </BarChart>
      </ResponsiveContainer>
    </div>
  )
}
