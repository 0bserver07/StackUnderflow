import { useEffect, useState } from 'react'
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
import type { DashboardStats, Trends } from '../../types/api'
import StatsCards from '../analytics/StatsCards'
import TokenUsageChart from '../charts/TokenUsageChart'
import DailyCostChart from '../charts/DailyCostChart'
import ModelDistributionChart from '../charts/ModelDistributionChart'
import HourlyPatternChart from '../charts/HourlyPatternChart'
import ErrorDistributionChart from '../charts/ErrorDistributionChart'
import ToolUsageChart from '../charts/ToolUsageChart'
import ToolUsageBarChart from '../charts/ToolUsageBarChart'
import CommandToolDistChart from '../charts/CommandToolDistChart'
import InterruptionRateChart from '../charts/InterruptionRateChart'
import ErrorRateChart from '../charts/ErrorRateChart'
import ErrorCategoryChart from '../charts/ErrorCategoryChart'
import TrendDeltaStrip from '../cost/TrendDeltaStrip'
import CacheRoiCard from '../cost/CacheRoiCard'
import TokenCompositionDonut from '../cost/TokenCompositionDonut'
import { setTab } from '../../services/navigation'

interface OverviewTabProps {
  stats: DashboardStats
}

function formatNumber(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`
  return n.toLocaleString()
}

import { formatCost } from '../../services/format'

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
  // Trends moved off /api/dashboard-data into /api/cost-data (spec §A3) — lazy
  // fetch them in a non-blocking effect so the rest of the overview renders
  // immediately. `stats.trends` will normally be undefined here; we still seed
  // from it so an older payload (or a future re-merge) keeps working.
  const [trends, setTrends] = useState<Trends | null>(stats.trends ?? null)

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

  // `token_composition` also moved to /api/cost-data; per task brief, prefer
  // the simpler fallback derived from the still-present overview.total_tokens.
  const tokenTotals = stats.token_composition?.totals ?? {
    input: tokens.input,
    output: tokens.output,
    cache_read: tokens.cache_read,
    cache_creation: tokens.cache_creation,
  }

  // Click on any TrendDeltaStrip tile → jump to the Cost tab. The strip also
  // dispatches a `stackunderflow:filter-window` event independently; the Cost
  // tab can pick that up to apply a date filter once it lands.
  const handleTrendTileClick = () => {
    setTab('cost')
  }

  return (
    <div className="space-y-6">
      {/* Trend delta strip — full-width top banner (spec §2.4 / C22) */}
      <TrendDeltaStrip
        trends={trends}
        endDate={dateRange.end || undefined}
        onTileClick={handleTrendTileClick}
      />

      {/* Primary stats from existing StatsCards component */}
      <StatsCards stats={stats} />

      {/* Cache ROI hero card — uses the still-present `cache` field on
          /api/dashboard-data; daily_stats supplies the ROI sparkline. */}
      <CacheRoiCard cache={stats.cache} dailyStats={stats.daily_stats} />

      {/* Token composition donut replaces the four mini token cards (spec §2.4) */}
      <TokenCompositionDonut totals={tokenTotals} />

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
          value={formatCost(stats.overview.total_cost ?? 0)}
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
          sublabel={Object.keys(modelsUsed).slice(0, 2).join(', ')}
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
          value={dateRange.start ? `${dateRange.start.slice(5)}` : 'N/A'}
          sublabel={dateRange.end ? `to ${dateRange.end.slice(5)}` : ''}
          color="text-gray-600 dark:text-gray-400"
        />
      </div>

      {/* Charts section - 2 column grid */}
      <div className="grid grid-cols-1 xl:grid-cols-2 gap-6">
        <TokenUsageChart dailyStats={stats.daily_stats ?? {}} />
        <DailyCostChart dailyStats={stats.daily_stats ?? {}} />
        <ToolUsageChart toolStats={stats.tools?.usage_counts ?? {}} />
        <ToolUsageBarChart toolStats={stats.tools?.usage_counts ?? {}} />
        <ModelDistributionChart modelStats={stats.models ?? {}} />
        <HourlyPatternChart hourlyPattern={stats.hourly_pattern ?? { messages: {}, tokens: {} }} />
        <CommandToolDistChart toolCountDist={stats.user_interactions?.tool_count_distribution ?? {}} />
        <InterruptionRateChart dailyStats={stats.daily_stats ?? {}} />
        <ErrorDistributionChart errorCategories={stats.errors?.by_category ?? {}} />
        <ErrorRateChart dailyStats={stats.daily_stats ?? {}} />
        <ErrorCategoryChart errorCategories={stats.errors?.by_category ?? {}} />
      </div>
    </div>
  )
}
