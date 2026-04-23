import { useState, useEffect, useRef, useCallback } from 'react'
import { useQuery } from '@tanstack/react-query'
import {
  IconSearch,
  IconRefresh,
  IconChevronLeft,
  IconChevronRight,
  IconAlertCircle,
} from '@tabler/icons-react'
import { searchMessages, reindexSearch } from '../../services/api'
import type { SearchResult } from '../../types/api'
import LoadingSpinner from '../common/LoadingSpinner'
import EmptyState from '../common/EmptyState'
import Badge from '../common/Badge'
import TimeAgo from '../common/TimeAgo'

interface SearchTabProps {
  projectName: string
  initialQuery?: string
}

const PER_PAGE = 20

function useDebounce<T>(value: T, delay: number): T {
  const [debounced, setDebounced] = useState(value)

  useEffect(() => {
    const timer = setTimeout(() => setDebounced(value), delay)
    return () => clearTimeout(timer)
  }, [value, delay])

  return debounced
}

function roleBadgeColor(role: string): 'blue' | 'green' | 'yellow' | 'purple' | 'gray' {
  switch (role) {
    case 'user':
      return 'blue'
    case 'assistant':
      return 'green'
    case 'system':
      return 'yellow'
    case 'tool':
      return 'purple'
    default:
      return 'gray'
  }
}

/**
 * Safely render a search snippet that may contain <mark> tags for highlighting.
 * Instead of using dangerouslySetInnerHTML, we parse the string and render
 * <mark> tags as React elements while escaping everything else as plain text.
 */
function HighlightedSnippet({ html }: { html: string }) {
  // Strip every HTML tag except <mark> and </mark>, then split on mark boundaries
  const stripped = html.replace(/<\/?(?!mark\b)[a-z][^>]*>/gi, '')
  const parts = stripped.split(/(<mark>|<\/mark>)/gi)

  const elements: React.ReactNode[] = []
  let inMark = false

  for (let i = 0; i < parts.length; i++) {
    const part = parts[i]!
    if (part.toLowerCase() === '<mark>') {
      inMark = true
      continue
    }
    if (part.toLowerCase() === '</mark>') {
      inMark = false
      continue
    }
    if (part === '') continue

    if (inMark) {
      elements.push(
        <mark key={i} className="bg-yellow-500/30 text-yellow-200 rounded-sm px-0.5">
          {part}
        </mark>,
      )
    } else {
      elements.push(part)
    }
  }

  return (
    <span className="text-sm text-gray-700 dark:text-gray-300 leading-relaxed">
      {elements}
    </span>
  )
}

function SearchResultItem({ result }: { result: SearchResult }) {
  return (
    <div className="px-4 py-3 border-b border-gray-200 dark:border-gray-800 hover:bg-gray-100/70 dark:hover:bg-gray-800/50 transition-colors">
      <div className="flex items-start justify-between gap-3">
        <div className="flex-1 min-w-0">
          <div className="mb-1.5">
            {result.snippet ? (
              <HighlightedSnippet html={result.snippet} />
            ) : (
              <p className="text-sm text-gray-700 dark:text-gray-300 leading-relaxed line-clamp-3">
                {result.content.length > 300 ? result.content.slice(0, 300) + '...' : result.content}
              </p>
            )}
          </div>

          <div className="flex items-center gap-2 flex-wrap">
            <Badge color={roleBadgeColor(result.role)}>{result.role}</Badge>
            {result.model && result.model !== 'N/A' && (
              <span className="text-[10px] text-gray-500 font-mono">{result.model}</span>
            )}
            <span className="text-[10px] text-gray-600 dark:text-gray-400 font-mono truncate max-w-[180px]">
              {result.session_id}
            </span>
            <TimeAgo timestamp={result.timestamp} />
          </div>
        </div>

        <div className="flex-shrink-0 text-right">
          <span className="text-[10px] font-mono text-gray-500">
            {result.relevance.toFixed(2)}
          </span>
        </div>
      </div>
    </div>
  )
}

export default function SearchTab({ projectName, initialQuery = '' }: SearchTabProps) {
  const [query, setQuery] = useState(initialQuery)
  const [page, setPage] = useState(1)
  const [reindexing, setReindexing] = useState(false)
  const inputRef = useRef<HTMLInputElement>(null)

  const debouncedQuery = useDebounce(query, 300)

  // Reset page when query changes
  useEffect(() => {
    setPage(1)
  }, [debouncedQuery])

  // Auto-focus input
  useEffect(() => {
    inputRef.current?.focus()
  }, [])

  const { data, isLoading, isError, error } = useQuery({
    queryKey: ['search', projectName, debouncedQuery, page],
    queryFn: () =>
      searchMessages({
        q: debouncedQuery,
        project: projectName,
        page,
        per_page: PER_PAGE,
      }),
    enabled: debouncedQuery.length > 0,
  })

  const handleReindex = useCallback(async () => {
    setReindexing(true)
    try {
      await reindexSearch()
    } finally {
      setReindexing(false)
    }
  }, [])

  const totalPages = data ? Math.ceil(data.total / PER_PAGE) : 0

  return (
    <div className="flex flex-col h-full">
      {/* Search bar */}
      <div className="px-4 py-3 border-b border-gray-200 dark:border-gray-800">
        <div className="flex items-center gap-2">
          <div className="relative flex-1">
            <IconSearch
              size={16}
              className="absolute left-3 top-1/2 -translate-y-1/2 text-gray-500"
            />
            <input
              ref={inputRef}
              type="text"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder="Search messages..."
              className="w-full pl-9 pr-3 py-2 bg-white dark:bg-gray-800 border border-gray-200 dark:border-gray-700 rounded-md text-sm text-gray-800 dark:text-gray-200 placeholder-gray-500 focus:outline-none focus:border-blue-600 focus:ring-1 focus:ring-blue-600 transition-colors"
            />
          </div>
          <button
            onClick={handleReindex}
            disabled={reindexing}
            className="flex items-center gap-1.5 px-3 py-2 text-xs font-medium text-gray-600 dark:text-gray-400 bg-white dark:bg-gray-800 border border-gray-200 dark:border-gray-700 rounded-md hover:text-gray-800 dark:hover:text-gray-200 hover:border-gray-300 dark:hover:border-gray-600 disabled:opacity-50 transition-colors"
            title="Reindex search"
          >
            <IconRefresh size={14} className={reindexing ? 'animate-spin' : ''} />
            Reindex
          </button>
        </div>

        {/* Results count */}
        {data && debouncedQuery && (
          <p className="mt-2 text-xs text-gray-500">
            {data.total} result{data.total !== 1 ? 's' : ''} for &ldquo;{data.query}&rdquo;
          </p>
        )}
      </div>

      {/* Content */}
      <div className="flex-1 overflow-auto">
        {!debouncedQuery && (
          <EmptyState
            icon={<IconSearch size={32} />}
            title="Search messages"
            description="Enter a query to search across all messages in this project."
          />
        )}

        {debouncedQuery && isLoading && (
          <LoadingSpinner message="Searching..." />
        )}

        {isError && (
          <div className="flex flex-col items-center justify-center p-8 text-center">
            <IconAlertCircle size={32} className="text-red-400 mb-2" />
            <p className="text-sm text-red-400 font-medium">Search failed</p>
            <p className="text-xs text-gray-500 mt-1">
              {error instanceof Error ? error.message : 'An unexpected error occurred'}
            </p>
          </div>
        )}

        {data && data.results.length === 0 && (
          <EmptyState
            icon={<IconSearch size={32} />}
            title="No results found"
            description={`No messages match "${debouncedQuery}". Try a different search term.`}
          />
        )}

        {data && data.results.length > 0 && (
          <div>
            {data.results.map((result, idx) => (
              <SearchResultItem key={`${result.session_id}-${idx}`} result={result} />
            ))}
          </div>
        )}
      </div>

      {/* Pagination */}
      {data && totalPages > 1 && (
        <div className="flex items-center justify-between px-4 py-2 border-t border-gray-200 dark:border-gray-800">
          <button
            onClick={() => setPage((p) => Math.max(1, p - 1))}
            disabled={page <= 1}
            className="flex items-center gap-1 px-2 py-1 text-xs text-gray-600 dark:text-gray-400 hover:text-gray-800 dark:hover:text-gray-200 disabled:opacity-40 disabled:cursor-not-allowed transition-colors"
          >
            <IconChevronLeft size={14} />
            Prev
          </button>

          <span className="text-xs text-gray-500">
            Page {page} of {totalPages}
          </span>

          <button
            onClick={() => setPage((p) => Math.min(totalPages, p + 1))}
            disabled={page >= totalPages}
            className="flex items-center gap-1 px-2 py-1 text-xs text-gray-600 dark:text-gray-400 hover:text-gray-800 dark:hover:text-gray-200 disabled:opacity-40 disabled:cursor-not-allowed transition-colors"
          >
            Next
            <IconChevronRight size={14} />
          </button>
        </div>
      )}
    </div>
  )
}
