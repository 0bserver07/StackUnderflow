import { Fragment, useCallback, useMemo, useState } from 'react'
import {
  IconAlertTriangle,
  IconArrowDown,
  IconArrowUp,
  IconChevronDown,
  IconChevronRight,
} from '@tabler/icons-react'
import type { CommandCost } from '../../types/api'
import Badge from '../common/Badge'
import { openInteraction } from '../../services/navigation'

type SortDir = 'asc' | 'desc'
type SortKey = 'cost' | 'tokens' | 'tools' | 'steps' | 'when'

const SORT_LABELS: Record<SortKey, string> = {
  cost: 'cost',
  tokens: 'tokens',
  tools: 'tools',
  steps: 'steps',
  when: 'when',
}

interface CommandCostListProps {
  data: CommandCost[]
  /** Backwards-compat callback. If omitted, rows call `openInteraction` directly. */
  onOpen?: (interactionId: string) => void
  /** Optional override for initial sort. Defaults to cost-desc. */
  initialSort?: { key: SortKey; dir: SortDir }
}

// ---------------- helpers ----------------

function totalTokens(t: Record<string, number> | undefined): number {
  if (!t) return 0
  return (
    (t.input ?? 0) +
    (t.output ?? 0) +
    (t.cache_read ?? 0) +
    (t.cache_creation ?? 0)
  )
}

function formatCost(cost: number): string {
  if (cost >= 100) return `$${cost.toFixed(0)}`
  if (cost >= 1) return `$${cost.toFixed(2)}`
  return `$${cost.toFixed(4)}`
}

function formatTokenCount(total: number): string {
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

function sortValue(row: CommandCost, key: SortKey): number | string {
  switch (key) {
    case 'cost': return row.cost ?? 0
    case 'tokens': return totalTokens(row.tokens)
    case 'tools': return row.tools_used ?? 0
    case 'steps': return row.steps ?? 0
    case 'when': return row.timestamp ?? ''
  }
}

function compareValues(a: number | string, b: number | string, dir: SortDir): number {
  // Nullish-ish (empty strings sort last in asc, first in desc — parity with hook spec)
  const aMissing = a === '' || a == null
  const bMissing = b === '' || b == null
  if (aMissing && bMissing) return 0
  if (aMissing) return 1
  if (bMissing) return -1
  if (typeof a === 'number' && typeof b === 'number') {
    return dir === 'asc' ? a - b : b - a
  }
  const sa = String(a)
  const sb = String(b)
  return dir === 'asc' ? sa.localeCompare(sb) : sb.localeCompare(sa)
}

function median(nums: number[]): number {
  if (nums.length === 0) return 0
  const sorted = [...nums].sort((a, b) => a - b)
  const mid = Math.floor(sorted.length / 2)
  if (sorted.length % 2 === 0) {
    return ((sorted[mid - 1] ?? 0) + (sorted[mid] ?? 0)) / 2
  }
  return sorted[mid] ?? 0
}

function percentile(nums: number[], p: number): number {
  if (nums.length === 0) return 0
  const sorted = [...nums].sort((a, b) => a - b)
  // Nearest-rank method, 1-indexed.
  const rank = Math.ceil((p / 100) * sorted.length)
  const idx = Math.min(Math.max(rank - 1, 0), sorted.length - 1)
  return sorted[idx] ?? 0
}

// Local sort hook instead of the shared `hooks/useSortableTable` because the
// 'tokens' column sorts by an aggregate (input + output + cache_*), not a
// direct row property. The shared hook only supports `row[key]` access.
function useCommandCostSort(
  rows: CommandCost[],
  initialKey: SortKey,
  initialDir: SortDir,
) {
  const [sortKey, setSortKey] = useState<SortKey>(initialKey)
  const [sortDir, setSortDir] = useState<SortDir>(initialDir)
  const sorted = useMemo(() => {
    const copy = [...rows]
    copy.sort((a, b) => compareValues(sortValue(a, sortKey), sortValue(b, sortKey), sortDir))
    return copy
  }, [rows, sortKey, sortDir])
  const setSort = useCallback((k: SortKey) => {
    setSortKey((prev) => {
      if (prev === k) {
        setSortDir((d) => (d === 'asc' ? 'desc' : 'asc'))
        return prev
      }
      setSortDir('desc')
      return k
    })
  }, [])
  return { sorted, sortKey, sortDir, setSort }
}

// ---------------- sub-components ----------------

interface SortHeaderProps {
  label: string
  sortKey: SortKey
  activeKey: SortKey
  dir: SortDir
  onSort: (k: SortKey) => void
  align?: 'left' | 'right'
  className?: string
}

function SortHeader({ label, sortKey, activeKey, dir, onSort, align = 'right', className }: SortHeaderProps) {
  const active = sortKey === activeKey
  return (
    <th
      scope="col"
      className={`px-3 py-2 cursor-pointer select-none hover:text-gray-800 dark:hover:text-gray-200 ${align === 'right' ? 'text-right' : 'text-left'} ${className ?? ''}`}
      onClick={() => onSort(sortKey)}
      aria-sort={active ? (dir === 'asc' ? 'ascending' : 'descending') : 'none'}
      data-testid={`ccl-sort-${sortKey}`}
    >
      <span className="inline-flex items-center gap-0.5">
        {label}
        {active && (dir === 'asc'
          ? <IconArrowUp size={11} className="inline" />
          : <IconArrowDown size={11} className="inline" />
        )}
      </span>
    </th>
  )
}

interface ExpandedDetailProps {
  row: CommandCost
  colSpan: number
}

function ExpandedDetail({ row, colSpan }: ExpandedDetailProps) {
  return (
    <tr className="border-b border-gray-200/50 dark:border-gray-800/50 bg-gray-50/60 dark:bg-gray-900/40" data-testid="ccl-expanded">
      <td colSpan={colSpan} className="px-3 py-3">
        <div className="space-y-2">
          <div>
            <div className="text-[10px] text-gray-500 uppercase tracking-wider mb-1">Prompt</div>
            <div className="text-sm text-gray-800 dark:text-gray-200 whitespace-pre-wrap break-words">
              {row.prompt_preview || <span className="text-gray-500 italic">(empty prompt)</span>}
            </div>
          </div>
          <div className="flex flex-wrap items-center gap-2 pt-1">
            {row.had_error && (
              <Badge color="red">
                <IconAlertTriangle size={10} className="mr-1" />
                had error
              </Badge>
            )}
            {(row.models_used ?? []).map((m) => (
              <Badge key={m} color="purple">{m}</Badge>
            ))}
            {(row.models_used == null || row.models_used.length === 0) && (
              <span className="text-[10px] text-gray-500">no model recorded</span>
            )}
            <span className="text-[10px] text-gray-500 ml-auto font-mono">
              session {row.session_id.slice(0, 8)} · {formatTime(row.timestamp)}
            </span>
          </div>
        </div>
      </td>
    </tr>
  )
}

// ---------------- main component ----------------

export default function CommandCostList({ data, onOpen, initialSort }: CommandCostListProps) {
  const rows = data ?? []
  const { sorted, sortKey, sortDir, setSort } = useCommandCostSort(
    rows,
    initialSort?.key ?? 'cost',
    initialSort?.dir ?? 'desc',
  )

  const [expanded, setExpanded] = useState<Set<string>>(() => new Set())
  const [page, setPage] = useState(1)
  const [pageSize, setPageSize] = useState(25)
  const totalPages = Math.max(1, Math.ceil(sorted.length / pageSize))
  const safePage = Math.min(page, totalPages)
  const paged = useMemo(
    () => sorted.slice((safePage - 1) * pageSize, safePage * pageSize),
    [sorted, safePage, pageSize],
  )

  const totalCost = useMemo(
    () => rows.reduce((s, r) => s + (r.cost ?? 0), 0),
    [rows],
  )

  const costStats = useMemo(() => {
    const costs = rows.map((r) => r.cost ?? 0)
    return {
      sum: costs.reduce((s, c) => s + c, 0),
      median: median(costs),
      p95: percentile(costs, 95),
    }
  }, [rows])

  const toggleExpand = useCallback((id: string) => {
    setExpanded((prev) => {
      const next = new Set(prev)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })
  }, [])

  const handleRowClick = useCallback((id: string) => {
    if (onOpen) {
      onOpen(id)
      return
    }
    openInteraction(id)
  }, [onOpen])

  if (!data || data.length === 0) {
    return (
      <div
        className="bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-4 border border-gray-200 dark:border-gray-800"
        data-testid="ccl-root-empty"
      >
        <h3 className="text-sm font-medium text-gray-700 dark:text-gray-300 mb-3">Most Expensive Commands</h3>
        <div className="text-xs text-gray-500 py-8 text-center">No command cost data yet</div>
      </div>
    )
  }

  const colSpan = 8 // chevron + prompt + when + cost + %total + tokens + tools + steps

  return (
    <div
      className="bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-4 border border-gray-200 dark:border-gray-800"
      data-testid="ccl-root"
    >
      <div className="flex items-baseline justify-between mb-3 gap-3 flex-wrap">
        <h3 className="text-sm font-medium text-gray-700 dark:text-gray-300">Most Expensive Commands</h3>
        <div className="flex items-center gap-3">
          <div className="text-xs text-gray-500" data-testid="ccl-caption">
            {sorted.length === 0
              ? 'no rows'
              : `${(safePage - 1) * pageSize + 1}–${Math.min(safePage * pageSize, sorted.length)} of ${sorted.length}, sorted by ${SORT_LABELS[sortKey]} (${sortDir})`}
          </div>
          <select
            value={pageSize}
            onChange={(e) => { setPageSize(Number(e.target.value)); setPage(1) }}
            aria-label="Rows per page"
            className="bg-white dark:bg-gray-800 border border-gray-300 dark:border-gray-700 rounded px-2 py-1 text-xs text-gray-700 dark:text-gray-300"
          >
            {[10, 25, 50, 100].map((n) => (
              <option key={n} value={n}>{n}/page</option>
            ))}
          </select>
        </div>
      </div>

      <div className="bg-gray-100/50 dark:bg-gray-800/30 rounded border border-gray-200 dark:border-gray-800 overflow-hidden">
        <div className="overflow-x-auto">
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-gray-200 dark:border-gray-800 text-gray-600 dark:text-gray-400 text-xs uppercase tracking-wider">
                <th scope="col" className="w-8" aria-label="expand" />
                <th scope="col" className="px-3 py-2 text-left">Prompt</th>
                <SortHeader
                  label="When" sortKey="when" activeKey={sortKey} dir={sortDir}
                  onSort={setSort} align="left" className="w-32"
                />
                <SortHeader
                  label="Cost" sortKey="cost" activeKey={sortKey} dir={sortDir}
                  onSort={setSort} className="w-20"
                />
                <th scope="col" className="px-3 py-2 text-right w-16">%Total</th>
                <SortHeader
                  label="Tokens" sortKey="tokens" activeKey={sortKey} dir={sortDir}
                  onSort={setSort} className="w-20"
                />
                <SortHeader
                  label="Tools" sortKey="tools" activeKey={sortKey} dir={sortDir}
                  onSort={setSort} className="w-16"
                />
                <SortHeader
                  label="Steps" sortKey="steps" activeKey={sortKey} dir={sortDir}
                  onSort={setSort} className="w-16"
                />
              </tr>
            </thead>
            <tbody>
              {paged.map((r) => {
                const tokens = totalTokens(r.tokens)
                const pctOfTotal = totalCost > 0 ? (r.cost / totalCost) * 100 : 0
                const isOpen = expanded.has(r.interaction_id)
                return (
                  <Fragment key={r.interaction_id}>
                    <tr
                      className="border-b border-gray-200/50 dark:border-gray-800/50 hover:bg-gray-100/70 dark:hover:bg-gray-800/50 cursor-pointer"
                      onClick={() => handleRowClick(r.interaction_id)}
                      data-testid="ccl-row"
                    >
                      <td className="px-2 py-2 text-gray-500 w-8">
                        <button
                          type="button"
                          onClick={(e) => {
                            e.stopPropagation()
                            toggleExpand(r.interaction_id)
                          }}
                          onKeyDown={(e) => {
                            if (e.key === 'Enter' || e.key === ' ') {
                              e.preventDefault()
                              e.stopPropagation()
                              toggleExpand(r.interaction_id)
                            }
                          }}
                          className="p-0.5 rounded hover:text-gray-800 dark:hover:text-gray-200 hover:bg-gray-200/70 dark:hover:bg-gray-700/50 focus:outline-none focus:ring-1 focus:ring-indigo-500"
                          aria-expanded={isOpen}
                          aria-label={isOpen ? 'Collapse row' : 'Expand row'}
                          data-testid="ccl-expand-toggle"
                        >
                          {isOpen
                            ? <IconChevronDown size={14} />
                            : <IconChevronRight size={14} />
                          }
                        </button>
                      </td>
                      <td className="px-3 py-2 text-gray-800 dark:text-gray-200 max-w-md">
                        <div className="flex items-start gap-1.5">
                          {r.had_error && (
                            <IconAlertTriangle
                              size={12}
                              className="text-red-400 mt-0.5 flex-shrink-0"
                            />
                          )}
                          <span className="truncate block" title={r.prompt_preview}>
                            {r.prompt_preview || (
                              <span className="text-gray-500 italic">(empty prompt)</span>
                            )}
                          </span>
                        </div>
                      </td>
                      <td className="px-3 py-2 text-gray-500 text-xs whitespace-nowrap">
                        {formatTime(r.timestamp)}
                      </td>
                      <td className="px-3 py-2 text-right text-gray-900 dark:text-gray-100 font-medium tabular-nums">
                        {formatCost(r.cost)}
                      </td>
                      <td className="px-3 py-2 text-right text-gray-500 tabular-nums text-xs">
                        {pctOfTotal >= 0.1 ? `${pctOfTotal.toFixed(1)}%` : '—'}
                      </td>
                      <td className="px-3 py-2 text-right text-gray-700 dark:text-gray-300 tabular-nums">
                        {formatTokenCount(tokens)}
                      </td>
                      <td className="px-3 py-2 text-right text-gray-700 dark:text-gray-300 tabular-nums">
                        {r.tools_used}
                      </td>
                      <td className="px-3 py-2 text-right text-gray-700 dark:text-gray-300 tabular-nums">
                        {r.steps}
                      </td>
                    </tr>
                    {isOpen && <ExpandedDetail row={r} colSpan={colSpan} />}
                  </Fragment>
                )
              })}
            </tbody>
            <tfoot>
              <tr
                className="border-t border-gray-300 dark:border-gray-700 text-gray-700 dark:text-gray-300 text-xs bg-gray-100/60 dark:bg-gray-800/40"
                data-testid="ccl-footer"
              >
                <td colSpan={3} className="px-3 py-2 text-left uppercase tracking-wider text-gray-500">
                  Cost aggregates
                </td>
                <td className="px-3 py-2 text-right font-medium tabular-nums" title="Sum">
                  Σ {formatCost(costStats.sum)}
                </td>
                <td className="px-3 py-2 text-right text-gray-500 tabular-nums">100%</td>
                <td
                  colSpan={3}
                  className="px-3 py-2 text-right text-gray-600 dark:text-gray-400 tabular-nums text-[11px]"
                >
                  median {formatCost(costStats.median)} · p95 {formatCost(costStats.p95)}
                </td>
              </tr>
            </tfoot>
          </table>
        </div>
      </div>

      {totalPages > 1 && (
        <div className="flex items-center justify-between mt-3 text-xs text-gray-600 dark:text-gray-400" data-testid="ccl-pagination">
          <span>Page {safePage} of {totalPages}</span>
          <div className="flex items-center gap-2">
            <button
              type="button"
              onClick={() => setPage((p) => Math.max(1, p - 1))}
              disabled={safePage <= 1}
              className="px-2 py-1 bg-white dark:bg-gray-800 rounded border border-gray-300 dark:border-gray-700 disabled:opacity-50 disabled:cursor-not-allowed"
            >Prev</button>
            <button
              type="button"
              onClick={() => setPage((p) => Math.min(totalPages, p + 1))}
              disabled={safePage >= totalPages}
              className="px-2 py-1 bg-white dark:bg-gray-800 rounded border border-gray-300 dark:border-gray-700 disabled:opacity-50 disabled:cursor-not-allowed"
            >Next</button>
          </div>
        </div>
      )}
    </div>
  )
}
