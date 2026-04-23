import type { CommandCost } from '../../types/api'
import { IconAlertTriangle } from '@tabler/icons-react'

interface CommandCostListProps {
  data: CommandCost[]
  onOpen?: (interactionId: string) => void
}

function formatCost(cost: number): string {
  if (cost >= 100) return `$${cost.toFixed(0)}`
  if (cost >= 1) return `$${cost.toFixed(2)}`
  return `$${cost.toFixed(4)}`
}

function formatTokens(t: Record<string, number> | undefined): string {
  if (!t) return '0'
  const total =
    (t.input ?? 0) +
    (t.output ?? 0) +
    (t.cache_read ?? 0) +
    (t.cache_creation ?? 0)
  if (total >= 1_000_000) return `${(total / 1_000_000).toFixed(1)}M`
  if (total >= 1_000) return `${(total / 1_000).toFixed(1)}K`
  return total.toLocaleString()
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

export default function CommandCostList({ data, onOpen }: CommandCostListProps) {
  if (!data || data.length === 0) {
    return (
      <div className="bg-gray-800/50 rounded-lg p-4 border border-gray-800">
        <h3 className="text-sm font-medium text-gray-300 mb-3">Most Expensive Commands</h3>
        <div className="text-xs text-gray-500 py-8 text-center">No command cost data yet</div>
      </div>
    )
  }

  const rows = [...data].sort((a, b) => b.cost - a.cost)

  return (
    <div className="bg-gray-800/50 rounded-lg p-4 border border-gray-800">
      <h3 className="text-sm font-medium text-gray-300 mb-3">
        Most Expensive Commands
        <span className="ml-2 text-xs text-gray-500 font-normal">top {rows.length}</span>
      </h3>
      <div className="bg-gray-800/30 rounded border border-gray-800 overflow-hidden">
        <div className="overflow-x-auto">
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-gray-800 text-gray-400 text-xs uppercase tracking-wider">
                <th className="px-3 py-2 text-left">Prompt</th>
                <th className="px-3 py-2 text-left w-32">When</th>
                <th className="px-3 py-2 text-right w-20">Cost</th>
                <th className="px-3 py-2 text-right w-20">Tokens</th>
                <th className="px-3 py-2 text-right w-16">Tools</th>
                <th className="px-3 py-2 text-right w-16">Steps</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((r) => (
                <tr
                  key={r.interaction_id}
                  className={`border-b border-gray-800/50 ${onOpen ? 'hover:bg-gray-800/50 cursor-pointer' : ''}`}
                  onClick={() => onOpen?.(r.interaction_id)}
                >
                  <td className="px-3 py-2 text-gray-200 max-w-md">
                    <div className="flex items-start gap-1.5">
                      {r.had_error && (
                        <IconAlertTriangle size={12} className="text-red-400 mt-0.5 flex-shrink-0" />
                      )}
                      <span className="truncate block" title={r.prompt_preview}>
                        {r.prompt_preview || <span className="text-gray-500 italic">(empty prompt)</span>}
                      </span>
                    </div>
                  </td>
                  <td className="px-3 py-2 text-gray-500 text-xs whitespace-nowrap">
                    {formatTime(r.timestamp)}
                  </td>
                  <td className="px-3 py-2 text-right text-gray-100 font-medium tabular-nums">
                    {formatCost(r.cost)}
                  </td>
                  <td className="px-3 py-2 text-right text-gray-300 tabular-nums">
                    {formatTokens(r.tokens)}
                  </td>
                  <td className="px-3 py-2 text-right text-gray-300 tabular-nums">
                    {r.tools_used}
                  </td>
                  <td className="px-3 py-2 text-right text-gray-300 tabular-nums">
                    {r.steps}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  )
}
