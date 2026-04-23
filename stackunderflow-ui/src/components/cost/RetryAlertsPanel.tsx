import { IconAlertTriangle, IconRefresh } from '@tabler/icons-react'
import type { RetrySignal } from '../../types/api'

interface RetryAlertsPanelProps {
  signals: RetrySignal[] | null | undefined
}

function formatCost(cost: number): string {
  if (cost >= 100) return `$${cost.toFixed(0)}`
  if (cost >= 1) return `$${cost.toFixed(2)}`
  return `$${cost.toFixed(4)}`
}

function formatTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`
  if (n >= 1_000) return `${(n / 1_000).toFixed(0)}K`
  return n.toLocaleString()
}

function formatTime(iso: string): string {
  if (!iso) return ''
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  return d.toLocaleString(undefined, {
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
  })
}

/**
 * Severity per signal — 3+ consecutive failures = red, 2 = amber.
 */
function severity(sig: RetrySignal): 'red' | 'amber' {
  return sig.consecutive_failures >= 3 ? 'red' : 'amber'
}

const STYLES = {
  red: {
    wrapper: 'bg-red-900/20 border-red-800/60',
    icon: 'text-red-400',
    label: 'text-red-300',
  },
  amber: {
    wrapper: 'bg-amber-900/20 border-amber-800/60',
    icon: 'text-amber-400',
    label: 'text-amber-300',
  },
} as const

export default function RetryAlertsPanel({ signals }: RetryAlertsPanelProps) {
  if (!signals || signals.length === 0) {
    return (
      <div className="bg-gray-800/50 rounded-lg p-4 border border-gray-800">
        <h3 className="text-sm font-medium text-gray-300 mb-3">Retry Alerts</h3>
        <div className="text-xs text-gray-500 py-8 text-center">No retry storms detected — nice.</div>
      </div>
    )
  }

  const sorted = [...signals].sort(
    (a, b) => b.estimated_wasted_cost - a.estimated_wasted_cost ||
              b.consecutive_failures - a.consecutive_failures,
  )

  const totalWasted = sorted.reduce((s, sig) => s + (sig.estimated_wasted_cost ?? 0), 0)

  return (
    <div className="bg-gray-800/50 rounded-lg p-4 border border-gray-800">
      <h3 className="text-sm font-medium text-gray-300 mb-3">
        Retry Alerts
        <span className="ml-2 text-xs text-gray-500 font-normal">
          {sorted.length} signal{sorted.length === 1 ? '' : 's'} · ~{formatCost(totalWasted)} wasted
        </span>
      </h3>
      <div className="space-y-2">
        {sorted.map((sig, idx) => {
          const style = STYLES[severity(sig)]
          return (
            <div
              key={`${sig.interaction_id}-${sig.tool}-${idx}`}
              className={`flex items-start gap-3 p-3 rounded border ${style.wrapper}`}
            >
              <IconAlertTriangle size={16} className={`${style.icon} mt-0.5 flex-shrink-0`} />
              <div className="flex-1 min-w-0">
                <div className="flex items-center gap-2 flex-wrap">
                  <span className={`text-sm font-medium ${style.label}`}>
                    {sig.tool}
                  </span>
                  <span className="inline-flex items-center gap-1 text-[10px] text-gray-400 bg-gray-800/80 border border-gray-700 rounded-full px-2 py-0.5">
                    <IconRefresh size={10} />
                    {sig.consecutive_failures}× failed · {sig.total_invocations} total
                  </span>
                  <span className="text-[10px] text-gray-500">{formatTime(sig.timestamp)}</span>
                </div>
                <div className="text-xs text-gray-400 mt-1 tabular-nums">
                  ~{formatTokens(sig.estimated_wasted_tokens)} wasted tokens · {formatCost(sig.estimated_wasted_cost)} wasted cost
                </div>
              </div>
            </div>
          )
        })}
      </div>
    </div>
  )
}
