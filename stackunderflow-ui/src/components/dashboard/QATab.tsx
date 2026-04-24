import { useState, useEffect } from 'react'
import { useQuery } from '@tanstack/react-query'
import {
  IconSearch,
  IconRefresh,
  IconChevronLeft,
  IconChevronRight,
  IconMessageQuestion,
  IconCode,
  IconSortDescending,
} from '@tabler/icons-react'
import { getQAList, reindexQA } from '../../services/api'
import type { QAPair, ResolutionStatus } from '../../types/api'
import LoadingSpinner from '../common/LoadingSpinner'
import EmptyState from '../common/EmptyState'
import TimeAgo from '../common/TimeAgo'

interface QATabProps {
  projectName: string
}

const PER_PAGE = 20

type SortMode = 'recent' | 'tools' | 'has_code'
type ResolutionFilter = 'all' | ResolutionStatus

const RESOLUTION_STYLES: Record<ResolutionStatus, { label: string; className: string; title: string }> = {
  resolved: {
    label: 'resolved',
    className: 'bg-emerald-900/40 text-emerald-300 border-emerald-800',
    title: 'Answer appears to have worked — no follow-up frustration detected.',
  },
  looped: {
    label: 'looped',
    className: 'bg-amber-900/40 text-amber-300 border-amber-800',
    title: 'User kept asking variants of the same question — agent may have gone in circles.',
  },
  abandoned: {
    label: 'abandoned',
    className: 'bg-rose-900/40 text-rose-300 border-rose-800',
    title: 'Question never got a follow-up or resolution signal.',
  },
  open: {
    label: 'open',
    className: 'bg-white dark:bg-gray-800 text-gray-600 dark:text-gray-400 border-gray-200 dark:border-gray-700',
    title: 'Not enough signal to classify.',
  },
}

function QAItem({
  qa,
}: {
  qa: QAPair
}) {
  const questionPreview = qa.question_text.length > 200
    ? qa.question_text.slice(0, 200) + '...'
    : qa.question_text
  const answerPreview = qa.answer_text.length > 200
    ? qa.answer_text.slice(0, 200) + '...'
    : qa.answer_text

  return (
    <div
      className="w-full px-4 py-3 border-b border-gray-200 dark:border-gray-800"
    >
      <div className="mb-1.5">
        <p className="text-sm text-gray-800 dark:text-gray-200 font-medium line-clamp-2">
          {questionPreview}
        </p>
      </div>

      <p className="text-xs text-gray-600 dark:text-gray-400 line-clamp-2 mb-2">
        {answerPreview}
      </p>

      <div className="flex items-center gap-2 flex-wrap">
        {qa.resolution_status && qa.resolution_status !== 'open' && (
          <span
            className={`inline-flex items-center gap-0.5 px-1.5 py-0.5 text-[10px] font-medium rounded-full border ${RESOLUTION_STYLES[qa.resolution_status].className}`}
            title={RESOLUTION_STYLES[qa.resolution_status].title}
          >
            {RESOLUTION_STYLES[qa.resolution_status].label}
            {qa.loop_count > 1 && qa.resolution_status === 'looped' && ` ×${qa.loop_count}`}
          </span>
        )}

        {qa.code_snippets.length > 0 && (
          <span className="inline-flex items-center gap-0.5 px-1.5 py-0.5 text-[10px] font-medium rounded-full border bg-purple-900/50 text-purple-300 border-purple-800">
            <IconCode size={10} />
            code
          </span>
        )}

        {qa.tools_used.length > 0 && (
          <span className="text-[10px] text-gray-500 font-mono">
            {qa.tools_used.slice(0, 3).join(', ')}
            {qa.tools_used.length > 3 && ` +${qa.tools_used.length - 3}`}
          </span>
        )}

        {qa.model && (
          <span className="text-[10px] text-gray-600 dark:text-gray-400 font-mono">{qa.model}</span>
        )}

        <span className="ml-auto">
          <TimeAgo timestamp={qa.timestamp} />
        </span>
      </div>
    </div>
  )
}

function sortQAPairs(pairs: QAPair[], mode: SortMode): QAPair[] {
  const sorted = [...pairs]
  switch (mode) {
    case 'recent':
      return sorted.sort(
        (a, b) => new Date(b.timestamp).getTime() - new Date(a.timestamp).getTime()
      )
    case 'tools':
      return sorted.sort((a, b) => b.tools_used.length - a.tools_used.length)
    case 'has_code':
      return sorted.sort((a, b) => {
        const aHas = a.code_snippets.length > 0
        const bHas = b.code_snippets.length > 0
        if (aHas === bHas) {
          return new Date(b.timestamp).getTime() - new Date(a.timestamp).getTime()
        }
        return aHas ? -1 : 1
      })
    default:
      return sorted
  }
}

export default function QATab({ projectName }: QATabProps) {
  const [search, setSearch] = useState('')
  const [page, setPage] = useState(1)
  const [sortMode, setSortMode] = useState<SortMode>('recent')
  const [resolutionFilter, setResolutionFilter] = useState<ResolutionFilter>('all')
  const [reindexing, setReindexing] = useState(false)

  useEffect(() => {
    setPage(1)
  }, [search, resolutionFilter])

  const { data, isLoading } = useQuery({
    queryKey: ['qa-list', projectName, search, resolutionFilter, page],
    queryFn: () =>
      getQAList({
        project: projectName,
        search: search || undefined,
        resolution_status: resolutionFilter === 'all' ? undefined : resolutionFilter,
        page,
        per_page: PER_PAGE,
      }),
  })

  const handleReindex = async () => {
    setReindexing(true)
    try {
      await reindexQA()
    } finally {
      setReindexing(false)
    }
  }

  const pairs = data?.results ?? []
  const sortedPairs = sortQAPairs(pairs, sortMode)
  const totalPages = data?.total_pages ?? 0

  return (
    <div className="flex flex-col h-full">
      {/* Controls */}
      <div className="px-4 py-3 border-b border-gray-200 dark:border-gray-800">
        <div className="flex items-center gap-2">
          <div className="relative flex-1">
            <IconSearch
              size={16}
              className="absolute left-3 top-1/2 -translate-y-1/2 text-gray-500"
            />
            <input
              type="text"
              value={search}
              onChange={(e) => setSearch(e.target.value)}
              placeholder="Filter Q&A pairs..."
              className="w-full pl-9 pr-3 py-2 bg-white dark:bg-gray-800 border border-gray-200 dark:border-gray-700 rounded-md text-sm text-gray-800 dark:text-gray-200 placeholder-gray-500 focus:outline-none focus:border-blue-600 focus:ring-1 focus:ring-blue-600 transition-colors"
            />
          </div>

          {/* Resolution filter */}
          <select
            value={resolutionFilter}
            onChange={(e) => setResolutionFilter(e.target.value as ResolutionFilter)}
            className="appearance-none px-2 py-2 bg-white dark:bg-gray-800 border border-gray-200 dark:border-gray-700 rounded-md text-xs text-gray-700 dark:text-gray-300 focus:outline-none focus:border-blue-600 cursor-pointer"
            title="Filter by resolution status"
          >
            <option value="all">All</option>
            <option value="resolved">Resolved</option>
            <option value="looped">Looped</option>
            <option value="abandoned">Abandoned</option>
            <option value="open">Open</option>
          </select>

          {/* Sort dropdown */}
          <div className="relative">
            <select
              value={sortMode}
              onChange={(e) => setSortMode(e.target.value as SortMode)}
              className="appearance-none pl-7 pr-6 py-2 bg-white dark:bg-gray-800 border border-gray-200 dark:border-gray-700 rounded-md text-xs text-gray-700 dark:text-gray-300 focus:outline-none focus:border-blue-600 cursor-pointer"
            >
              <option value="recent">Recent</option>
              <option value="tools">Most Tools</option>
              <option value="has_code">Has Code</option>
            </select>
            <IconSortDescending
              size={14}
              className="absolute left-2 top-1/2 -translate-y-1/2 text-gray-500 pointer-events-none"
            />
          </div>

          <button
            onClick={handleReindex}
            disabled={reindexing}
            className="flex items-center gap-1.5 px-3 py-2 text-xs font-medium text-gray-600 dark:text-gray-400 bg-white dark:bg-gray-800 border border-gray-200 dark:border-gray-700 rounded-md hover:text-gray-800 dark:hover:text-gray-200 hover:border-gray-300 dark:hover:border-gray-600 disabled:opacity-50 transition-colors"
            title="Reindex Q&A"
          >
            <IconRefresh size={14} className={reindexing ? 'animate-spin' : ''} />
            Reindex
          </button>
        </div>

        {data && (
          <p className="mt-2 text-xs text-gray-500">
            {data.total} Q&A pair{data.total !== 1 ? 's' : ''}
          </p>
        )}
      </div>

      {/* Content */}
      <div className="flex-1 overflow-auto">
        {isLoading && <LoadingSpinner message="Loading Q&A pairs..." />}

        {!isLoading && sortedPairs.length === 0 && (
          <EmptyState
            icon={<IconMessageQuestion size={32} />}
            title="No Q&A pairs found"
            description={
              search
                ? `No pairs match "${search}". Try a different filter.`
                : 'No Q&A pairs have been extracted for this project yet.'
            }
          />
        )}

        {sortedPairs.length > 0 && (
          <div>
            {sortedPairs.map((qa) => (
              <QAItem
                key={qa.id}
                qa={qa}
              />
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
