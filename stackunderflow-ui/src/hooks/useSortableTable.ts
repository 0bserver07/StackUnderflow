import { useCallback, useMemo, useState } from 'react'
import { createElement, type ReactElement } from 'react'
import { IconChevronUp, IconChevronDown } from '@tabler/icons-react'

/**
 * Direction a column is sorted in.
 */
export type SortDir = 'asc' | 'desc'

/**
 * Value types the comparator handles natively. Anything else is coerced via String().
 */
type SortableValue = string | number | boolean | null | undefined

// Matches full ISO 8601 date or date-time strings, e.g. "2025-04-23", "2025-04-23T14:05:00Z".
const ISO_DATE_RE = /^\d{4}-\d{2}-\d{2}(?:T[\d:.+\-Z]*)?$/

function isNullish(v: unknown): boolean {
  return v === null || v === undefined || (typeof v === 'number' && Number.isNaN(v))
}

function isIsoDateString(v: string): boolean {
  return ISO_DATE_RE.test(v)
}

/**
 * Compare two sortable values for a given direction. Nullish values always sort
 * to the end regardless of direction.
 */
function compareValues(a: SortableValue, b: SortableValue, dir: SortDir): number {
  const aNull = isNullish(a)
  const bNull = isNullish(b)
  if (aNull && bNull) return 0
  if (aNull) return 1
  if (bNull) return -1

  let cmp = 0
  if (typeof a === 'number' && typeof b === 'number') {
    cmp = a - b
  } else if (typeof a === 'boolean' && typeof b === 'boolean') {
    cmp = a === b ? 0 : a ? 1 : -1
  } else {
    const sa = String(a)
    const sb = String(b)
    if (isIsoDateString(sa) && isIsoDateString(sb)) {
      const da = Date.parse(sa)
      const db = Date.parse(sb)
      if (!Number.isNaN(da) && !Number.isNaN(db)) {
        cmp = da - db
      } else {
        cmp = sa.localeCompare(sb)
      }
    } else {
      cmp = sa.localeCompare(sb, undefined, { numeric: true, sensitivity: 'base' })
    }
  }
  return dir === 'asc' ? cmp : -cmp
}

/**
 * Inspects the first non-nullish value for a column and decides whether it should
 * default to numeric sort ordering (desc) or string sort ordering (asc).
 */
function isNumericColumn<T>(rows: readonly T[], key: keyof T): boolean {
  for (const row of rows) {
    const v = row[key] as unknown
    if (v === null || v === undefined) continue
    if (typeof v === 'number') return !Number.isNaN(v)
    if (typeof v === 'boolean') return true // booleans behave like numbers → prefer desc
    return false
  }
  return false
}

export interface UseSortableTableResult<T> {
  /** Rows sorted by the current key + direction. Stable reference for the same inputs. */
  sorted: T[]
  /** The active sort key. */
  sortKey: keyof T
  /** The active sort direction. */
  sortDir: SortDir
  /**
   * Request a sort by `key`:
   * - If `key === sortKey`, flips the direction.
   * - Otherwise, sets `key` with `desc` for numeric/boolean columns, `asc` for strings.
   */
  setSort: (key: keyof T) => void
}

/**
 * Type-safe sortable-table hook.
 *
 * - Handles `number`, `string`, `boolean`, and ISO date-string columns out of the box.
 * - Nullish (`null`, `undefined`, `NaN`) values always sort to the end regardless of direction.
 * - Clicking the same key flips direction; clicking a new key picks a sensible default
 *   (`desc` for numeric/boolean columns, `asc` for strings).
 *
 * @example
 * ```tsx
 * type Row = { name: string; cost: number; when: string | null }
 * const rows: Row[] = [...]
 *
 * function CostTable({ rows }: { rows: Row[] }) {
 *   const { sorted, sortKey, sortDir, setSort } = useSortableTable(rows, {
 *     key: 'cost',
 *     dir: 'desc',
 *   })
 *   return (
 *     <table>
 *       <thead>
 *         <tr>
 *           <SortHeader label="Name" sortKey="name" activeKey={String(sortKey)}
 *             dir={sortDir} onClick={() => setSort('name')} />
 *           <SortHeader label="Cost" sortKey="cost" activeKey={String(sortKey)}
 *             dir={sortDir} onClick={() => setSort('cost')} align="right" />
 *         </tr>
 *       </thead>
 *       <tbody>
 *         {sorted.map(r => <tr key={r.name}><td>{r.name}</td><td>{r.cost}</td></tr>)}
 *       </tbody>
 *     </table>
 *   )
 * }
 * ```
 */
export function useSortableTable<T>(
  rows: T[],
  initial: { key: keyof T; dir: SortDir }
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
      setSortDir(isNumericColumn(rows, key) ? 'desc' : 'asc')
    },
    [sortKey, rows]
  )

  const sorted = useMemo(() => {
    const copy = rows.slice()
    copy.sort((a, b) =>
      compareValues(
        a[sortKey] as unknown as SortableValue,
        b[sortKey] as unknown as SortableValue,
        sortDir
      )
    )
    return copy
  }, [rows, sortKey, sortDir])

  return { sorted, sortKey, sortDir, setSort }
}

export interface SortHeaderProps {
  label: string
  sortKey: string
  activeKey: string
  dir: SortDir
  onClick: () => void
  className?: string
  align?: 'left' | 'right'
}

/**
 * A clickable `<th>` that shows an up/down chevron when the column is active.
 * Kept framework-agnostic (plain Tailwind classes) so it drops into any existing table.
 *
 * Note: defined with `createElement` (not JSX) so this module can live in `.ts` and keep
 * `useSortableTable` importable from non-TSX call sites without forcing a `.tsx` rename.
 */
export function SortHeader({
  label,
  sortKey,
  activeKey,
  dir,
  onClick,
  className,
  align = 'left',
}: SortHeaderProps): ReactElement {
  const active = sortKey === activeKey
  const alignClass = align === 'right' ? 'text-right' : 'text-left'
  const thClass = ['cursor-pointer select-none', alignClass, className ?? '']
    .filter(Boolean)
    .join(' ')
  const innerClass = [
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
      className: thClass,
      'data-sort-key': sortKey,
      'aria-sort': active ? (dir === 'asc' ? 'ascending' : 'descending') : 'none',
      role: 'columnheader',
      scope: 'col',
      tabIndex: 0,
      onKeyDown: (e: React.KeyboardEvent<HTMLTableCellElement>) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault()
          onClick()
        }
      },
    },
    createElement(
      'span',
      { className: innerClass },
      createElement('span', null, label),
      Chevron ? createElement(Chevron, { size: 12, stroke: 2.5, 'aria-hidden': true }) : null
    )
  )
}
