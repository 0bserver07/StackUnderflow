import {
  createElement,
  useCallback,
  useMemo,
  useState,
  type KeyboardEvent,
  type ReactElement,
  type ReactNode,
} from 'react'
import {
  IconTool,
  IconRoute,
  IconChevronRight,
  IconChevronUp,
  IconChevronDown,
} from '@tabler/icons-react'
import type { Outliers, OutlierCommand } from '../../types/api'
import { openInteraction, openSession } from '../../services/navigation'

type SortDir = 'asc' | 'desc'

function isNullish(v: unknown): boolean {
  return v === null || v === undefined || (typeof v === 'number' && Number.isNaN(v))
}

function compareValues(a: unknown, b: unknown, dir: SortDir): number {
  const aNull = isNullish(a)
  const bNull = isNullish(b)
  if (aNull && bNull) return 0
  if (aNull) return 1
  if (bNull) return -1
  let cmp = 0
  if (typeof a === 'number' && typeof b === 'number') {
    cmp = a - b
  } else {
    const sa = String(a)
    const sb = String(b)
    // Treat ISO-ish strings as dates for the "when" column.
    if (/^\d{4}-\d{2}-\d{2}/.test(sa) && /^\d{4}-\d{2}-\d{2}/.test(sb)) {
      const da = Date.parse(sa)
      const db = Date.parse(sb)
      if (!Number.isNaN(da) && !Number.isNaN(db)) cmp = da - db
      else cmp = sa.localeCompare(sb)
    } else {
      cmp = sa.localeCompare(sb, undefined, { numeric: true, sensitivity: 'base' })
    }
  }
  return dir === 'asc' ? cmp : -cmp
}

function useSortableTable<T>(rows: T[], initial: { key: keyof T; dir: SortDir }) {
  const [sortKey, setSortKey] = useState<keyof T>(initial.key)
  const [sortDir, setSortDir] = useState<SortDir>(initial.dir)
  const setSort = useCallback(
    (key: keyof T) => {
      if (key === sortKey) {
        setSortDir((d) => (d === 'asc' ? 'desc' : 'asc'))
        return
      }
      setSortKey(key)
      // Sensible default: desc for numbers, asc otherwise.
      const sample = rows.find(
        (r) => !isNullish((r as Record<string, unknown>)[key as string])
      )
      const v = sample ? (sample as Record<string, unknown>)[key as string] : undefined
      setSortDir(typeof v === 'number' ? 'desc' : 'asc')
    },
    [sortKey, rows]
  )
  const sorted = useMemo(() => {
    const copy = rows.slice()
    copy.sort((a, b) =>
      compareValues(
        (a as Record<string, unknown>)[sortKey as string],
        (b as Record<string, unknown>)[sortKey as string],
        sortDir
      )
    )
    return copy
  }, [rows, sortKey, sortDir])
  return { sorted, sortKey, sortDir, setSort }
}

interface SortHeaderProps {
  label: string
  sortKey: string
  activeKey: string
  dir: SortDir
  onClick: () => void
  align?: 'left' | 'right'
  className?: string
}

function SortHeader({
  label,
  sortKey,
  activeKey,
  dir,
  onClick,
  align = 'left',
  className,
}: SortHeaderProps): ReactElement {
  const active = sortKey === activeKey
  const alignCls = align === 'right' ? 'text-right' : 'text-left'
  const thCls = ['cursor-pointer select-none px-3 py-2', alignCls, className ?? '']
    .filter(Boolean)
    .join(' ')
  const innerCls = [
    'inline-flex items-center gap-1',
    align === 'right' ? 'w-full justify-end' : '',
  ]
    .filter(Boolean)
    .join(' ')
  const Chevron = active ? (dir === 'asc' ? IconChevronUp : IconChevronDown) : null
  return createElement(
    'th',
    {
      onClick,
      className: thCls,
      'data-sort-key': sortKey,
      'aria-sort': active ? (dir === 'asc' ? 'ascending' : 'descending') : 'none',
      role: 'columnheader',
      scope: 'col',
      tabIndex: 0,
      onKeyDown: (e: KeyboardEvent<HTMLTableCellElement>) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault()
          onClick()
        }
      },
    },
    createElement(
      'span',
      { className: innerCls },
      createElement('span', null, label),
      Chevron ? createElement(Chevron, { size: 12, stroke: 2.5, 'aria-hidden': true }) : null
    )
  )
}

interface ExpandableRowProps {
  expanded: boolean
  onToggle: () => void
  columns: number
  children: ReactNode
  detail: ReactNode
  rowClassName?: string
  detailClassName?: string
  'data-testid'?: string
}

function ExpandableRow({
  expanded,
  onToggle,
  columns,
  children,
  detail,
  rowClassName,
  detailClassName,
  'data-testid': testId,
}: ExpandableRowProps) {
  const rowCls =
    rowClassName ??
    'border-b border-gray-800/50 hover:bg-gray-800/50 cursor-pointer focus:outline-none focus:bg-gray-800/50'
  const detailCls =
    detailClassName ?? 'bg-gray-900/60 text-gray-300 px-6 py-3 border-b border-gray-800'
  const handleKeyDown = (e: KeyboardEvent<HTMLTableRowElement>) => {
    if (e.key === 'Enter' || e.key === ' ' || e.key === 'Spacebar') {
      e.preventDefault()
      onToggle()
    }
  }
  return (
    <>
      <tr
        className={rowCls}
        role="button"
        tabIndex={0}
        aria-expanded={expanded}
        onClick={onToggle}
        onKeyDown={handleKeyDown}
        data-testid={testId}
      >
        <td className="w-6 px-2 py-2 text-gray-500 align-middle">
          <IconChevronRight
            size={14}
            className={`transition-transform duration-150 ease-out ${expanded ? 'rotate-90' : ''}`}
            aria-hidden="true"
          />
        </td>
        {children}
      </tr>
      {expanded && (
        <tr data-testid={testId ? `${testId}-detail` : undefined}>
          <td colSpan={columns} className={detailCls}>
            {detail}
          </td>
        </tr>
      )}
    </>
  )
}

function median(values: number[]): number {
  const clean = values.filter((v) => typeof v === 'number' && !Number.isNaN(v))
  if (clean.length === 0) return 0
  const sorted = [...clean].sort((a, b) => a - b)
  const mid = Math.floor(sorted.length / 2)
  if (sorted.length % 2 === 0) {
    return ((sorted[mid - 1] ?? 0) + (sorted[mid] ?? 0)) / 2
  }
  return sorted[mid] ?? 0
}

// -------------------------------------------------------------------------------------
// Component
// -------------------------------------------------------------------------------------

interface OutlierCommandsTableProps {
  outliers: Outliers | null | undefined
  /** When provided, row click/expand calls this instead of the default nav. */
  onOpen?: (interactionId: string) => void
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

const DEFAULT_PAGE_SIZE = 50

type SortableKey = keyof Pick<
  OutlierCommand,
  'tool_count' | 'step_count' | 'cost' | 'timestamp'
>

interface OutlierSectionProps {
  title: string
  icon: React.ReactNode
  rows: OutlierCommand[]
  countKey: 'tool_count' | 'step_count'
  countLabel: string
  empty: string
  testIdPrefix: string
  onOpenInteraction: (interactionId: string) => void
  onOpenSession: (sessionId: string) => void
}

function OutlierSection({
  title,
  icon,
  rows,
  countKey,
  countLabel,
  empty,
  testIdPrefix,
  onOpenInteraction,
  onOpenSession,
}: OutlierSectionProps) {
  const { sorted, sortDir, sortKey, setSort } = useSortableTable<OutlierCommand>(rows, {
    key: countKey,
    dir: 'desc',
  })
  const [showAll, setShowAll] = useState(false)
  const [expandedId, setExpandedId] = useState<string | null>(null)

  const total = sorted.length
  const capped = !showAll && total > DEFAULT_PAGE_SIZE
  const visible = capped ? sorted.slice(0, DEFAULT_PAGE_SIZE) : sorted

  const medianCost = useMemo(() => median(rows.map((r) => r.cost)), [rows])

  // 5 visible columns: chevron, prompt, when, count, cost
  const COLS = 5
  const activeKey = String(sortKey)

  return (
    <div data-testid={`${testIdPrefix}-section`}>
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
            <table className="w-full text-sm" data-testid={`${testIdPrefix}-table`}>
              <thead>
                <tr className="border-b border-gray-800 text-gray-400 text-xs uppercase tracking-wider">
                  <th className="w-6 px-2 py-2" aria-hidden="true" />
                  <th className="px-3 py-2 text-left">Prompt</th>
                  <SortHeader
                    label="When"
                    sortKey="timestamp"
                    activeKey={activeKey}
                    dir={sortDir}
                    onClick={() => setSort('timestamp' as SortableKey)}
                    className="w-28"
                  />
                  <SortHeader
                    label={countLabel}
                    sortKey={countKey}
                    activeKey={activeKey}
                    dir={sortDir}
                    onClick={() => setSort(countKey as SortableKey)}
                    align="right"
                    className="w-20"
                  />
                  <SortHeader
                    label="Cost"
                    sortKey="cost"
                    activeKey={activeKey}
                    dir={sortDir}
                    onClick={() => setSort('cost' as SortableKey)}
                    align="right"
                    className="w-20"
                  />
                </tr>
              </thead>
              <tbody>
                {visible.map((r) => {
                  const isOpen = expandedId === r.interaction_id
                  return (
                    <ExpandableRow
                      key={r.interaction_id}
                      expanded={isOpen}
                      onToggle={() => {
                        setExpandedId(isOpen ? null : r.interaction_id)
                        onOpenInteraction(r.interaction_id)
                      }}
                      columns={COLS}
                      data-testid={`${testIdPrefix}-row-${r.interaction_id}`}
                      detail={
                        <div className="space-y-2">
                          <div>
                            <div className="text-xs uppercase tracking-wider text-gray-500 mb-1">
                              Prompt
                            </div>
                            <div className="text-sm text-gray-200 whitespace-pre-wrap break-words">
                              {r.prompt_preview || (
                                <span className="text-gray-500 italic">(empty prompt)</span>
                              )}
                            </div>
                          </div>
                          <div className="flex flex-wrap gap-x-6 gap-y-1 text-xs text-gray-400">
                            <div>
                              <span className="text-gray-500">Tool calls:</span>{' '}
                              <span className="tabular-nums text-gray-200">{r.tool_count}</span>
                            </div>
                            <div>
                              <span className="text-gray-500">Assistant steps:</span>{' '}
                              <span className="tabular-nums text-gray-200">{r.step_count}</span>
                            </div>
                            <div>
                              <span className="text-gray-500">Cost:</span>{' '}
                              <span className="tabular-nums text-gray-200">
                                {formatCost(r.cost)}
                              </span>
                            </div>
                            <div>
                              <span className="text-gray-500">Session:</span>{' '}
                              <button
                                type="button"
                                className="text-blue-400 hover:text-blue-300 hover:underline font-mono text-xs"
                                onClick={(e) => {
                                  e.stopPropagation()
                                  onOpenSession(r.session_id)
                                }}
                              >
                                {r.session_id}
                              </button>
                            </div>
                          </div>
                        </div>
                      }
                    >
                      <td className="px-3 py-2 text-gray-200 max-w-md">
                        <span className="truncate block" title={r.prompt_preview}>
                          {r.prompt_preview || (
                            <span className="text-gray-500 italic">(empty prompt)</span>
                          )}
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
                    </ExpandableRow>
                  )
                })}
              </tbody>
              <tfoot
                className="border-t border-gray-800 bg-gray-800/40 text-xs"
                data-testid={`${testIdPrefix}-footer`}
              >
                <tr>
                  <td className="w-6 px-2 py-2" aria-hidden="true" />
                  <td className="px-3 py-2 font-medium text-gray-400 uppercase tracking-wider">
                    {total} {total === 1 ? 'command' : 'commands'}
                    {capped ? (
                      <span className="text-gray-500 normal-case ml-2">
                        (showing first {DEFAULT_PAGE_SIZE})
                      </span>
                    ) : null}
                  </td>
                  <td className="px-3 py-2" />
                  <td className="px-3 py-2 text-right text-gray-500 uppercase tracking-wider">
                    median
                  </td>
                  <td className="px-3 py-2 text-right text-gray-200 tabular-nums">
                    {formatCost(medianCost)}
                  </td>
                </tr>
              </tfoot>
            </table>
          </div>
          {total > DEFAULT_PAGE_SIZE && (
            <div className="border-t border-gray-800 px-3 py-2 bg-gray-800/20 text-center">
              <button
                type="button"
                className="text-xs text-blue-400 hover:text-blue-300 hover:underline"
                onClick={() => setShowAll((v) => !v)}
                data-testid={`${testIdPrefix}-toggle-all`}
              >
                {showAll ? `Show first ${DEFAULT_PAGE_SIZE}` : `Show all (${total})`}
              </button>
            </div>
          )}
        </div>
      )}
    </div>
  )
}

export default function OutlierCommandsTable({ outliers, onOpen }: OutlierCommandsTableProps) {
  const highTool = outliers?.high_tool_commands ?? []
  const highStep = outliers?.high_step_commands ?? []

  const onOpenInteraction = useCallback(
    (id: string) => {
      if (onOpen) onOpen(id)
      else openInteraction(id)
    },
    [onOpen]
  )
  const onOpenSession = useCallback((id: string) => {
    openSession(id)
  }, [])

  if (!outliers || (highTool.length === 0 && highStep.length === 0)) {
    return (
      <div
        className="bg-gray-800/50 rounded-lg p-4 border border-gray-800"
        data-testid="outlier-commands-table"
      >
        <h3 className="text-sm font-medium text-gray-300 mb-3">Outlier Commands</h3>
        <div className="text-xs text-gray-500 py-8 text-center">
          No outlier commands — nothing exceeded the thresholds.
        </div>
      </div>
    )
  }

  return (
    <div
      className="bg-gray-800/50 rounded-lg p-4 border border-gray-800"
      data-testid="outlier-commands-table"
    >
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
          testIdPrefix="outlier-high-tool"
          onOpenInteraction={onOpenInteraction}
          onOpenSession={onOpenSession}
        />
        <OutlierSection
          title="High step count"
          icon={<IconRoute size={12} />}
          rows={highStep}
          countKey="step_count"
          countLabel="Steps"
          empty="No commands exceeded 15 assistant steps."
          testIdPrefix="outlier-high-step"
          onOpenInteraction={onOpenInteraction}
          onOpenSession={onOpenSession}
        />
      </div>
    </div>
  )
}
