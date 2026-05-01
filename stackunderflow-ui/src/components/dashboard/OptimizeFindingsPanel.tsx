import { useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import {
  IconAlertHexagon,
  IconChevronDown,
  IconChevronRight,
  IconBulb,
} from '@tabler/icons-react'
import { getOptimize } from '../../services/api'
import type { Finding, FindingSeverity } from '../../types/api'
import Badge from '../common/Badge'
import { formatNumber } from '../../services/format'

// ---------------------------------------------------------------------------
// OptimizeFindingsPanel — v0.6.0 follow-up.
//
// Surfaces `GET /api/optimize?period=month`. The route already returns
// findings sorted by severity desc; we render the top 5 collapsed and let
// the user expand to see the rest. Period is fixed to `month` because that
// matches the spec brief; if we add a control later, share the inline one
// the Compare/Yield tabs use.
// ---------------------------------------------------------------------------

const TOP_N = 5

const SEVERITY_COLOR: Record<FindingSeverity, 'red' | 'yellow' | 'gray'> = {
  high: 'red',
  medium: 'yellow',
  low: 'gray',
}

interface FindingRowProps {
  finding: Finding
}

function FindingRow({ finding }: FindingRowProps) {
  return (
    <div className="px-4 py-3 border-t border-gray-100 dark:border-gray-800 flex items-start gap-3">
      <div className="flex-shrink-0 pt-0.5">
        <Badge color={SEVERITY_COLOR[finding.severity]} size="sm">
          {finding.severity}
        </Badge>
      </div>
      <div className="flex-1 min-w-0">
        <div className="flex items-baseline gap-2 flex-wrap">
          <h4 className="text-sm font-medium text-gray-900 dark:text-gray-100">
            {finding.title}
          </h4>
          <span className="text-[11px] text-gray-500 tabular-nums">
            {finding.affected_count} affected
          </span>
          {finding.estimated_waste_tokens !== null && finding.estimated_waste_tokens > 0 && (
            <span className="text-[11px] text-gray-500 tabular-nums">
              · ~{formatNumber(finding.estimated_waste_tokens)} wasted tokens
            </span>
          )}
        </div>
        {finding.description && (
          <p className="text-xs text-gray-600 dark:text-gray-400 mt-1">{finding.description}</p>
        )}
        {finding.suggested_fix && (
          <p className="text-xs text-gray-700 dark:text-gray-300 mt-1.5 flex items-start gap-1.5">
            <IconBulb size={12} className="flex-shrink-0 text-yellow-500 mt-0.5" />
            <span>{finding.suggested_fix}</span>
          </p>
        )}
      </div>
    </div>
  )
}

export default function OptimizeFindingsPanel() {
  const [expanded, setExpanded] = useState(false)
  const { data, isLoading, error } = useQuery({
    queryKey: ['optimize', 'month'],
    queryFn: () => getOptimize('month'),
    staleTime: 5 * 60_000,
  })

  // Hide entirely while in flight; the panel is supplementary, not primary,
  // so flashing a loading spinner above the existing stats cards is noise.
  if (isLoading || error) return null
  if (!data) return null

  const all = data.patterns ?? []
  const total = all.length
  const visible = expanded ? all : all.slice(0, TOP_N)
  const hidden = Math.max(0, total - TOP_N)

  // Suppress when there's nothing to show — the panel is opt-in noise
  // otherwise, and the stats cards already explain "things look fine".
  if (total === 0) return null

  return (
    <div className="bg-white dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-800">
      <button
        type="button"
        onClick={() => setExpanded(!expanded)}
        className="w-full flex items-center justify-between gap-3 px-4 py-3 hover:bg-gray-50 dark:hover:bg-gray-800/40"
        aria-expanded={expanded}
      >
        <div className="flex items-center gap-2">
          {expanded ? (
            <IconChevronDown size={14} className="text-gray-500" />
          ) : (
            <IconChevronRight size={14} className="text-gray-500" />
          )}
          <IconAlertHexagon size={16} className="text-gray-500" />
          <span className="text-sm font-semibold text-gray-800 dark:text-gray-200">
            Optimization findings
          </span>
          <span className="text-xs text-gray-500">
            {total} pattern{total === 1 ? '' : 's'} detected · {data.scope}
          </span>
        </div>
      </button>

      {/* Always render at least the top N so the panel isn't a one-line tease;
          expand toggle reveals the remainder when present. */}
      <div>
        {visible.map((f, i) => (
          <FindingRow key={`${f.pattern_id}-${i}`} finding={f} />
        ))}
      </div>

      {hidden > 0 && (
        <button
          type="button"
          onClick={() => setExpanded(true)}
          disabled={expanded}
          className="w-full px-4 py-2.5 text-xs text-indigo-600 dark:text-indigo-400 hover:bg-gray-50 dark:hover:bg-gray-800/40 border-t border-gray-100 dark:border-gray-800 disabled:text-gray-400 disabled:cursor-default"
        >
          {expanded ? `Showing all ${total}` : `View all ${total} findings`}
        </button>
      )}
    </div>
  )
}
