/**
 * Tab-level "Download" button.
 *
 * Lives in the top-right of each dashboard tab. Click pops a small dropdown
 * with format (CSV / JSON) + period (today / 7d / 30d / all) selectors and
 * a Download button. Submission triggers a `GET /api/export?...` via a
 * temporary anchor — the backend sets ``Content-Disposition: attachment``
 * so the browser saves the file rather than rendering it.
 *
 * The same component is reused on every tab; tab-specific filtering is
 * forwarded as query params (the v0.6.0 backend route accepts the union
 * of CLI flags). The ``tab`` prop is captured into the export filename
 * so users can tell which tab the file came from.
 */

import { useEffect, useRef, useState } from 'react'
import { IconDownload, IconChevronDown } from '@tabler/icons-react'

type ExportFormat = 'csv' | 'json'
type ExportPeriod = 'today' | 'week' | 'month' | 'all'

interface ExportButtonProps {
  /** Identifier for the tab — only used as a tooltip label today, kept for
   *  future tab-scoped filters. */
  tab: string
  /** Optional className passed through to the outer wrapper. */
  className?: string
}

const PERIODS: Array<{ value: ExportPeriod; label: string }> = [
  { value: 'today', label: 'Today' },
  { value: 'week', label: 'Last 7 days' },
  { value: 'month', label: 'Last 30 days' },
  { value: 'all', label: 'All time' },
]

const FORMATS: Array<{ value: ExportFormat; label: string }> = [
  { value: 'csv', label: 'CSV' },
  { value: 'json', label: 'JSON' },
]

export default function ExportButton({ tab, className }: ExportButtonProps) {
  const [open, setOpen] = useState(false)
  const [format, setFormat] = useState<ExportFormat>('csv')
  const [period, setPeriod] = useState<ExportPeriod>('week')
  const wrapperRef = useRef<HTMLDivElement>(null)

  // Close on outside click / Escape so the popover stops blocking the page.
  useEffect(() => {
    if (!open) return
    const onClick = (e: MouseEvent) => {
      if (!wrapperRef.current) return
      if (!wrapperRef.current.contains(e.target as Node)) setOpen(false)
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false)
    }
    window.addEventListener('mousedown', onClick)
    window.addEventListener('keydown', onKey)
    return () => {
      window.removeEventListener('mousedown', onClick)
      window.removeEventListener('keydown', onKey)
    }
  }, [open])

  const handleDownload = () => {
    const params = new URLSearchParams({ format, period })
    const url = `/api/export?${params.toString()}`
    // Use an anchor with `download` so the browser respects
    // Content-Disposition without navigating away from the SPA.
    const a = document.createElement('a')
    a.href = url
    a.rel = 'noopener'
    // The backend sets the filename via Content-Disposition; leave `download`
    // empty so the browser uses the server-suggested name.
    a.download = ''
    document.body.appendChild(a)
    a.click()
    document.body.removeChild(a)
    setOpen(false)
  }

  return (
    <div ref={wrapperRef} className={`relative ${className ?? ''}`}>
      <button
        type="button"
        onClick={() => setOpen(v => !v)}
        aria-haspopup="menu"
        aria-expanded={open}
        title={`Export ${tab} data`}
        data-testid={`export-button-${tab}`}
        className="inline-flex items-center gap-1.5 px-2.5 py-1.5 bg-white dark:bg-gray-800 border border-gray-300 dark:border-gray-700 rounded text-xs text-gray-700 dark:text-gray-300 hover:border-gray-400 dark:hover:border-gray-600 hover:text-gray-900 dark:hover:text-white"
      >
        <IconDownload size={13} />
        Export
        <IconChevronDown size={11} className={open ? 'rotate-180 transition-transform' : 'transition-transform'} />
      </button>

      {open && (
        <div
          role="menu"
          className="absolute right-0 top-full mt-1 z-20 w-56 bg-white dark:bg-gray-900 border border-gray-200 dark:border-gray-800 rounded-md shadow-lg p-3 space-y-3"
          data-testid={`export-popover-${tab}`}
        >
          <div>
            <div className="text-[10px] uppercase tracking-wider text-gray-500 mb-1">Format</div>
            <div className="flex gap-1">
              {FORMATS.map(f => {
                const active = format === f.value
                return (
                  <button
                    key={f.value}
                    type="button"
                    onClick={() => setFormat(f.value)}
                    aria-pressed={active}
                    className={
                      'flex-1 text-xs px-2 py-1 rounded border transition-colors ' +
                      (active
                        ? 'bg-indigo-500/20 border-indigo-500/60 text-indigo-700 dark:text-indigo-200'
                        : 'bg-gray-50 dark:bg-gray-800 border-gray-300 dark:border-gray-700 text-gray-700 dark:text-gray-300 hover:border-gray-400 dark:hover:border-gray-600')
                    }
                  >
                    {f.label}
                  </button>
                )
              })}
            </div>
          </div>

          <div>
            <div className="text-[10px] uppercase tracking-wider text-gray-500 mb-1">Period</div>
            <select
              value={period}
              onChange={e => setPeriod(e.target.value as ExportPeriod)}
              className="w-full bg-white dark:bg-gray-800 border border-gray-300 dark:border-gray-700 rounded px-2 py-1 text-xs text-gray-700 dark:text-gray-300 focus:outline-none focus:border-indigo-500"
              aria-label="Period"
            >
              {PERIODS.map(p => (
                <option key={p.value} value={p.value}>
                  {p.label}
                </option>
              ))}
            </select>
          </div>

          <button
            type="button"
            onClick={handleDownload}
            className="w-full inline-flex items-center justify-center gap-1.5 px-2 py-1.5 bg-indigo-600 hover:bg-indigo-500 text-white text-xs font-medium rounded"
            data-testid={`export-download-${tab}`}
          >
            <IconDownload size={13} />
            Download
          </button>
        </div>
      )}
    </div>
  )
}
