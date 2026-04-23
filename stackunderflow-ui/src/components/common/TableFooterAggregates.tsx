/**
 * TableFooterAggregates — <tfoot> renderer that summarises numeric columns.
 *
 * Drop inside any <table> to show one row per requested aggregate
 * (sum / median / p95 / mean). Each column in the `columns` array maps
 * 1:1 with the underlying table columns; pass `null` for columns that
 * should render an empty cell (e.g. a prompt / text column).
 *
 * The leftmost cell of every aggregate row carries the aggregate label
 * ("Sum", "Median", "p95", "Mean"), replacing whatever aggregate that
 * column would have rendered. Remaining cells show each column's
 * aggregate value, formatted via the column's `format` prop (default
 * `toLocaleString`).
 *
 * See docs/specs/analytics-polish.md §A7.
 */

type AggregateKind = 'sum' | 'median' | 'p95' | 'mean'

export interface AggregateColumn {
  align?: 'left' | 'right' | 'center'
  label?: string
  values: number[]
  format?: (n: number) => string
  show?: AggregateKind[]
}

interface TableFooterAggregatesProps {
  columns: Array<AggregateColumn | null>
  className?: string
}

const AGGREGATE_LABELS: Record<AggregateKind, string> = {
  sum: 'Sum',
  median: 'Median',
  p95: 'p95',
  mean: 'Mean',
}

const DEFAULT_SHOW: AggregateKind[] = ['sum', 'median']

function cleanNumbers(values: number[]): number[] {
  const out: number[] = []
  for (const v of values) {
    if (v === null || v === undefined) continue
    if (typeof v !== 'number') continue
    if (Number.isNaN(v)) continue
    out.push(v)
  }
  return out
}

function computeSum(values: number[]): number {
  let s = 0
  for (const v of values) s += v
  return s
}

function computeMean(values: number[]): number {
  if (values.length === 0) return 0
  return computeSum(values) / values.length
}

function computeMedian(values: number[]): number {
  if (values.length === 0) return 0
  const sorted = [...values].sort((a, b) => a - b)
  const mid = Math.floor(sorted.length / 2)
  if (sorted.length % 2 === 0) {
    const lo = sorted[mid - 1] ?? 0
    const hi = sorted[mid] ?? 0
    return (lo + hi) / 2
  }
  return sorted[mid] ?? 0
}

function computeP95(values: number[]): number {
  if (values.length === 0) return 0
  const sorted = [...values].sort((a, b) => a - b)
  const idx = Math.floor(0.95 * sorted.length)
  const clamped = Math.min(idx, sorted.length - 1)
  return sorted[clamped] ?? 0
}

function computeAggregate(kind: AggregateKind, values: number[]): number {
  switch (kind) {
    case 'sum':
      return computeSum(values)
    case 'mean':
      return computeMean(values)
    case 'median':
      return computeMedian(values)
    case 'p95':
      return computeP95(values)
  }
}

function alignClass(align: AggregateColumn['align']): string {
  if (align === 'right') return 'text-right'
  if (align === 'center') return 'text-center'
  return 'text-left'
}

function formatValue(col: AggregateColumn, value: number): string {
  if (col.format) return col.format(value)
  return value.toLocaleString()
}

export default function TableFooterAggregates({
  columns,
  className,
}: TableFooterAggregatesProps) {
  // Collect the union of aggregates to render across all non-null columns,
  // preserving canonical display order (sum → median → p95 → mean).
  const canonicalOrder: AggregateKind[] = ['sum', 'median', 'p95', 'mean']
  const requested = new Set<AggregateKind>()
  for (const col of columns) {
    if (!col) continue
    const show = col.show ?? DEFAULT_SHOW
    for (const k of show) requested.add(k)
  }
  const rows = canonicalOrder.filter(k => requested.has(k))

  if (rows.length === 0 || columns.length === 0) {
    return null
  }

  const rootClass = [
    'border-t border-gray-800 bg-gray-800/40 text-xs',
    className ?? '',
  ]
    .filter(Boolean)
    .join(' ')

  const firstCol = columns[0] ?? null
  const firstAlignCls = alignClass(firstCol?.align)

  return (
    <tfoot className={rootClass} data-testid="table-footer-aggregates">
      {rows.map(kind => (
        <tr key={kind} data-testid={`aggregate-row-${kind}`}>
          {/* Leftmost cell: aggregate label. */}
          <td
            className={`px-4 py-2 font-medium text-gray-400 uppercase tracking-wider ${firstAlignCls}`}
          >
            {firstCol?.label ?? AGGREGATE_LABELS[kind]}
          </td>

          {/* Remaining cells: each column's aggregate value, or empty for null. */}
          {columns.slice(1).map((col, sliceIdx) => {
            const idx = sliceIdx + 1
            const keyBase = `${kind}-${idx}`

            if (col === null) {
              return (
                <td
                  key={keyBase}
                  className="px-4 py-2"
                  aria-hidden="true"
                />
              )
            }

            const show = col.show ?? DEFAULT_SHOW
            const alignCls = alignClass(col.align)

            if (!show.includes(kind)) {
              return <td key={keyBase} className={`px-4 py-2 ${alignCls}`} />
            }

            const clean = cleanNumbers(col.values)
            const formatted =
              clean.length === 0
                ? '—'
                : formatValue(col, computeAggregate(kind, clean))

            return (
              <td
                key={keyBase}
                className={`px-4 py-2 text-gray-200 tabular-nums ${alignCls}`}
              >
                {formatted}
              </td>
            )
          })}
        </tr>
      ))}
    </tfoot>
  )
}
