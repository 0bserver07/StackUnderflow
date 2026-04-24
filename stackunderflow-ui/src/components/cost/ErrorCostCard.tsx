import { useState } from 'react'
import { IconAlertTriangle, IconChevronDown, IconChevronUp, IconExternalLink } from '@tabler/icons-react'
import type { ErrorCost, OutlierCommand } from '../../types/api'
import { openInteraction } from '../../services/navigation'

// The shared `../common/ExpandableRow` primitive is `<tr>`-based and intended
// for tables. This card is a flex/grid layout, so we use an inline disclosure
// with the same UX (chevron toggle + scrollable detail panel + Show all/Show fewer).

interface ErrorCostCardProps {
  errorCost: ErrorCost | null | undefined
}

const COLLAPSED_TOOL_COUNT = 6

function formatCost(cost: number): string {
  if (cost >= 100) return `$${cost.toFixed(0)}`
  if (cost >= 1) return `$${cost.toFixed(2)}`
  return `$${cost.toFixed(4)}`
}

function formatTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`
  return n.toLocaleString()
}

function truncate(s: string, max = 64): string {
  if (!s) return ''
  return s.length > max ? `${s.slice(0, max - 1).trimEnd()}…` : s
}

interface ToolBarListProps {
  rows: Array<[string, number]>
  maxCount: number
  scrollable?: boolean
}

function ToolBarList({ rows, maxCount, scrollable = false }: ToolBarListProps) {
  const containerClass = scrollable
    ? 'space-y-1.5 max-h-[300px] overflow-y-auto pr-1'
    : 'space-y-1.5'
  return (
    <div className={containerClass}>
      {rows.map(([tool, count]) => {
        const pct = maxCount > 0 ? (count / maxCount) * 100 : 0
        return (
          <div key={tool} className="flex items-center gap-2 text-xs">
            <span className="w-24 text-gray-700 dark:text-gray-300 truncate" title={tool}>{tool}</span>
            <div className="flex-1 h-1.5 bg-white dark:bg-gray-800 rounded-full overflow-hidden">
              <div className="h-full bg-red-500/70" style={{ width: `${pct}%` }} />
            </div>
            <span className="text-gray-600 dark:text-gray-400 tabular-nums w-10 text-right">{count}</span>
          </div>
        )
      })}
    </div>
  )
}

interface TopErrorCommandRowProps {
  cmd: OutlierCommand
}

function TopErrorCommandRow({ cmd }: TopErrorCommandRowProps) {
  const errorCount = cmd.tool_count ?? 0
  const cost = cmd.cost ?? 0
  return (
    <button
      type="button"
      onClick={() => openInteraction(cmd.interaction_id)}
      className="w-full flex items-start gap-2 px-2 py-1.5 rounded hover:bg-red-900/20 focus:outline-none focus:bg-red-900/20 text-left transition-colors group"
      title="Open in Messages"
    >
      <IconExternalLink
        size={12}
        className="text-gray-500 group-hover:text-red-300 mt-1 flex-shrink-0"
        aria-hidden="true"
      />
      <span className="flex-1 text-xs text-gray-700 dark:text-gray-300 truncate" title={cmd.prompt_preview ?? ''}>
        {truncate(cmd.prompt_preview ?? '(no prompt)', 72)}
      </span>
      <span className="text-[10px] text-red-700 dark:text-red-300 tabular-nums whitespace-nowrap">
        {errorCount} err
      </span>
      <span className="text-[10px] text-gray-600 dark:text-gray-400 tabular-nums whitespace-nowrap w-14 text-right">
        {formatCost(cost)}
      </span>
    </button>
  )
}

export default function ErrorCostCard({ errorCost }: ErrorCostCardProps) {
  const [expanded, setExpanded] = useState(false)
  const [topErrorsExpanded, setTopErrorsExpanded] = useState(false)

  if (!errorCost || errorCost.total_errors === 0) {
    return (
      <div className="bg-gradient-to-br from-red-900/20 to-gray-50/50 dark:to-gray-900/30 rounded-lg p-6 border border-red-900/30">
        <div className="flex items-center gap-2 mb-3">
          <IconAlertTriangle size={18} className="text-red-400" />
          <span className="text-xs text-gray-600 dark:text-gray-400 uppercase tracking-wider">Error Cost</span>
        </div>
        <div className="text-gray-500 text-sm">No errors recorded.</div>
      </div>
    )
  }

  const totals = errorCost.total_errors ?? 0
  const retryCost = errorCost.estimated_retry_cost ?? 0
  const retryTokens = errorCost.estimated_retry_tokens ?? 0

  const allByTool = Object.entries(errorCost.errors_by_tool ?? {})
    .filter(([, c]) => c > 0)
    .sort((a, b) => b[1] - a[1])
  const collapsedByTool = allByTool.slice(0, COLLAPSED_TOOL_COUNT)
  const overflowToolCount = Math.max(0, allByTool.length - COLLAPSED_TOOL_COUNT)
  const maxToolCount = allByTool.length > 0 ? Math.max(...allByTool.map(([, c]) => c)) : 0
  const distinctToolCount = allByTool.length

  const topErrorCommands = errorCost.top_error_commands ?? []
  const collapsedTopErrors = topErrorCommands.slice(0, 3)
  const overflowTopErrors = Math.max(0, topErrorCommands.length - 3)

  return (
    <div className="bg-gradient-to-br from-red-900/30 to-gray-50/60 dark:to-gray-900/40 rounded-lg p-6 border border-red-900/40">
      <div className="flex items-center gap-2 mb-3">
        <IconAlertTriangle size={18} className="text-red-400" />
        <span className="text-xs text-gray-600 dark:text-gray-400 uppercase tracking-wider">Error Cost</span>
      </div>

      {/* Hero — wasted retry $ is the lede; total error count is supporting detail. */}
      <div>
        <div className="text-4xl font-bold text-red-700 dark:text-red-300 leading-none tabular-nums">
          {formatCost(retryCost)}
        </div>
        <div className="text-xs text-gray-600 dark:text-gray-400 mt-2">
          wasted on{' '}
          <span className="text-red-700 dark:text-red-200 font-medium">{totals.toLocaleString()}</span> error
          {totals === 1 ? '' : 's'} across{' '}
          <span className="text-red-700 dark:text-red-200 font-medium">{distinctToolCount}</span> tool
          {distinctToolCount === 1 ? '' : 's'}
        </div>
        <div className="text-[10px] text-gray-500 mt-1 tabular-nums">
          ≈ {formatTokens(retryTokens)} retry tokens
        </div>
      </div>

      {allByTool.length > 0 && (
        <div className="mt-5 pt-4 border-t border-red-900/30">
          <div className="text-[10px] text-gray-500 uppercase tracking-wider mb-2">Errors by tool</div>
          <ToolBarList
            rows={expanded ? allByTool : collapsedByTool}
            maxCount={maxToolCount}
            scrollable={expanded}
          />
          {(overflowToolCount > 0 || expanded) && (
            <button
              type="button"
              onClick={() => setExpanded((v) => !v)}
              className="mt-2 inline-flex items-center gap-1 text-[11px] text-red-700/80 dark:text-red-300/80 hover:text-red-800 dark:hover:text-red-200 focus:outline-none"
              aria-expanded={expanded}
            >
              {expanded ? (
                <>
                  <IconChevronUp size={12} aria-hidden="true" />
                  Show fewer
                </>
              ) : (
                <>
                  <IconChevronDown size={12} aria-hidden="true" />
                  Show all {allByTool.length} tools
                </>
              )}
            </button>
          )}
        </div>
      )}

      {topErrorCommands.length > 0 && (
        <div className="mt-5 pt-4 border-t border-red-900/30">
          <div className="text-[10px] text-gray-500 uppercase tracking-wider mb-2">
            Top error commands
          </div>
          <div className="space-y-0.5">
            {(topErrorsExpanded ? topErrorCommands : collapsedTopErrors).map((cmd) => (
              <TopErrorCommandRow key={cmd.interaction_id} cmd={cmd} />
            ))}
          </div>
          {(overflowTopErrors > 0 || topErrorsExpanded) && (
            <button
              type="button"
              onClick={() => setTopErrorsExpanded((v) => !v)}
              className="mt-2 inline-flex items-center gap-1 text-[11px] text-red-700/80 dark:text-red-300/80 hover:text-red-800 dark:hover:text-red-200 focus:outline-none"
              aria-expanded={topErrorsExpanded}
            >
              {topErrorsExpanded ? (
                <>
                  <IconChevronUp size={12} aria-hidden="true" />
                  Show fewer
                </>
              ) : (
                <>
                  <IconChevronDown size={12} aria-hidden="true" />
                  Show all {topErrorCommands.length} commands
                </>
              )}
            </button>
          )}
        </div>
      )}
    </div>
  )
}

