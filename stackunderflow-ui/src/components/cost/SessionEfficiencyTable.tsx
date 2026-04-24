import { useCallback, useMemo, useState } from 'react'
import { IconChevronUp, IconChevronDown } from '@tabler/icons-react'
import Badge from '../common/Badge'
import type { SessionEfficiency } from '../../types/api'
import { openSession } from '../../services/navigation'

type SortDir = 'asc' | 'desc'

interface UseSortableTableResult<T> {
  sorted: T[]
  sortKey: keyof T
  sortDir: SortDir
  setSort: (key: keyof T) => void
}

function useSortableTable<T>(
  rows: T[],
  initial: { key: keyof T; dir: SortDir },
): UseSortableTableResult<T> {
  const [sortKey, setSortKey] = useState<keyof T>(initial.key)
  const [sortDir, setSortDir] = useState<SortDir>(initial.dir)

  const setSort = useCallback(
    (key: keyof T) => {
      if (key === sortKey) {
        setSortDir((d) => (d === 'asc' ? 'desc' : 'asc'))
        return
      }
      setSortKey(key)
      // numeric/boolean → desc; string → asc (mirror primitive heuristic)
      let numeric = false
      for (const row of rows) {
        const v = row[key] as unknown
        if (v === null || v === undefined) continue
        numeric = typeof v === 'number' || typeof v === 'boolean'
        break
      }
      setSortDir(numeric ? 'desc' : 'asc')
    },
    [sortKey, rows],
  )

  const sorted = useMemo(() => {
    const copy = rows.slice()
    copy.sort((a, b) => {
      const av = a[sortKey] as unknown
      const bv = b[sortKey] as unknown
      const aNull = av === null || av === undefined || (typeof av === 'number' && Number.isNaN(av))
      const bNull = bv === null || bv === undefined || (typeof bv === 'number' && Number.isNaN(bv))
      if (aNull && bNull) return 0
      if (aNull) return 1
      if (bNull) return -1
      let cmp = 0
      if (typeof av === 'number' && typeof bv === 'number') cmp = av - bv
      else cmp = String(av).localeCompare(String(bv), undefined, { numeric: true, sensitivity: 'base' })
      return sortDir === 'asc' ? cmp : -cmp
    })
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
  className?: string
  align?: 'left' | 'right'
}

function SortHeader({
  label,
  sortKey,
  activeKey,
  dir,
  onClick,
  className,
  align = 'left',
}: SortHeaderProps) {
  const active = sortKey === activeKey
  const alignClass = align === 'right' ? 'text-right' : 'text-left'
  const Chevron = active ? (dir === 'asc' ? IconChevronUp : IconChevronDown) : null
  return (
    <th
      onClick={onClick}
      onKeyDown={(e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault()
          onClick()
        }
      }}
      className={['cursor-pointer select-none', alignClass, className ?? ''].filter(Boolean).join(' ')}
      data-sort-key={sortKey}
      aria-sort={active ? (dir === 'asc' ? 'ascending' : 'descending') : 'none'}
      role="columnheader"
      scope="col"
      tabIndex={0}
    >
      <span
        className={['inline-flex items-center gap-1', align === 'right' ? 'w-full justify-end' : '']
          .filter(Boolean)
          .join(' ')}
      >
        <span>{label}</span>
        {Chevron ? <Chevron size={12} stroke={2.5} aria-hidden="true" /> : null}
      </span>
    </th>
  )
}

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

// Stable display order for chips and footer summary.
const CLASSIFICATION_ORDER = ['edit-heavy', 'research-heavy', 'idle-heavy', 'balanced'] as const

type ClassificationFilter = 'all' | (typeof CLASSIFICATION_ORDER)[number]

const FILTER_CHIPS: ReadonlyArray<{ key: ClassificationFilter; label: string }> = [
  { key: 'all', label: 'All' },
  { key: 'edit-heavy', label: 'edit-heavy' },
  { key: 'research-heavy', label: 'research-heavy' },
  { key: 'idle-heavy', label: 'idle-heavy' },
  { key: 'balanced', label: 'balanced' },
]

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

function classificationCounts(rows: SessionEfficiency[]): Record<string, number> {
  const counts: Record<string, number> = {}
  for (const row of rows) {
    counts[row.classification] = (counts[row.classification] ?? 0) + 1
  }
  return counts
}

export default function SessionEfficiencyTable({ data }: SessionEfficiencyTableProps) {
  const rows = useMemo(() => data ?? [], [data])
  const [filter, setFilter] = useState<ClassificationFilter>('all')

  const filtered = useMemo(
    () => (filter === 'all' ? rows : rows.filter((r) => r.classification === filter)),
    [rows, filter],
  )

  const { sorted, sortKey, sortDir, setSort } = useSortableTable<SessionEfficiency>(filtered, {
    key: 'idle_gap_total_s',
    dir: 'desc',
  })

  const totalCounts = useMemo(() => classificationCounts(rows), [rows])
  const orderedClassifications = useMemo(() => {
    const known = new Set<string>(CLASSIFICATION_ORDER)
    const extras = Object.keys(totalCounts)
      .filter((k) => !known.has(k))
      .sort()
    return [...CLASSIFICATION_ORDER.filter((k) => totalCounts[k]), ...extras]
  }, [totalCounts])

  if (!data || data.length === 0) {
    return (
      <div
        className="bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-4 border border-gray-200 dark:border-gray-800"
        data-testid="session-efficiency-table"
      >
        <h3 className="text-sm font-medium text-gray-700 dark:text-gray-300 mb-3">Session Efficiency</h3>
        <div className="text-xs text-gray-500 py-8 text-center">No session efficiency data yet</div>
      </div>
    )
  }

  const sortKeyStr = String(sortKey)
  const summaryParts = orderedClassifications.map((c) => `${totalCounts[c] ?? 0} ${c}`)
  const summary = summaryParts.length ? summaryParts.join(' · ') : ''

  return (
    <div
      className="bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-4 border border-gray-200 dark:border-gray-800"
      data-testid="session-efficiency-table"
    >
      <h3 className="text-sm font-medium text-gray-700 dark:text-gray-300 mb-3">
        Session Efficiency
        <span className="ml-2 text-xs text-gray-500 font-normal">
          {filtered.length === rows.length
            ? `${rows.length} sessions`
            : `${filtered.length} of ${rows.length} sessions`}
        </span>
      </h3>

      <div
        className="flex flex-wrap gap-1.5 mb-3"
        role="toolbar"
        aria-label="Filter by classification"
        data-testid="session-efficiency-filter-chips"
      >
        {FILTER_CHIPS.map((chip) => {
          const active = filter === chip.key
          const count =
            chip.key === 'all' ? rows.length : totalCounts[chip.key] ?? 0
          const disabled = chip.key !== 'all' && count === 0
          return (
            <button
              key={chip.key}
              type="button"
              disabled={disabled}
              onClick={() => setFilter(chip.key)}
              data-testid={`session-efficiency-chip-${chip.key}`}
              aria-pressed={active}
              className={[
                'inline-flex items-center gap-1.5 px-2.5 py-1 rounded-full text-xs border transition-colors',
                active
                  ? 'bg-blue-900/60 text-blue-200 border-blue-700'
                  : 'bg-gray-100/80 dark:bg-gray-800/60 text-gray-600 dark:text-gray-400 border-gray-300 dark:border-gray-700 hover:bg-white dark:hover:bg-gray-800 hover:text-gray-800 dark:hover:text-gray-200',
                disabled ? 'opacity-40 cursor-not-allowed hover:bg-gray-100/80 dark:hover:bg-gray-800/60 hover:text-gray-600 dark:hover:text-gray-400' : '',
              ]
                .filter(Boolean)
                .join(' ')}
            >
              <span>{chip.label}</span>
              <span className="text-[10px] tabular-nums opacity-80">{count}</span>
            </button>
          )
        })}
      </div>

      <div className="bg-gray-100/50 dark:bg-gray-800/30 rounded border border-gray-200 dark:border-gray-800 overflow-hidden">
        <div className="overflow-x-auto">
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-gray-200 dark:border-gray-800 text-gray-600 dark:text-gray-400 text-xs uppercase tracking-wider">
                <SortHeader
                  label="Session"
                  sortKey="session_id"
                  activeKey={sortKeyStr}
                  dir={sortDir}
                  onClick={() => setSort('session_id')}
                  className="px-3 py-2 w-28"
                />
                <SortHeader
                  label="Class"
                  sortKey="classification"
                  activeKey={sortKeyStr}
                  dir={sortDir}
                  onClick={() => setSort('classification')}
                  className="px-3 py-2 w-32"
                />
                <SortHeader
                  label="Edit"
                  sortKey="edit_ratio"
                  activeKey={sortKeyStr}
                  dir={sortDir}
                  onClick={() => setSort('edit_ratio')}
                  align="right"
                  className="px-3 py-2 w-16"
                />
                <SortHeader
                  label="Read"
                  sortKey="read_ratio"
                  activeKey={sortKeyStr}
                  dir={sortDir}
                  onClick={() => setSort('read_ratio')}
                  align="right"
                  className="px-3 py-2 w-16"
                />
                <SortHeader
                  label="Search"
                  sortKey="search_ratio"
                  activeKey={sortKeyStr}
                  dir={sortDir}
                  onClick={() => setSort('search_ratio')}
                  align="right"
                  className="px-3 py-2 w-16"
                />
                <SortHeader
                  label="Bash"
                  sortKey="bash_ratio"
                  activeKey={sortKeyStr}
                  dir={sortDir}
                  onClick={() => setSort('bash_ratio')}
                  align="right"
                  className="px-3 py-2 w-16"
                />
                <SortHeader
                  label="Idle Total"
                  sortKey="idle_gap_total_s"
                  activeKey={sortKeyStr}
                  dir={sortDir}
                  onClick={() => setSort('idle_gap_total_s')}
                  align="right"
                  className="px-3 py-2 w-20"
                />
                <SortHeader
                  label="Idle Max"
                  sortKey="idle_gap_max_s"
                  activeKey={sortKeyStr}
                  dir={sortDir}
                  onClick={() => setSort('idle_gap_max_s')}
                  align="right"
                  className="px-3 py-2 w-20"
                />
              </tr>
            </thead>
            <tbody>
              {sorted.length === 0 ? (
                <tr>
                  <td colSpan={8} className="px-3 py-6 text-center text-xs text-gray-500">
                    No sessions match this filter
                  </td>
                </tr>
              ) : (
                sorted.map((s) => {
                  const color = CLASSIFICATION_COLOR[s.classification] ?? 'gray'
                  return (
                    <tr
                      key={s.session_id}
                      className="border-b border-gray-200/50 dark:border-gray-800/50 cursor-pointer hover:bg-gray-100/60 dark:hover:bg-gray-800/40 focus:bg-gray-100/80 dark:focus:bg-gray-800/60 focus:outline-none"
                      onClick={() => openSession(s.session_id)}
                      onKeyDown={(e) => {
                        if (e.key === 'Enter' || e.key === ' ') {
                          e.preventDefault()
                          openSession(s.session_id)
                        }
                      }}
                      tabIndex={0}
                      role="link"
                      aria-label={`Open session ${s.session_id}`}
                      data-testid={`session-efficiency-row-${s.session_id}`}
                    >
                      <td className="px-3 py-2 text-gray-700 dark:text-gray-300 font-mono text-xs">
                        {shortSession(s.session_id)}
                      </td>
                      <td className="px-3 py-2">
                        <Badge color={color}>{s.classification}</Badge>
                      </td>
                      <td className="px-3 py-2 text-right text-gray-700 dark:text-gray-300 tabular-nums">
                        {formatPct(s.edit_ratio)}
                      </td>
                      <td className="px-3 py-2 text-right text-gray-700 dark:text-gray-300 tabular-nums">
                        {formatPct(s.read_ratio)}
                      </td>
                      <td className="px-3 py-2 text-right text-gray-700 dark:text-gray-300 tabular-nums">
                        {formatPct(s.search_ratio)}
                      </td>
                      <td className="px-3 py-2 text-right text-gray-700 dark:text-gray-300 tabular-nums">
                        {formatPct(s.bash_ratio)}
                      </td>
                      <td className="px-3 py-2 text-right text-gray-600 dark:text-gray-400 tabular-nums">
                        {formatDuration(s.idle_gap_total_s)}
                      </td>
                      <td className="px-3 py-2 text-right text-gray-600 dark:text-gray-400 tabular-nums">
                        {formatDuration(s.idle_gap_max_s)}
                      </td>
                    </tr>
                  )
                })
              )}
            </tbody>
          </table>
        </div>
      </div>

      <div
        className="mt-2 text-xs text-gray-500"
        data-testid="session-efficiency-footer"
      >
        {rows.length} sessions{summary ? `: ${summary}` : ''}
      </div>
    </div>
  )
}
