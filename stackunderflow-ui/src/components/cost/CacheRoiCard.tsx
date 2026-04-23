import { useMemo, useState } from 'react'
import {
  IconDatabase,
  IconCircleCheck,
  IconCircleDashed,
  IconChevronRight,
} from '@tabler/icons-react'
import {
  Line,
  LineChart,
  ResponsiveContainer,
  Tooltip as RechartsTooltip,
} from 'recharts'
import type { CacheStats, DailyData, SessionCost } from '../../types/api'

interface CacheRoiCardProps {
  cache: CacheStats | null | undefined
  /**
   * Optional list of per-session cost + token breakdowns. When supplied, the
   * card exposes an expandable breakdown of the top 5 cache-saving sessions
   * ranked by `tokens.cache_read - tokens.cache_creation`.
   */
  sessionCosts?: SessionCost[]
  /**
   * Optional per-day aggregates keyed by ISO date. When each bucket contains
   * `tokens.cache_read` + `tokens.cache_creation`, the card renders a small
   * ROI trend sparkline (cache_read / cache_creation). Untyped at the prop
   * boundary so callers can pass either `Record<string, DailyData>` or a
   * looser shape.
   */
  dailyStats?: Record<string, DailyData | Record<string, unknown>>
}

interface SessionSaver {
  session_id: string
  tokens_saved: number
  cost_saved: number
}

interface RoiPoint {
  date: string
  ratio: number
}

function formatNumber(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`
  return n.toLocaleString()
}

function formatCost(cost: number): string {
  if (cost >= 100) return `$${cost.toFixed(0)}`
  if (cost >= 1) return `$${cost.toFixed(2)}`
  return `$${cost.toFixed(4)}`
}

function shortId(id: string): string {
  return id.length > 8 ? id.slice(0, 8) : id
}

/**
 * Compute top 5 cache-saving sessions, pro-rating overall `cost_saved` by each
 * session's share of total tokens saved. Pro-rating is a deliberate
 * approximation: per-session dollar savings isn't tracked directly and pricing
 * varies by model — the ratio gives a faithful ranking even when the absolute
 * number lags reality on mixed-model sessions.
 */
function computeTopSavers(
  sessionCosts: SessionCost[] | undefined,
  cache: CacheStats,
): SessionSaver[] {
  if (!sessionCosts || sessionCosts.length === 0) return []
  const totalTokensSaved = cache.tokens_saved ?? 0
  const totalCostSaved = cache.cost_saved_base_units ?? 0
  const unitCost =
    totalTokensSaved > 0 ? totalCostSaved / totalTokensSaved : 0

  return sessionCosts
    .map((s) => {
      const read = s.tokens?.cache_read ?? 0
      const creation = s.tokens?.cache_creation ?? 0
      const tokens_saved = read - creation
      return {
        session_id: s.session_id,
        tokens_saved,
        cost_saved: Math.max(tokens_saved, 0) * unitCost,
      }
    })
    .filter((s) => s.tokens_saved > 0)
    .sort((a, b) => b.tokens_saved - a.tokens_saved)
    .slice(0, 5)
}

/** Extract ROI ratio points from `dailyStats`; returns `null` if insufficient. */
function computeRoiTrend(
  dailyStats: CacheRoiCardProps['dailyStats'],
): RoiPoint[] | null {
  if (!dailyStats) return null
  const points: RoiPoint[] = []

  for (const [date, bucket] of Object.entries(dailyStats)) {
    const tokens = (bucket as { tokens?: Record<string, unknown> })?.tokens
    if (!tokens || typeof tokens !== 'object') continue
    const read = Number((tokens as Record<string, unknown>).cache_read ?? 0)
    const creation = Number(
      (tokens as Record<string, unknown>).cache_creation ?? 0,
    )
    if (!Number.isFinite(read) || !Number.isFinite(creation)) continue
    if (creation <= 0) continue
    points.push({ date, ratio: read / creation })
  }

  if (points.length < 2) return null
  points.sort((a, b) => a.date.localeCompare(b.date))
  return points
}

/**
 * Hero card for cache ROI — the marquee metric for "am I getting my money's
 * worth?". ROI % is computed from tokens_saved vs tokens written into the
 * cache. When per-session + per-day data is available, the card also exposes
 * a top-5 session breakdown and a small ROI trend sparkline.
 */
export default function CacheRoiCard({
  cache,
  sessionCosts,
  dailyStats,
}: CacheRoiCardProps) {
  // TODO(stackunderflow): once `../common/ExpandableRow` lands on main, migrate
  // the session-saver disclosure to use that primitive. It's table-shaped
  // (<tr> pair), so card-level toggling is kept inline here for now.
  const [expanded, setExpanded] = useState(false)

  const topSavers = useMemo(
    () => (cache ? computeTopSavers(sessionCosts, cache) : []),
    [sessionCosts, cache],
  )
  const trend = useMemo(() => computeRoiTrend(dailyStats), [dailyStats])

  if (!cache || cache.total_created === 0) {
    return (
      <div
        className="bg-gradient-to-br from-indigo-900/30 to-gray-900/30 rounded-lg p-6 border border-indigo-900/40"
        data-testid="cache-roi-card"
      >
        <div className="flex items-center gap-2 mb-3">
          <IconDatabase size={18} className="text-indigo-400" />
          <span className="text-xs text-gray-400 uppercase tracking-wider">Cache ROI</span>
        </div>
        <div className="text-gray-500 text-sm">No cache activity yet</div>
      </div>
    )
  }

  const tokensSaved = cache.tokens_saved ?? 0
  const tokensCreated = cache.total_created ?? 0
  const roiPct = tokensCreated > 0 ? (tokensSaved / tokensCreated) * 100 : 0
  const costSaved = cache.cost_saved_base_units ?? 0
  const breakEven = cache.break_even_achieved === true
  const hitRate = cache.hit_rate ?? 0

  const canToggle = topSavers.length > 0

  return (
    <div
      className="bg-gradient-to-br from-indigo-900/40 to-gray-900/40 rounded-lg p-6 border border-indigo-900/50"
      data-testid="cache-roi-card"
    >
      <div className="flex items-center gap-2 mb-3">
        <IconDatabase size={18} className="text-indigo-400" />
        <span className="text-xs text-gray-400 uppercase tracking-wider">Cache ROI</span>
        <div className="flex-1" />
        {breakEven ? (
          <span className="inline-flex items-center gap-1 text-[10px] text-green-300 bg-green-900/40 border border-green-800 rounded-full px-2 py-0.5">
            <IconCircleCheck size={10} /> break-even
          </span>
        ) : (
          <span className="inline-flex items-center gap-1 text-[10px] text-amber-300 bg-amber-900/30 border border-amber-800 rounded-full px-2 py-0.5">
            <IconCircleDashed size={10} /> below break-even
          </span>
        )}
      </div>

      <div className="flex items-end gap-5">
        <div className="flex-1 min-w-0">
          <div className="text-5xl font-bold text-indigo-300 leading-none">
            {roiPct.toFixed(0)}
            <span className="text-2xl text-indigo-400/80 align-top ml-1">%</span>
          </div>
          <div className="text-xs text-gray-500 mt-1">return on cache writes</div>
        </div>
        {trend ? (
          <div
            className="w-28 h-12 shrink-0"
            aria-label="Cache ROI trend sparkline"
            data-testid="cache-roi-sparkline"
          >
            <ResponsiveContainer width="100%" height="100%">
              <LineChart data={trend} margin={{ top: 2, right: 2, bottom: 2, left: 2 }}>
                <RechartsTooltip
                  contentStyle={{
                    backgroundColor: '#1F2937',
                    border: '1px solid #374151',
                    borderRadius: '4px',
                    fontSize: '10px',
                    padding: '4px 6px',
                  }}
                  labelStyle={{ color: '#D1D5DB' }}
                  formatter={(value: number) => [`${value.toFixed(2)}×`, 'read / created']}
                />
                <Line
                  type="monotone"
                  dataKey="ratio"
                  stroke="#A5B4FC"
                  strokeWidth={1.5}
                  dot={false}
                  isAnimationActive={false}
                />
              </LineChart>
            </ResponsiveContainer>
          </div>
        ) : null}
      </div>

      <div className="grid grid-cols-3 gap-4 mt-5 pt-4 border-t border-indigo-900/30">
        <div>
          <div className="text-[10px] text-gray-500 uppercase tracking-wider">Tokens Saved</div>
          <div className="text-lg font-semibold text-gray-100 mt-0.5">{formatNumber(tokensSaved)}</div>
        </div>
        <div>
          <div className="text-[10px] text-gray-500 uppercase tracking-wider">Cost Saved</div>
          <div className="text-lg font-semibold text-green-300 mt-0.5">{formatCost(costSaved)}</div>
        </div>
        <div>
          <div className="text-[10px] text-gray-500 uppercase tracking-wider">Hit Rate</div>
          <div className="text-lg font-semibold text-gray-100 mt-0.5">{hitRate.toFixed(1)}%</div>
        </div>
      </div>

      {canToggle ? (
        <div className="mt-4 pt-3 border-t border-indigo-900/30">
          <button
            type="button"
            onClick={() => setExpanded((v) => !v)}
            aria-expanded={expanded}
            className="w-full flex items-center gap-1.5 text-[11px] text-indigo-300 hover:text-indigo-200 uppercase tracking-wider focus:outline-none focus:text-indigo-200"
            data-testid="cache-roi-toggle"
          >
            <IconChevronRight
              size={12}
              className={`transition-transform ${expanded ? 'rotate-90' : ''}`}
            />
            <span>Top cache savers</span>
            <span className="text-gray-500 normal-case tracking-normal">
              · top {topSavers.length} of {sessionCosts?.length ?? 0}
            </span>
          </button>
          {expanded ? (
            <ul
              className="mt-3 divide-y divide-indigo-900/20 rounded border border-indigo-900/30 bg-gray-900/40"
              data-testid="cache-roi-savers"
            >
              {topSavers.map((s) => (
                <li
                  key={s.session_id}
                  className="flex items-center justify-between px-3 py-2 text-xs"
                >
                  <code className="text-indigo-300 font-mono">{shortId(s.session_id)}</code>
                  <div className="flex items-center gap-4 text-right">
                    <div>
                      <div className="text-gray-100 font-semibold">
                        {formatNumber(s.tokens_saved)}
                      </div>
                      <div className="text-[10px] text-gray-500 uppercase tracking-wider">
                        tokens
                      </div>
                    </div>
                    <div>
                      <div className="text-green-300 font-semibold">
                        {formatCost(s.cost_saved)}
                      </div>
                      <div className="text-[10px] text-gray-500 uppercase tracking-wider">
                        saved
                      </div>
                    </div>
                  </div>
                </li>
              ))}
            </ul>
          ) : null}
        </div>
      ) : null}
    </div>
  )
}
