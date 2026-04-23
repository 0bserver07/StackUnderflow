import { useState, useMemo } from 'react'
import { IconArrowUp, IconArrowDown, IconSearch, IconDownload } from '@tabler/icons-react'

export interface Column<T> {
  key: string
  label: string
  render: (row: T) => React.ReactNode
  sortValue?: (row: T) => string | number
  align?: 'left' | 'right' | 'center'
  width?: string
}

interface DataTableProps<T> {
  columns: Column<T>[]
  data: T[]
  keyFn: (row: T) => string
  searchable?: boolean
  searchPlaceholder?: string
  searchFn?: (row: T, query: string) => boolean
  onRowClick?: (row: T) => void
  perPageOptions?: number[]
  defaultPerPage?: number
  exportFilename?: string
  exportFn?: (data: T[]) => string
  emptyMessage?: string
}

export default function DataTable<T>({
  columns, data, keyFn, searchable, searchPlaceholder, searchFn,
  onRowClick, perPageOptions = [25, 50, 100], defaultPerPage = 25,
  exportFilename, exportFn, emptyMessage = 'No data',
}: DataTableProps<T>) {
  const [search, setSearch] = useState('')
  const [sortKey, setSortKey] = useState<string | null>(null)
  const [sortDir, setSortDir] = useState<'asc' | 'desc'>('desc')
  const [page, setPage] = useState(1)
  const [perPage, setPerPage] = useState(defaultPerPage)

  const filtered = useMemo(() => {
    let result = data
    if (search && searchFn) {
      const q = search.toLowerCase()
      result = result.filter(row => searchFn(row, q))
    }
    if (sortKey) {
      const col = columns.find(c => c.key === sortKey)
      if (col?.sortValue) {
        const sv = col.sortValue
        result = [...result].sort((a, b) => {
          const av = sv(a)
          const bv = sv(b)
          if (av < bv) return sortDir === 'asc' ? -1 : 1
          if (av > bv) return sortDir === 'asc' ? 1 : -1
          return 0
        })
      }
    }
    return result
  }, [data, search, searchFn, sortKey, sortDir, columns])

  const totalPages = Math.ceil(filtered.length / perPage)
  const paged = filtered.slice((page - 1) * perPage, page * perPage)

  const toggleSort = (key: string) => {
    const col = columns.find(c => c.key === key)
    if (!col?.sortValue) return
    if (sortKey === key) {
      setSortDir(d => d === 'asc' ? 'desc' : 'asc')
    } else {
      setSortKey(key)
      setSortDir('desc')
    }
    setPage(1)
  }

  const handleExport = () => {
    if (!exportFn || !exportFilename) return
    const csv = exportFn(filtered)
    const blob = new Blob([csv], { type: 'text/csv' })
    const url = URL.createObjectURL(blob)
    const a = document.createElement('a')
    a.href = url
    a.download = exportFilename
    a.click()
    URL.revokeObjectURL(url)
  }

  return (
    <div className="space-y-3">
      {/* Controls */}
      <div className="flex items-center gap-3">
        {searchable && (
          <div className="relative flex-1 max-w-xs">
            <IconSearch size={14} className="absolute left-2.5 top-1/2 -translate-y-1/2 text-gray-500" />
            <input
              type="text"
              value={search}
              onChange={e => { setSearch(e.target.value); setPage(1) }}
              placeholder={searchPlaceholder ?? 'Search...'}
              className="w-full bg-white dark:bg-gray-800 border border-gray-300 dark:border-gray-700 rounded pl-8 pr-3 py-1.5 text-sm text-gray-700 dark:text-gray-300 placeholder-gray-500 focus:outline-none focus:border-indigo-500"
            />
          </div>
        )}
        <div className="flex-1" />
        <select
          value={perPage}
          onChange={e => { setPerPage(Number(e.target.value)); setPage(1) }}
          className="bg-white dark:bg-gray-800 border border-gray-300 dark:border-gray-700 rounded px-2 py-1.5 text-xs text-gray-700 dark:text-gray-300"
        >
          {perPageOptions.map(n => <option key={n} value={n}>{n}</option>)}
        </select>
        {exportFn && exportFilename && (
          <button onClick={handleExport} className="flex items-center gap-1 px-2 py-1.5 bg-white dark:bg-gray-800 border border-gray-300 dark:border-gray-700 rounded text-xs text-gray-700 dark:text-gray-300 hover:text-gray-900 dark:hover:text-white hover:border-gray-400 dark:hover:border-gray-600">
            <IconDownload size={14} /> CSV
          </button>
        )}
      </div>

      {/* Table */}
      <div className="bg-gray-100/50 dark:bg-gray-800/30 rounded-lg border border-gray-200 dark:border-gray-800 overflow-hidden">
        <div className="overflow-x-auto">
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-gray-200 dark:border-gray-800 text-gray-600 dark:text-gray-400 text-xs uppercase tracking-wider">
                {columns.map(col => (
                  <th
                    key={col.key}
                    className={`px-4 py-2.5 ${col.align === 'right' ? 'text-right' : col.align === 'center' ? 'text-center' : 'text-left'} ${col.sortValue ? 'cursor-pointer hover:text-gray-800 dark:hover:text-gray-200' : ''}`}
                    style={col.width ? { width: col.width } : undefined}
                    onClick={() => col.sortValue && toggleSort(col.key)}
                  >
                    {col.label}
                    {sortKey === col.key && (
                      sortDir === 'asc'
                        ? <IconArrowUp size={12} className="inline ml-0.5" />
                        : <IconArrowDown size={12} className="inline ml-0.5" />
                    )}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {paged.length === 0 ? (
                <tr><td colSpan={columns.length} className="px-4 py-8 text-center text-gray-500">{emptyMessage}</td></tr>
              ) : paged.map(row => (
                <tr
                  key={keyFn(row)}
                  className={`border-b border-gray-200/50 dark:border-gray-800/50 ${onRowClick ? 'hover:bg-gray-100/70 dark:hover:bg-gray-800/50 cursor-pointer' : ''}`}
                  onClick={() => onRowClick?.(row)}
                >
                  {columns.map(col => (
                    <td key={col.key} className={`px-4 py-2.5 ${col.align === 'right' ? 'text-right' : col.align === 'center' ? 'text-center' : 'text-left'}`}>
                      {col.render(row)}
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>

      {/* Pagination */}
      {totalPages > 1 && (
        <div className="flex items-center justify-between text-xs text-gray-600 dark:text-gray-400">
          <span>
            {(page - 1) * perPage + 1}-{Math.min(page * perPage, filtered.length)} of {filtered.length}
          </span>
          <div className="flex items-center gap-2">
            <button onClick={() => setPage(p => Math.max(1, p - 1))} disabled={page <= 1} className="px-2 py-1 bg-white dark:bg-gray-800 rounded border border-gray-300 dark:border-gray-700 disabled:opacity-50">Prev</button>
            <span>{page}/{totalPages}</span>
            <button onClick={() => setPage(p => Math.min(totalPages, p + 1))} disabled={page >= totalPages} className="px-2 py-1 bg-white dark:bg-gray-800 rounded border border-gray-300 dark:border-gray-700 disabled:opacity-50">Next</button>
          </div>
        </div>
      )}
    </div>
  )
}
