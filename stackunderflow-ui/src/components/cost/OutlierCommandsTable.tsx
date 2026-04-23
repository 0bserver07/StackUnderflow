import { IconTool, IconRoute } from '@tabler/icons-react'
import type { Outliers, OutlierCommand } from '../../types/api'

interface OutlierCommandsTableProps {
  outliers: Outliers | null | undefined
}

function formatCost(cost: number): string {
  if (cost >= 100) return `$${cost.toFixed(0)}`
  if (cost >= 1) return `$${cost.toFixed(2)}`
  return `$${cost.toFixed(4)}`
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

interface OutlierSectionProps {
  title: string
  icon: React.ReactNode
  rows: OutlierCommand[]
  countKey: 'tool_count' | 'step_count'
  countLabel: string
  empty: string
}

function OutlierSection({ title, icon, rows, countKey, countLabel, empty }: OutlierSectionProps) {
  return (
    <div>
      <div className="flex items-center gap-1.5 text-xs text-gray-400 mb-2">
        <span className="text-gray-500">{icon}</span>
        <span className="font-medium uppercase tracking-wider">{title}</span>
        <span className="text-gray-600">({rows.length})</span>
      </div>
      {rows.length === 0 ? (
        <div className="text-xs text-gray-500 py-4 px-3 bg-gray-800/30 rounded border border-gray-800">
          {empty}
        </div>
      ) : (
        <div className="bg-gray-800/30 rounded border border-gray-800 overflow-hidden">
          <div className="overflow-x-auto">
            <table className="w-full text-sm">
              <thead>
                <tr className="border-b border-gray-800 text-gray-400 text-xs uppercase tracking-wider">
                  <th className="px-3 py-2 text-left">Prompt</th>
                  <th className="px-3 py-2 text-left w-28">When</th>
                  <th className="px-3 py-2 text-right w-20">{countLabel}</th>
                  <th className="px-3 py-2 text-right w-20">Cost</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((r) => (
                  <tr key={r.interaction_id} className="border-b border-gray-800/50">
                    <td className="px-3 py-2 text-gray-200 max-w-md">
                      <span className="truncate block" title={r.prompt_preview}>
                        {r.prompt_preview || <span className="text-gray-500 italic">(empty prompt)</span>}
                      </span>
                    </td>
                    <td className="px-3 py-2 text-gray-500 text-xs whitespace-nowrap">
                      {formatTime(r.timestamp)}
                    </td>
                    <td className="px-3 py-2 text-right text-gray-100 font-medium tabular-nums">
                      {r[countKey]}
                    </td>
                    <td className="px-3 py-2 text-right text-gray-300 tabular-nums">
                      {formatCost(r.cost)}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}
    </div>
  )
}

export default function OutlierCommandsTable({ outliers }: OutlierCommandsTableProps) {
  const highTool = outliers?.high_tool_commands ?? []
  const highStep = outliers?.high_step_commands ?? []

  if (!outliers || (highTool.length === 0 && highStep.length === 0)) {
    return (
      <div className="bg-gray-800/50 rounded-lg p-4 border border-gray-800">
        <h3 className="text-sm font-medium text-gray-300 mb-3">Outlier Commands</h3>
        <div className="text-xs text-gray-500 py-8 text-center">No outlier commands — nothing exceeded the thresholds.</div>
      </div>
    )
  }

  return (
    <div className="bg-gray-800/50 rounded-lg p-4 border border-gray-800">
      <h3 className="text-sm font-medium text-gray-300 mb-3">
        Outlier Commands
        <span className="ml-2 text-xs text-gray-500 font-normal">
          tool-count &gt; 20 · step-count &gt; 15
        </span>
      </h3>
      <div className="space-y-4">
        <OutlierSection
          title="High tool count"
          icon={<IconTool size={12} />}
          rows={highTool}
          countKey="tool_count"
          countLabel="Tools"
          empty="No commands exceeded 20 tool calls."
        />
        <OutlierSection
          title="High step count"
          icon={<IconRoute size={12} />}
          rows={highStep}
          countKey="step_count"
          countLabel="Steps"
          empty="No commands exceeded 15 assistant steps."
        />
      </div>
    </div>
  )
}
