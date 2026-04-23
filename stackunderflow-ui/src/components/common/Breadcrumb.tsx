/**
 * Breadcrumb + BackButton — in-UI navigation affordance for deep-linked views.
 *
 * Spec: docs/specs/analytics-fixes.md §D3
 *
 * `<Breadcrumb>` renders a `Home › Tab › Detail` style trail. Earlier segments
 * with an `onClick` become buttons; the final segment is always rendered as
 * plain text (current location, not actionable).
 *
 * `<BackButton>` is a minimal wrapper around `window.history.back()` with an
 * accessible left-arrow icon.
 *
 * The breadcrumb itself never manipulates history — clicking a segment invokes
 * the provided `onClick`, leaving URL state management to the caller (e.g.
 * `clearParam` or `setTab` from `services/navigation`).
 */
import { Fragment } from 'react'
import { IconArrowLeft, IconChevronRight } from '@tabler/icons-react'

export interface BreadcrumbSegment {
  label: string
  onClick?: () => void
}

interface BreadcrumbProps {
  trail: BreadcrumbSegment[]
}

export function Breadcrumb({ trail }: BreadcrumbProps) {
  if (trail.length === 0) return null

  return (
    <nav aria-label="Breadcrumb" className="flex items-center text-xs text-gray-400">
      <ol className="flex items-center flex-wrap gap-1">
        {trail.map((segment, idx) => {
          const isLast = idx === trail.length - 1
          const clickable = !isLast && typeof segment.onClick === 'function'
          return (
            <Fragment key={`${idx}-${segment.label}`}>
              <li className="flex items-center">
                {clickable ? (
                  <button
                    type="button"
                    onClick={segment.onClick}
                    className="text-gray-400 hover:text-indigo-400 focus:outline-none focus-visible:ring-1 focus-visible:ring-indigo-400 rounded px-0.5"
                  >
                    {segment.label}
                  </button>
                ) : (
                  <span
                    className={isLast ? 'text-gray-200 font-medium' : 'text-gray-400'}
                    aria-current={isLast ? 'page' : undefined}
                  >
                    {segment.label}
                  </span>
                )}
              </li>
              {!isLast && (
                <li aria-hidden="true" className="flex items-center text-gray-600">
                  <IconChevronRight size={12} />
                </li>
              )}
            </Fragment>
          )
        })}
      </ol>
    </nav>
  )
}

export function BackButton() {
  return (
    <button
      type="button"
      aria-label="Go back"
      onClick={() => {
        if (typeof window !== 'undefined') {
          window.history.back()
        }
      }}
      className="inline-flex items-center gap-1 px-2 py-1 text-xs text-gray-400 hover:text-gray-200 bg-gray-800 hover:bg-gray-700 border border-gray-700 hover:border-gray-600 rounded focus:outline-none focus-visible:ring-1 focus-visible:ring-indigo-400"
    >
      <IconArrowLeft size={12} />
      Back
    </button>
  )
}

export default Breadcrumb
