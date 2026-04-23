import { IconAlertTriangle } from '@tabler/icons-react'
import type { ErrorCost } from '../../types/api'

interface ErrorCostCardProps {
  errorCost: ErrorCost | null | undefined
}

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

export default function ErrorCostCard({ errorCost }: ErrorCostCardProps) {
  if (!errorCost || errorCost.total_errors === 0) {
    return (
      <div className="bg-gradient-to-br from-red-900/20 to-gray-900/30 rounded-lg p-6 border border-red-900/30">
        <div className="flex items-center gap-2 mb-3">
          <IconAlertTriangle size={18} className="text-red-400" />
          <span className="text-xs text-gray-400 uppercase tracking-wider">Error Cost</span>
        </div>
        <div className="text-gray-500 text-sm">No errors recorded.</div>
      </div>
    )
  }

  const totals = errorCost.total_errors ?? 0
  const retryCost = errorCost.estimated_retry_cost ?? 0
  const retryTokens = errorCost.estimated_retry_tokens ?? 0
  const byTool = Object.entries(errorCost.errors_by_tool ?? {})
    .filter(([, c]) => c > 0)
    .sort((a, b) => b[1] - a[1])
    .slice(0, 6)

  const maxToolCount = byTool.length > 0 ? Math.max(...byTool.map(([, c]) => c)) : 0

  return (
    <div className="bg-gradient-to-br from-red-900/30 to-gray-900/40 rounded-lg p-6 border border-red-900/40">
      <div className="flex items-center gap-2 mb-3">
        <IconAlertTriangle size={18} className="text-red-400" />
        <span className="text-xs text-gray-400 uppercase tracking-wider">Error Cost</span>
      </div>

      <div className="flex items-baseline gap-6">
        <div>
          <div className="text-4xl font-bold text-red-300 leading-none">{totals.toLocaleString()}</div>
          <div className="text-xs text-gray-500 mt-1">total errors</div>
        </div>
        <div>
          <div className="text-2xl font-semibold text-gray-100 leading-none">{formatCost(retryCost)}</div>
          <div className="text-xs text-gray-500 mt-1">est. retry cost · {formatTokens(retryTokens)} tokens</div>
        </div>
      </div>

      {byTool.length > 0 && (
        <div className="mt-5 pt-4 border-t border-red-900/30 space-y-1.5">
          <div className="text-[10px] text-gray-500 uppercase tracking-wider mb-2">Errors by tool</div>
          {byTool.map(([tool, count]) => {
            const pct = maxToolCount > 0 ? (count / maxToolCount) * 100 : 0
            return (
              <div key={tool} className="flex items-center gap-2 text-xs">
                <span className="w-24 text-gray-300 truncate" title={tool}>{tool}</span>
                <div className="flex-1 h-1.5 bg-gray-800 rounded-full overflow-hidden">
                  <div
                    className="h-full bg-red-500/70"
                    style={{ width: `${pct}%` }}
                  />
                </div>
                <span className="text-gray-400 tabular-nums w-10 text-right">{count}</span>
              </div>
            )
          })}
        </div>
      )}
    </div>
  )
}
