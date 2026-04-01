import { useState, useMemo } from 'react'
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import {
  IconBookmark,
  IconTrash,
  IconEdit,
  IconCheck,
  IconX,
  IconSortDescending,
  IconFilter,
} from '@tabler/icons-react'
import { getBookmarks, removeBookmark, updateBookmark } from '../../services/api'
import type { Bookmark } from '../../types/api'
import LoadingSpinner from '../common/LoadingSpinner'
import EmptyState from '../common/EmptyState'
import TagChip from '../common/TagChip'
import Modal from '../common/Modal'
import TimeAgo from '../common/TimeAgo'

type SortBy = 'created_at' | 'title'

function BookmarkItem({
  bookmark,
  onDelete,
  onEdit,
}: {
  bookmark: Bookmark
  onDelete: () => void
  onEdit: () => void
}) {
  const [confirmDelete, setConfirmDelete] = useState(false)

  return (
    <div className="px-4 py-3 border-b border-gray-800 hover:bg-gray-800/30 transition-colors">
      <div className="flex items-start justify-between gap-3">
        <div className="flex-1 min-w-0">
          <h3 className="text-sm font-medium text-gray-200 truncate">{bookmark.title}</h3>

          <p className="text-[10px] text-gray-500 font-mono mt-0.5 truncate">
            {bookmark.session_id}
          </p>

          {bookmark.notes && (
            <p className="text-xs text-gray-400 mt-1 line-clamp-2">{bookmark.notes}</p>
          )}

          <div className="flex items-center gap-2 mt-2 flex-wrap">
            {bookmark.tags.map((tag) => (
              <TagChip key={tag} tag={tag} size="sm" />
            ))}
            <TimeAgo timestamp={bookmark.created_at} />
          </div>
        </div>

        <div className="flex items-center gap-1 flex-shrink-0">
          <button
            onClick={onEdit}
            className="p-1 text-gray-500 hover:text-gray-300 rounded hover:bg-gray-700 transition-colors"
            title="Edit bookmark"
          >
            <IconEdit size={14} />
          </button>

          {confirmDelete ? (
            <div className="flex items-center gap-0.5">
              <button
                onClick={() => {
                  onDelete()
                  setConfirmDelete(false)
                }}
                className="p-1 text-red-400 hover:text-red-300 rounded hover:bg-red-900/30 transition-colors"
                title="Confirm delete"
              >
                <IconCheck size={14} />
              </button>
              <button
                onClick={() => setConfirmDelete(false)}
                className="p-1 text-gray-500 hover:text-gray-300 rounded hover:bg-gray-700 transition-colors"
                title="Cancel"
              >
                <IconX size={14} />
              </button>
            </div>
          ) : (
            <button
              onClick={() => setConfirmDelete(true)}
              className="p-1 text-gray-500 hover:text-red-400 rounded hover:bg-gray-700 transition-colors"
              title="Delete bookmark"
            >
              <IconTrash size={14} />
            </button>
          )}
        </div>
      </div>
    </div>
  )
}

function EditBookmarkModal({
  bookmark,
  isOpen,
  onClose,
  onSave,
  isSaving,
}: {
  bookmark: Bookmark
  isOpen: boolean
  onClose: () => void
  onSave: (data: { title: string; notes: string; tags: string[] }) => void
  isSaving: boolean
}) {
  const [title, setTitle] = useState(bookmark.title)
  const [notes, setNotes] = useState(bookmark.notes)
  const [tagsInput, setTagsInput] = useState(bookmark.tags.join(', '))

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault()
    const tags = tagsInput
      .split(',')
      .map((t) => t.trim())
      .filter(Boolean)
    onSave({ title, notes, tags })
  }

  return (
    <Modal isOpen={isOpen} onClose={onClose} title="Edit Bookmark">
      <form onSubmit={handleSubmit} className="space-y-3">
        <div>
          <label className="block text-xs font-medium text-gray-400 mb-1">Title</label>
          <input
            type="text"
            value={title}
            onChange={(e) => setTitle(e.target.value)}
            className="w-full px-3 py-2 bg-gray-900 border border-gray-700 rounded-md text-sm text-gray-200 focus:outline-none focus:border-blue-600 focus:ring-1 focus:ring-blue-600 transition-colors"
            required
          />
        </div>

        <div>
          <label className="block text-xs font-medium text-gray-400 mb-1">Notes</label>
          <textarea
            value={notes}
            onChange={(e) => setNotes(e.target.value)}
            rows={3}
            className="w-full px-3 py-2 bg-gray-900 border border-gray-700 rounded-md text-sm text-gray-200 focus:outline-none focus:border-blue-600 focus:ring-1 focus:ring-blue-600 transition-colors resize-none"
          />
        </div>

        <div>
          <label className="block text-xs font-medium text-gray-400 mb-1">
            Tags (comma-separated)
          </label>
          <input
            type="text"
            value={tagsInput}
            onChange={(e) => setTagsInput(e.target.value)}
            placeholder="tag1, tag2, tag3"
            className="w-full px-3 py-2 bg-gray-900 border border-gray-700 rounded-md text-sm text-gray-200 placeholder-gray-600 focus:outline-none focus:border-blue-600 focus:ring-1 focus:ring-blue-600 transition-colors"
          />
        </div>

        <div className="flex justify-end gap-2 pt-2">
          <button
            type="button"
            onClick={onClose}
            className="px-3 py-1.5 text-xs font-medium text-gray-400 bg-gray-800 border border-gray-700 rounded-md hover:text-gray-200 hover:border-gray-600 transition-colors"
          >
            Cancel
          </button>
          <button
            type="submit"
            disabled={isSaving || !title.trim()}
            className="px-3 py-1.5 text-xs font-medium text-white bg-blue-600 rounded-md hover:bg-blue-500 disabled:opacity-50 transition-colors"
          >
            {isSaving ? 'Saving...' : 'Save'}
          </button>
        </div>
      </form>
    </Modal>
  )
}

export default function BookmarksTab() {
  const queryClient = useQueryClient()
  const [sortBy, setSortBy] = useState<SortBy>('created_at')
  const [filterTag, setFilterTag] = useState<string>('')
  const [editingBookmark, setEditingBookmark] = useState<Bookmark | null>(null)

  const { data, isLoading } = useQuery({
    queryKey: ['bookmarks', filterTag, sortBy],
    queryFn: () => getBookmarks(filterTag || undefined, sortBy),
  })

  const deleteMutation = useMutation({
    mutationFn: (bookmarkId: string) => removeBookmark(bookmarkId),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['bookmarks'] })
    },
  })

  const updateMutation = useMutation({
    mutationFn: ({
      id,
      data: updateData,
    }: {
      id: string
      data: { title?: string; notes?: string; tags?: string[] }
    }) => updateBookmark(id, updateData),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['bookmarks'] })
      setEditingBookmark(null)
    },
  })

  // Collect all unique tags across bookmarks for the filter dropdown
  const allTags = useMemo(() => {
    if (!data?.bookmarks) return []
    const tagSet = new Set<string>()
    data.bookmarks.forEach((b) => b.tags.forEach((t) => tagSet.add(t)))
    return Array.from(tagSet).sort()
  }, [data])

  const bookmarks = data?.bookmarks ?? []

  return (
    <div className="flex flex-col h-full">
      {/* Controls */}
      <div className="px-4 py-3 border-b border-gray-800">
        <div className="flex items-center gap-2">
          {/* Tag filter */}
          <div className="relative flex-1">
            <IconFilter
              size={14}
              className="absolute left-2.5 top-1/2 -translate-y-1/2 text-gray-500 pointer-events-none"
            />
            <select
              value={filterTag}
              onChange={(e) => setFilterTag(e.target.value)}
              className="w-full appearance-none pl-8 pr-6 py-2 bg-gray-800 border border-gray-700 rounded-md text-xs text-gray-300 focus:outline-none focus:border-blue-600 cursor-pointer"
            >
              <option value="">All tags</option>
              {allTags.map((tag) => (
                <option key={tag} value={tag}>
                  {tag}
                </option>
              ))}
            </select>
          </div>

          {/* Sort */}
          <div className="relative">
            <select
              value={sortBy}
              onChange={(e) => setSortBy(e.target.value as SortBy)}
              className="appearance-none pl-7 pr-6 py-2 bg-gray-800 border border-gray-700 rounded-md text-xs text-gray-300 focus:outline-none focus:border-blue-600 cursor-pointer"
            >
              <option value="created_at">Date</option>
              <option value="title">Title</option>
            </select>
            <IconSortDescending
              size={14}
              className="absolute left-2 top-1/2 -translate-y-1/2 text-gray-500 pointer-events-none"
            />
          </div>
        </div>

        {data && (
          <p className="mt-2 text-xs text-gray-500">
            {bookmarks.length} bookmark{bookmarks.length !== 1 ? 's' : ''}
          </p>
        )}
      </div>

      {/* Content */}
      <div className="flex-1 overflow-auto">
        {isLoading && <LoadingSpinner message="Loading bookmarks..." />}

        {!isLoading && bookmarks.length === 0 && (
          <EmptyState
            icon={<IconBookmark size={32} />}
            title="No bookmarks yet"
            description={
              filterTag
                ? `No bookmarks with tag "${filterTag}".`
                : 'Bookmark sessions to find them quickly later.'
            }
          />
        )}

        {bookmarks.length > 0 && (
          <div>
            {bookmarks.map((bookmark) => (
              <BookmarkItem
                key={bookmark.id}
                bookmark={bookmark}
                onDelete={() => deleteMutation.mutate(bookmark.id)}
                onEdit={() => setEditingBookmark(bookmark)}
              />
            ))}
          </div>
        )}
      </div>

      {/* Edit modal */}
      {editingBookmark && (
        <EditBookmarkModal
          bookmark={editingBookmark}
          isOpen={true}
          onClose={() => setEditingBookmark(null)}
          onSave={(updateData) =>
            updateMutation.mutate({ id: editingBookmark.id, data: updateData })
          }
          isSaving={updateMutation.isPending}
        />
      )}
    </div>
  )
}
