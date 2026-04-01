import { useState } from 'react'
import { useQuery, useQueryClient } from '@tanstack/react-query'
import { IconRobot, IconSend, IconX } from '@tabler/icons-react'
import type { QADetailResponse } from '../../types/api'
import { getDiscussionTree, postDiscussion, triggerDiscussion } from '../../services/social'
import LoadingSpinner from '../common/LoadingSpinner'
import DiscussionPostItem from './DiscussionPostItem'
import SimulationProgress from './SimulationProgress'

interface DiscussionThreadProps {
  qaId: string
  qa?: QADetailResponse | null
}

export default function DiscussionThread({ qaId }: DiscussionThreadProps) {
  const queryClient = useQueryClient()
  const [runId, setRunId] = useState<string | null>(null)
  const [replyTo, setReplyTo] = useState<string | null>(null)
  const [replyContent, setReplyContent] = useState('')
  const [commentContent, setCommentContent] = useState('')
  const [posting, setPosting] = useState(false)
  const [triggering, setTriggering] = useState(false)

  const { data: tree, isLoading, error } = useQuery({
    queryKey: ['discussion-tree', qaId],
    queryFn: () => getDiscussionTree(qaId),
  })

  async function handleTriggerSimulation() {
    if (triggering) return
    setTriggering(true)
    try {
      const run = await triggerDiscussion(qaId)
      setRunId(run.id)
    } catch {
      // Failed to trigger
    } finally {
      setTriggering(false)
    }
  }

  function handleSimulationComplete() {
    setRunId(null)
    queryClient.invalidateQueries({ queryKey: ['discussion-tree', qaId] })
  }

  async function handlePostReply() {
    if (!replyContent.trim() || posting) return
    setPosting(true)
    try {
      await postDiscussion(qaId, replyContent.trim(), replyTo)
      setReplyContent('')
      setReplyTo(null)
      queryClient.invalidateQueries({ queryKey: ['discussion-tree', qaId] })
    } catch {
      // Failed to post
    } finally {
      setPosting(false)
    }
  }

  async function handlePostComment() {
    if (!commentContent.trim() || posting) return
    setPosting(true)
    try {
      await postDiscussion(qaId, commentContent.trim())
      setCommentContent('')
      queryClient.invalidateQueries({ queryKey: ['discussion-tree', qaId] })
    } catch {
      // Failed to post
    } finally {
      setPosting(false)
    }
  }

  return (
    <div className="border border-gray-700 rounded-lg bg-gray-900/50">
      {/* Header */}
      <div className="flex items-center justify-between p-4 border-b border-gray-700">
        <div className="flex items-center gap-2">
          <h3 className="text-sm font-semibold text-gray-200">Discussion</h3>
          {tree && (
            <span className="text-xs text-gray-500">
              {tree.total_count} post{tree.total_count !== 1 ? 's' : ''}
            </span>
          )}
        </div>
        <button
          onClick={handleTriggerSimulation}
          disabled={triggering || !!runId}
          className="flex items-center gap-1.5 px-3 py-1.5 text-xs font-medium rounded-md bg-blue-600 hover:bg-blue-500 text-white disabled:opacity-50 disabled:cursor-not-allowed"
        >
          <IconRobot size={14} />
          Get Agent Opinions
        </button>
      </div>

      {/* Simulation progress */}
      {runId && (
        <div className="p-4 border-b border-gray-700">
          <SimulationProgress runId={runId} onComplete={handleSimulationComplete} />
        </div>
      )}

      {/* Post list */}
      <div className="max-h-[600px] overflow-y-auto p-4">
        {isLoading && <LoadingSpinner size="sm" message="Loading discussion..." />}
        {error && <p className="text-sm text-red-400">Failed to load discussion.</p>}
        {tree && tree.posts.length === 0 && !isLoading && (
          <p className="text-sm text-gray-500 text-center py-4">
            No discussion yet. Be the first to comment or get agent opinions.
          </p>
        )}
        {tree?.posts.map((post) => (
          <DiscussionPostItem
            key={post.id}
            post={post}
            depth={0}
            qaId={qaId}
            onReply={(parentId) => {
              setReplyTo(parentId)
              setReplyContent('')
            }}
          />
        ))}
      </div>

      {/* Reply form (shown when replying to a post) */}
      {replyTo && (
        <div className="p-4 border-t border-gray-700 bg-gray-900/80">
          <div className="flex items-center justify-between mb-2">
            <span className="text-xs text-gray-400">Replying to post</span>
            <button
              onClick={() => setReplyTo(null)}
              className="text-gray-500 hover:text-gray-300"
            >
              <IconX size={14} />
            </button>
          </div>
          <textarea
            value={replyContent}
            onChange={(e) => setReplyContent(e.target.value)}
            placeholder="Write your reply..."
            rows={3}
            className="w-full bg-gray-800 border border-gray-700 rounded-md px-3 py-2 text-sm text-gray-200 placeholder-gray-500 focus:outline-none focus:border-blue-500 resize-none"
          />
          <div className="flex justify-end gap-2 mt-2">
            <button
              onClick={() => setReplyTo(null)}
              className="px-3 py-1.5 text-xs text-gray-400 hover:text-gray-200"
            >
              Cancel
            </button>
            <button
              onClick={handlePostReply}
              disabled={!replyContent.trim() || posting}
              className="flex items-center gap-1 px-3 py-1.5 text-xs font-medium rounded-md bg-blue-600 hover:bg-blue-500 text-white disabled:opacity-50"
            >
              <IconSend size={12} />
              Reply
            </button>
          </div>
        </div>
      )}

      {/* Comment input (always visible) */}
      <div className="p-4 border-t border-gray-700">
        <div className="flex gap-2">
          <textarea
            value={commentContent}
            onChange={(e) => setCommentContent(e.target.value)}
            placeholder="Add a comment..."
            rows={2}
            className="flex-1 bg-gray-800 border border-gray-700 rounded-md px-3 py-2 text-sm text-gray-200 placeholder-gray-500 focus:outline-none focus:border-blue-500 resize-none"
          />
          <button
            onClick={handlePostComment}
            disabled={!commentContent.trim() || posting}
            className="self-end px-3 py-2 rounded-md bg-blue-600 hover:bg-blue-500 text-white disabled:opacity-50"
          >
            <IconSend size={16} />
          </button>
        </div>
      </div>
    </div>
  )
}
