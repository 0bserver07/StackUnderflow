import {
  IconMessageCircle,
  IconCurrencyDollar,
  IconTerminal2,
  IconHash,
  IconHandStop,
  IconRoute,
  IconTool,
  IconDatabase,
  IconAlertTriangle,
} from '@tabler/icons-react'
import { useQuery } from '@tanstack/react-query'
import type { DashboardStats } from '../../types/api'
import { getPricing } from '../../services/api'

interface StatsCardsProps {
  stats: DashboardStats
}

function formatNumber(n: number): string {
  return n.toLocaleString()
}

function formatCost(cost: number): string {
  return `$${cost.toFixed(4)}`
}

function formatPercent(value: number): string {
  return `${value.toFixed(1)}%`
}

function daysSince(isoTimestamp: string | undefined): number | null {
  if (!isoTimestamp) return null
  const then = new Date(isoTimestamp).getTime()
  if (Number.isNaN(then)) return null
  const diffMs = Date.now() - then
  return Math.max(0, Math.floor(diffMs / (1000 * 60 * 60 * 24)))
}

interface StatCardProps {
  icon: React.ReactNode
  label: string
  value: string
  sublabel?: string
  badge?: React.ReactNode
}

function StatCard({ icon, label, value, sublabel, badge }: StatCardProps) {
  return (
    <div className="bg-gray-800 rounded-lg p-4">
      <div className="flex items-center gap-2 mb-2">
        <span className="text-gray-400">{icon}</span>
        <span className="text-xs text-gray-400 uppercase tracking-wider">{label}</span>
        {badge}
      </div>
      <div className="text-2xl font-bold text-gray-100">{value}</div>
      {sublabel && <div className="text-xs text-gray-500 mt-1">{sublabel}</div>}
    </div>
  )
}

export default function StatsCards({ stats }: StatsCardsProps) {
  // Pricing data is used only to surface a "stale pricing" indicator; a
  // failed fetch should never hide the dashboard, so errors are swallowed.
  const { data: pricing } = useQuery({
    queryKey: ['pricing'],
    queryFn: getPricing,
    staleTime: 60 * 60 * 1000,
    retry: false,
  })

  if (!stats?.overview) return null

  const tokens = stats.overview.total_tokens ?? { input: 0, output: 0, cache_read: 0, cache_creation: 0 }
  const totalTokens = tokens.input + tokens.output + tokens.cache_read + tokens.cache_creation
  const interactions = stats.user_interactions ?? {
    user_commands_analyzed: 0,
    avg_tools_per_command: 0,
    total_assistant_steps: 0,
    percentage_requiring_tools: 0,
  }
  const dateRange = stats.overview.date_range ?? { start: '', end: '' }
  const modelsCount = stats.models ? Object.keys(stats.models).length : 0

  const isPricingStale = pricing?.is_stale === true
  const pricingAgeDays = daysSince(pricing?.timestamp)
  const staleTooltip = pricingAgeDays != null
    ? `Pricing data is ${pricingAgeDays} day${pricingAgeDays === 1 ? '' : 's'} old — last refresh may have failed`
    : 'Pricing data could not be refreshed — costs may be out of date'
  const staleBadge = isPricingStale ? (
    <span
      title={staleTooltip}
      className="inline-flex items-center gap-1 ml-auto text-[10px] text-amber-400 bg-amber-500/10 border border-amber-500/30 rounded px-1.5 py-0.5"
    >
      <IconAlertTriangle size={10} />
      stale
    </span>
  ) : null
  const costSublabel = isPricingStale
    ? `${dateRange.start} - ${dateRange.end} · pricing may be outdated`
    : `${dateRange.start} - ${dateRange.end}`

  // Compute interruption rate from daily stats
  const dailyEntries = stats.daily_stats ? Object.values(stats.daily_stats) : []
  const totalCommands = dailyEntries.reduce((sum, d) => sum + (d.user_commands ?? 0), 0)
  const totalInterrupted = dailyEntries.reduce((sum, d) => sum + (d.interrupted_commands ?? 0), 0)
  const interruptionRate = totalCommands > 0 ? (totalInterrupted / totalCommands) * 100 : 0

  // Steps per command
  const stepsPerCommand = interactions.user_commands_analyzed > 0
    ? interactions.total_assistant_steps / interactions.user_commands_analyzed
    : 0

  // Cache and error stats
  const cache = stats.cache ?? { hit_rate: 0 }
  const errors = stats.errors ?? { rate: 0 }

  return (
    <div className="grid grid-cols-2 lg:grid-cols-4 gap-4">
      {/* Row 1 */}
      <StatCard
        icon={<IconHash size={18} />}
        label="Total Tokens"
        value={formatNumber(totalTokens)}
        sublabel={`In: ${formatNumber(tokens.input)} / Out: ${formatNumber(tokens.output)}`}
      />
      <StatCard
        icon={<IconCurrencyDollar size={18} />}
        label="Total Cost"
        value={formatCost(stats.overview.total_cost ?? 0)}
        sublabel={costSublabel}
        badge={staleBadge}
      />
      <StatCard
        icon={<IconTerminal2 size={18} />}
        label="Commands"
        value={formatNumber(interactions.user_commands_analyzed)}
        sublabel={`Avg ${(interactions.avg_tools_per_command ?? 0).toFixed(1)} tools/cmd`}
      />
      <StatCard
        icon={<IconMessageCircle size={18} />}
        label="Models Used"
        value={String(modelsCount)}
        sublabel={Object.keys(stats.models ?? {}).slice(0, 2).join(', ')}
      />

      {/* Row 2 */}
      <StatCard
        icon={<IconHandStop size={18} />}
        label="Interruption Rate"
        value={formatPercent(interruptionRate)}
        sublabel={`${totalInterrupted} of ${totalCommands} commands`}
      />
      <StatCard
        icon={<IconRoute size={18} />}
        label="Steps / Command"
        value={stepsPerCommand.toFixed(1)}
        sublabel={`${formatNumber(interactions.total_assistant_steps)} total steps`}
      />
      <StatCard
        icon={<IconTool size={18} />}
        label="Tool Use Rate"
        value={formatPercent(interactions.percentage_requiring_tools ?? 0)}
        sublabel={`${formatNumber(interactions.commands_requiring_tools ?? 0)} cmds with tools`}
      />
      <StatCard
        icon={<IconDatabase size={18} />}
        label="Cache Hit Rate"
        value={formatPercent(cache.hit_rate ?? 0)}
        sublabel={
          errors.rate != null
            ? `Error rate: ${formatPercent(errors.rate)}`
            : undefined
        }
      />
    </div>
  )
}
