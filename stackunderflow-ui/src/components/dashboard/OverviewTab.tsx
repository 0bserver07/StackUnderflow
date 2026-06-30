import { useCallback, useEffect, useMemo, useState } from 'react'
import {
  IconHash,
  IconCurrencyDollar,
  IconTerminal2,
  IconMessageCircle,
  IconCpu,
  IconClockHour4,
  IconUser,
  IconRobot,
  IconTool,
  IconMessage,
  IconCalendar,
} from '@tabler/icons-react'
import type { DashboardStats, Trends, ToolDistributionResponse, HourlyPattern } from '../../types/api'
import StatsCards from '../analytics/StatsCards'
import TokenUsageChart from '../charts/TokenUsageChart'
import DailyCostChart from '../charts/DailyCostChart'
import ModelDistributionChart from '../charts/ModelDistributionChart'
import HourlyPatternChart from '../charts/HourlyPatternChart'
import ErrorDistributionChart from '../charts/ErrorDistributionChart'
import ToolUsageBarChart from '../charts/ToolUsageBarChart'
import CommandToolDistChart from '../charts/CommandToolDistChart'
import InterruptionRateChart from '../charts/InterruptionRateChart'
import ErrorRateChart from '../charts/ErrorRateChart'
import TrendDeltaStrip from '../cost/TrendDeltaStrip'
import CacheRoiCard from '../cost/CacheRoiCard'
import TokenCompositionDonut from '../cost/TokenCompositionDonut'
import PlanBudgetCard from './PlanBudgetCard'
import OptimizeFindingsPanel from './OptimizeFindingsPanel'
import CostAnomaliesPanel from './CostAnomaliesPanel'
import { setTab } from '../../services/navigation'

interface OverviewTabProps {
  stats: DashboardStats
}

function formatNumber(n: number | null | undefined): string {
  if (!Number.isFinite(n)) return '0'
  const v = n as number
  if (v >= 1_000_000) return `${(v / 1_000_000).toFixed(2)}M`
  if (v >= 1_000) return `${(v / 1_000).toFixed(1)}k`
  return v.toLocaleString()
}

/**
 * "2026-01-30T20:58:11.193Z" → "Jan 30, 2026". Falls through to the original
 * string if it's not a parseable ISO timestamp so we never blank out a label.
 */
function formatDateRange(iso: string): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  return d.toLocaleDateString(undefined, {
    month: 'short',
    day: 'numeric',
    year: 'numeric',
  })
}

import { formatCost, formatModelName } from '../../services/format'
import { useCurrency } from '../../services/currency'
import { useFilters } from '../../services/filters'

// Hoisted empty fallbacks so the (memoised) chart children receive a stable
// reference every render instead of a fresh `{}` / `{messages,tokens}` literal
// — without this, `?? {}` defeats React.memo on every parent re-render (#8).
const EMPTY_RECORD: Record<string, never> = {}
const EMPTY_HOURLY_PATTERN: HourlyPattern = { messages: {}, tokens: {} }

interface MiniStatCardProps {
  icon: React.ReactNode
  label: string
  value: string
  sublabel?: string
  color?: string
}

function MiniStatCard({ icon, label, value, sublabel, color = 'text-gray-600 dark:text-gray-400' }: MiniStatCardProps) {
  return (
    <div className="bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-3 border border-gray-200 dark:border-gray-800">
      <div className="flex items-center gap-1.5 mb-1">
        <span className={color}>{icon}</span>
        <span className="text-[10px] text-gray-500 uppercase tracking-wider">{label}</span>
      </div>
      <div className="text-lg font-bold text-gray-900 dark:text-gray-100">{value}</div>
      {sublabel && <div className="text-[10px] text-gray-500 mt-0.5">{sublabel}</div>}
    </div>
  )
}

export default function OverviewTab({ stats }: OverviewTabProps) {
  const { currency } = useCurrency()
  // #33: the command/tool-count distribution must honour the active
  // provider/model filter. `queryString` is `&provider=…&model=…` (or '').
  const { queryString } = useFilters()
  // Trends moved off /api/dashboard-data into /api/cost-data (spec §A3) — lazy
  // fetch them in a non-blocking effect so the rest of the overview renders
  // immediately. `stats.trends` will normally be undefined here; we still seed
  // from it so an older payload (or a future re-merge) keeps working.
  const [trends, setTrends] = useState<Trends | null>(stats.trends ?? null)
  // §D2: tool_count_distribution moved off /api/dashboard-data onto
  // /api/tool-distribution. Fetch it post-mount so the chart shows its empty
  // state for the few hundred ms until the response arrives instead of
  // blocking the rest of the Overview tab.
  const [toolCountDist, setToolCountDist] = useState<Record<string, number>>({})

  useEffect(() => {
    if (stats.trends) {
      setTrends(stats.trends)
      return
    }
    let cancelled = false
    fetch('/api/cost-data')
      .then((res) => (res.ok ? res.json() : null))
      .then((data) => {
        if (cancelled || !data) return
        // /api/cost-data returns {} for missing trends — treat that as null so
        // TrendDeltaStrip renders its empty state instead of NaN tiles.
        const t = data.trends as Trends | undefined
        if (t && t.current_week && t.prior_week && t.delta_pct) {
          setTrends(t)
        }
      })
      .catch(() => {
        // Non-blocking: leave `trends` as null and let the strip show its
        // empty state. We deliberately don't surface this in the UI.
      })
    return () => {
      cancelled = true
    }
  }, [stats.trends])

  useEffect(() => {
    let cancelled = false
    // FLAG (backend): /api/tool-distribution (routes/commands.py) currently
    // ignores ?provider/?model — it must apply the same filter the rest of the
    // dashboard uses, or this chart stays project-wide while the others narrow.
    const qs = queryString ? `?${queryString.slice(1)}` : ''
    fetch(`/api/tool-distribution${qs}`)
      .then((res) => (res.ok ? (res.json() as Promise<ToolDistributionResponse>) : null))
      .then((data) => {
        if (cancelled || !data) return
        setToolCountDist(data.tool_count_distribution ?? {})
      })
      .catch(() => {
        // Non-blocking: leave the distribution empty so CommandToolDistChart
        // renders its empty state. Deliberately not surfaced in the UI.
      })
    return () => {
      cancelled = true
    }
  }, [queryString])

  // §C22 trend strip click → Cost tab. Memoised so the strip (and the rest of
  // the memoised children) aren't handed a fresh closure every render (#8).
  const handleTrendTileClick = useCallback(() => {
    setTab('cost')
  }, [])

  // `token_composition` also moved to /api/cost-data; per task brief, prefer
  // the simpler fallback derived from the still-present overview.total_tokens.
  // Memoised so TokenCompositionDonut's `totals` prop stays referentially
  // stable across unrelated re-renders (#8).
  const tokenTotals = useMemo(() => {
    const t = stats.overview?.total_tokens ?? { input: 0, output: 0, cache_read: 0, cache_creation: 0 }
    return (
      stats.token_composition?.totals ?? {
        input: t.input,
        output: t.output,
        cache_read: t.cache_read,
        cache_creation: t.cache_creation,
      }
    )
  }, [stats.token_composition, stats.overview])

  if (!stats?.overview) return null

  const tokens = stats.overview.total_tokens ?? { input: 0, output: 0, cache_read: 0, cache_creation: 0 }
  const totalTokens = tokens.input + tokens.output + tokens.cache_read + tokens.cache_creation
  const interactions = stats.user_interactions ?? { user_commands_analyzed: 0, avg_tools_per_command: 0 }
  const dateRange = stats.overview.date_range ?? { start: '', end: '' }
  const modelsUsed = stats.models ?? {}
  const messageTypes = stats.overview.message_types ?? {}

  const userMessages = messageTypes['user'] ?? 0
  const assistantMessages = messageTypes['assistant'] ?? 0
  const toolUseMessages = messageTypes['tool_use'] ?? 0
  const toolResultMessages = messageTypes['tool_result'] ?? 0

  return (
    <div className="space-y-6">
      {/* Plan budget — v0.6.0. Renders only when a plan is configured;
          self-hides when /api/plan returns nulls so Overview stays clean
          for users who haven't set one. Place above the trend strip so the
          "am I over budget?" answer is the first thing visible. */}
      <PlanBudgetCard />

      {/* Trend delta strip — full-width top banner (spec §2.4 / C22) */}
      <TrendDeltaStrip
        trends={trends}
        endDate={dateRange.end || undefined}
        onTileClick={handleTrendTileClick}
      />

      {/* Primary stats from existing StatsCards component */}
      <StatsCards stats={stats} />

      {/* Optimize findings — v0.6.0. Self-hides when /api/optimize returns
          zero patterns so we don't surface noise on a healthy install. */}
      <OptimizeFindingsPanel />

      {/* Cost anomalies — audit #7. Statistical outlier days/sessions from the
          same /api/optimize payload. Self-hides when there are no outliers. */}
      <CostAnomaliesPanel />

      {/* Cache ROI + Token composition share a row on wide screens so the
          donut doesn't stretch to a full-width band on its own. */}
      <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
        <CacheRoiCard cache={stats.cache} dailyStats={stats.daily_stats} />
        <TokenCompositionDonut totals={tokenTotals} />
      </div>

      {/* Extended stat cards grid */}
      <div className="grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-5 gap-3">
        <MiniStatCard
          icon={<IconHash size={14} />}
          label="Total Tokens"
          value={formatNumber(totalTokens)}
          color="text-gray-600 dark:text-gray-400"
        />
        <MiniStatCard
          icon={<IconCurrencyDollar size={14} />}
          label="Total Cost"
          value={formatCost(stats.overview.total_cost ?? 0, currency)}
          color="text-green-400"
        />
        <MiniStatCard
          icon={<IconTerminal2 size={14} />}
          label="Commands Analyzed"
          value={formatNumber(interactions.user_commands_analyzed)}
          color="text-cyan-400"
        />
        <MiniStatCard
          icon={<IconMessageCircle size={14} />}
          label="Total Messages"
          value={formatNumber(stats.overview.total_messages ?? 0)}
          color="text-violet-400"
        />
        <MiniStatCard
          icon={<IconClockHour4 size={14} />}
          label="Avg Tools/Cmd"
          value={(interactions.avg_tools_per_command ?? 0).toFixed(1)}
          color="text-blue-400"
        />
        <MiniStatCard
          icon={<IconCpu size={14} />}
          label="Models Used"
          value={String(Object.keys(modelsUsed).length)}
          sublabel={Object.keys(modelsUsed).slice(0, 2).map(formatModelName).join(', ')}
          color="text-pink-400"
        />
        <MiniStatCard
          icon={<IconUser size={14} />}
          label="User Messages"
          value={formatNumber(userMessages)}
          color="text-indigo-400"
        />
        <MiniStatCard
          icon={<IconRobot size={14} />}
          label="Assistant Messages"
          value={formatNumber(assistantMessages)}
          color="text-emerald-400"
        />
        <MiniStatCard
          icon={<IconTool size={14} />}
          label="Tool Use"
          value={formatNumber(toolUseMessages)}
          color="text-amber-400"
        />
        <MiniStatCard
          icon={<IconMessage size={14} />}
          label="Tool Results"
          value={formatNumber(toolResultMessages)}
          color="text-cyan-400"
        />
        <MiniStatCard
          icon={<IconCalendar size={14} />}
          label="Date Range"
          value={dateRange.start ? formatDateRange(dateRange.start) : 'N/A'}
          sublabel={dateRange.end ? `to ${formatDateRange(dateRange.end)}` : ''}
          color="text-gray-600 dark:text-gray-400"
        />
      </div>

      {/* Charts section - 2 column grid.
          #18: ToolUsageChart was rendered right beside ToolUsageBarChart on
          the same `usage_counts` dataset — dropped the duplicate, kept the bar.
          #53: a second "Error Categories" panel (ErrorCategoryChart pie) showed
          the identical `by_category` field — dropped it. */}
      <div className="grid grid-cols-1 xl:grid-cols-2 gap-6">
        <TokenUsageChart dailyStats={stats.daily_stats ?? EMPTY_RECORD} />
        <DailyCostChart dailyStats={stats.daily_stats ?? EMPTY_RECORD} />
        <ToolUsageBarChart toolStats={stats.tools?.usage_counts ?? EMPTY_RECORD} />
        <ModelDistributionChart modelStats={stats.models ?? EMPTY_RECORD} />
        <HourlyPatternChart hourlyPattern={stats.hourly_pattern ?? EMPTY_HOURLY_PATTERN} />
        <CommandToolDistChart toolCountDist={toolCountDist} />
        <InterruptionRateChart dailyStats={stats.daily_stats ?? EMPTY_RECORD} />
        <ErrorDistributionChart errorCategories={stats.errors?.by_category ?? EMPTY_RECORD} />
        <ErrorRateChart dailyStats={stats.daily_stats ?? EMPTY_RECORD} />
      </div>
    </div>
  )
}
