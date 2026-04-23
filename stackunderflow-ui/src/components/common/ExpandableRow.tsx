import type { KeyboardEvent, ReactNode } from 'react'
import { IconChevronRight } from '@tabler/icons-react'

export interface ExpandableRowProps {
  /** Whether the detail row is currently visible. */
  expanded: boolean
  /** Called when the caller should toggle `expanded`. */
  onToggle: () => void
  /**
   * Total column count for the detail row's `colSpan`. Must be inclusive of
   * the chevron cell this component renders as the first `<td>`, i.e. equal
   * to `(caller <td> count) + 1`.
   */
  columns: number
  /** Main-row cells (`<td>`s), rendered AFTER the chevron cell. */
  children: ReactNode
  /** Content rendered inside a full-width `<td colSpan={columns}>` when expanded. */
  detail: ReactNode
  /** Extra class names for the main `<tr>`. */
  rowClassName?: string
  /** Extra class names for the detail `<td>`. Defaults to a muted/indented look. */
  detailClassName?: string
  /** Optional test id applied to the main `<tr>`. */
  'data-testid'?: string
}

const DEFAULT_ROW_CLASS =
  'border-b border-gray-800/50 hover:bg-gray-800/50 cursor-pointer focus:outline-none focus:bg-gray-800/50'

const DEFAULT_DETAIL_CLASS =
  'bg-gray-900/60 text-gray-300 px-6 py-3 border-b border-gray-800'

/**
 * Keyboard-accessible expandable table row. Renders two `<tr>`s:
 *   1. A main row with a left-most chevron cell + caller-provided `<td>`s.
 *   2. A detail row rendered only when `expanded`, spanning all columns.
 *
 * Caller usage:
 *
 *   <ExpandableRow
 *     expanded={open}
 *     onToggle={() => setOpen(v => !v)}
 *     columns={6}          // 5 data columns + 1 for the chevron
 *     detail={<DetailPanel row={r} />}
 *   >
 *     <td>{r.name}</td>
 *     <td className="text-right">{r.cost}</td>
 *     ...
 *   </ExpandableRow>
 */
export default function ExpandableRow({
  expanded,
  onToggle,
  columns,
  children,
  detail,
  rowClassName,
  detailClassName,
  'data-testid': testId,
}: ExpandableRowProps) {
  const handleKeyDown = (e: KeyboardEvent<HTMLTableRowElement>) => {
    if (e.key === 'Enter' || e.key === ' ' || e.key === 'Spacebar') {
      e.preventDefault()
      onToggle()
    }
  }

  return (
    <>
      <tr
        className={rowClassName ?? DEFAULT_ROW_CLASS}
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
          <td colSpan={columns} className={detailClassName ?? DEFAULT_DETAIL_CLASS}>
            {detail}
          </td>
        </tr>
      )}
    </>
  )
}
