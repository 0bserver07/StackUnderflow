import { useQuery } from '@tanstack/react-query'
import { IconBrain, IconAlertCircle } from '@tabler/icons-react'
import { getContextBudget } from '../../services/api'
import { formatCost, formatNumber } from '../../services/format'
import { useCurrency } from '../../services/currency'

// ---------------------------------------------------------------------------
// ContextBudgetCard — v0.6.0 follow-up.
//
// Surfaces `GET /api/context-budget` (global view, no project scope) inside
// the Settings page. The estimator is heuristic by design — a flat 4
// chars-per-token plus a per-MCP-server fee — so we render the heuristic
// caveat as a footer that's hard to miss.
//
// `cost_per_session_usd` and `estimated_monthly_cost_usd` come from the
// route in *raw USD* (no currency conversion at the route boundary). We
// convert here using the active currency rate so the figures align with
// every other cost in the dashboard.
// ---------------------------------------------------------------------------

const TOP_SLICES = 5

export default function ContextBudgetCard() {
  const { currency } = useCurrency()
  const { data, isLoading, error } = useQuery({
    queryKey: ['contextBudget'],
    queryFn: getContextBudget,
    // Stable: rebuilds only when the user adds/removes an MCP server, edits
    // CLAUDE.md, or installs a skill. 5 minutes is plenty for a settings card.
    staleTime: 5 * 60_000,
  })

  const rate = currency?.rate_from_usd ?? 1.0
  const costPerSession = (data?.cost_per_session_usd ?? 0) * rate
  const monthlyEstimate = (data?.estimated_monthly_cost_usd ?? 0) * rate

  // Top N slices by token count — the route emits them in insertion order
  // so we resort by tokens here rather than depend on the route ordering.
  const topSlices = (data?.slices ?? [])
    .slice()
    .sort((a, b) => b.tokens - a.tokens)
    .slice(0, TOP_SLICES)

  return (
    <section className="bg-white dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-800 p-5">
      <div className="flex items-center gap-2">
        <IconBrain size={18} className="text-gray-600 dark:text-gray-400" />
        <h2 className="text-base font-semibold text-gray-900 dark:text-gray-100">Context budget</h2>
      </div>
      <p className="text-xs text-gray-500 mt-1">
        Per-session "context tax" — the system prompt, registered MCP servers, available skills,
        agent definitions, and global memory files paid on every turn before the user types.
      </p>

      {isLoading && (
        <div className="mt-4 text-xs text-gray-500">Estimating context budget…</div>
      )}

      {error && (
        <div className="mt-4 text-xs text-red-600 dark:text-red-400">
          Failed to load context budget: {error instanceof Error ? error.message : 'Unknown error'}
        </div>
      )}

      {!isLoading && !error && data && (
        <>
          <div className="grid grid-cols-3 gap-3 mt-4">
            <div className="bg-gray-50 dark:bg-gray-800/50 rounded-md p-3 border border-gray-200 dark:border-gray-800">
              <div className="text-[10px] uppercase tracking-wider text-gray-500 font-medium">Total tokens</div>
              <div className="text-lg font-bold text-gray-900 dark:text-gray-100 mt-1 tabular-nums">
                {formatNumber(data.total_tokens)}
              </div>
            </div>
            <div className="bg-gray-50 dark:bg-gray-800/50 rounded-md p-3 border border-gray-200 dark:border-gray-800">
              <div className="text-[10px] uppercase tracking-wider text-gray-500 font-medium">$/session</div>
              <div className="text-lg font-bold text-gray-900 dark:text-gray-100 mt-1 tabular-nums">
                {formatCost(costPerSession, currency)}
              </div>
            </div>
            <div className="bg-gray-50 dark:bg-gray-800/50 rounded-md p-3 border border-gray-200 dark:border-gray-800">
              <div className="text-[10px] uppercase tracking-wider text-gray-500 font-medium">Monthly est.</div>
              <div className="text-lg font-bold text-gray-900 dark:text-gray-100 mt-1 tabular-nums">
                {formatCost(monthlyEstimate, currency)}
              </div>
            </div>
          </div>

          {topSlices.length > 0 && (
            <div className="mt-4">
              <div className="text-[10px] uppercase tracking-wider text-gray-500 font-medium mb-2">
                Top {topSlices.length} slices
              </div>
              <div className="overflow-hidden rounded border border-gray-200 dark:border-gray-800">
                <table className="w-full text-sm">
                  <thead className="bg-gray-50 dark:bg-gray-800/60">
                    <tr>
                      <th className="text-left px-3 py-2 text-[10px] uppercase tracking-wider text-gray-500 font-medium">
                        Source
                      </th>
                      <th className="text-right px-3 py-2 text-[10px] uppercase tracking-wider text-gray-500 font-medium">
                        Tokens
                      </th>
                      <th className="text-left px-3 py-2 text-[10px] uppercase tracking-wider text-gray-500 font-medium">
                        Path
                      </th>
                    </tr>
                  </thead>
                  <tbody>
                    {topSlices.map((slice, i) => (
                      <tr
                        key={`${slice.name}-${i}`}
                        className="border-t border-gray-200 dark:border-gray-800"
                      >
                        <td className="px-3 py-2 text-xs font-medium text-gray-800 dark:text-gray-200">
                          {slice.name}
                        </td>
                        <td className="px-3 py-2 text-xs tabular-nums text-gray-700 dark:text-gray-300 text-right">
                          {formatNumber(slice.tokens)}
                        </td>
                        <td className="px-3 py-2 text-[11px] font-mono text-gray-500 break-all max-w-[280px]">
                          {slice.source_path ?? '—'}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            </div>
          )}

          <div className="mt-4 flex items-start gap-1.5 text-[11px] text-gray-500 italic">
            <IconAlertCircle size={12} className="flex-shrink-0 mt-0.5" />
            <span>
              Estimates only — token counts are <span className="font-mono">len(text) // 4</span> with
              a flat per-MCP-server fee. Useful for spotting bloat, not for billing.
            </span>
          </div>
        </>
      )}
    </section>
  )
}
