import Badge from '../common/Badge'
import type { SessionEfficiency } from '../../types/api'

interface SessionEfficiencyTableProps {
  data: SessionEfficiency[] | null | undefined
}

type BadgeColor = 'blue' | 'green' | 'yellow' | 'red' | 'purple' | 'gray'

const CLASSIFICATION_COLOR: Record<string, BadgeColor> = {
  'edit-heavy': 'green',
  'research-heavy': 'blue',
  'balanced': 'gray',
  'idle-heavy': 'yellow',
}

function shortSession(sid: string): string {
  return sid.length > 12 ? sid.slice(0, 8) : sid
}

function formatDuration(seconds: number): string {
  if (!seconds || seconds < 1) return '—'
  if (seconds < 60) return `${seconds.toFixed(0)}s`
  const m = seconds / 60
  if (m < 60) return `${m.toFixed(1)}m`
  const h = m / 60
  return `${h.toFixed(1)}h`
}

function formatPct(ratio: number): string {
  return `${(ratio * 100).toFixed(0)}%`
}

export default function SessionEfficiencyTable({ data }: SessionEfficiencyTableProps) {
  if (!data || data.length === 0) {
    return (
      <div className="bg-gray-800/50 rounded-lg p-4 border border-gray-800">
        <h3 className="text-sm font-medium text-gray-300 mb-3">Session Efficiency</h3>
        <div className="text-xs text-gray-500 py-8 text-center">No session efficiency data yet</div>
      </div>
    )
  }

  return (
    <div className="bg-gray-800/50 rounded-lg p-4 border border-gray-800">
      <h3 className="text-sm font-medium text-gray-300 mb-3">
        Session Efficiency
        <span className="ml-2 text-xs text-gray-500 font-normal">{data.length} sessions</span>
      </h3>
      <div className="bg-gray-800/30 rounded border border-gray-800 overflow-hidden">
        <div className="overflow-x-auto">
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-gray-800 text-gray-400 text-xs uppercase tracking-wider">
                <th className="px-3 py-2 text-left w-28">Session</th>
                <th className="px-3 py-2 text-left w-32">Class</th>
                <th className="px-3 py-2 text-right w-16">Edit</th>
                <th className="px-3 py-2 text-right w-16">Read</th>
                <th className="px-3 py-2 text-right w-16">Search</th>
                <th className="px-3 py-2 text-right w-16">Bash</th>
                <th className="px-3 py-2 text-right w-20">Idle Total</th>
                <th className="px-3 py-2 text-right w-20">Idle Max</th>
              </tr>
            </thead>
            <tbody>
              {data.map((s) => {
                const color = CLASSIFICATION_COLOR[s.classification] ?? 'gray'
                return (
                  <tr key={s.session_id} className="border-b border-gray-800/50">
                    <td className="px-3 py-2 text-gray-300 font-mono text-xs">
                      {shortSession(s.session_id)}
                    </td>
                    <td className="px-3 py-2">
                      <Badge color={color}>{s.classification}</Badge>
                    </td>
                    <td className="px-3 py-2 text-right text-gray-300 tabular-nums">
                      {formatPct(s.edit_ratio)}
                    </td>
                    <td className="px-3 py-2 text-right text-gray-300 tabular-nums">
                      {formatPct(s.read_ratio)}
                    </td>
                    <td className="px-3 py-2 text-right text-gray-300 tabular-nums">
                      {formatPct(s.search_ratio)}
                    </td>
                    <td className="px-3 py-2 text-right text-gray-300 tabular-nums">
                      {formatPct(s.bash_ratio)}
                    </td>
                    <td className="px-3 py-2 text-right text-gray-400 tabular-nums">
                      {formatDuration(s.idle_gap_total_s)}
                    </td>
                    <td className="px-3 py-2 text-right text-gray-400 tabular-nums">
                      {formatDuration(s.idle_gap_max_s)}
                    </td>
                  </tr>
                )
              })}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  )
}
