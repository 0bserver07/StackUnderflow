import { IconMessageReply } from '@tabler/icons-react'
import type { DiscussionPost } from '../../types/social'
import AgentAvatar from './AgentAvatar'
import VoteButton from './VoteButton'
import Markdown from '../common/Markdown'

interface DiscussionPostItemProps {
  post: DiscussionPost
  depth: number
  qaId: string
  onReply: (parentId: string) => void
}

function timeAgo(dateStr: string): string {
  const seconds = Math.floor((Date.now() - new Date(dateStr).getTime()) / 1000)
  if (seconds < 60) return 'just now'
  const minutes = Math.floor(seconds / 60)
  if (minutes < 60) return `${minutes}m ago`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours}h ago`
  const days = Math.floor(hours / 24)
  return `${days}d ago`
}

const MAX_DEPTH = 4

export default function DiscussionPostItem({ post, depth, qaId, onReply }: DiscussionPostItemProps) {
  const effectiveDepth = Math.min(depth, MAX_DEPTH)

  return (
    <div
      className={effectiveDepth > 0 ? 'pl-4' : ''}
      style={effectiveDepth > 0 ? { borderLeftWidth: 2, borderLeftColor: post.author_color } : undefined}
    >
      <div className="py-2">
        {/* Author header */}
        <div className="flex items-center gap-2 mb-1">
          <AgentAvatar
            emoji={post.author_emoji}
            color={post.author_color}
            size={depth > 0 ? 'sm' : 'md'}
          />
          <div className="flex items-center gap-1.5 text-sm">
            <span className="font-medium text-gray-200">{post.author_name}</span>
            <span className="text-gray-500">{post.author_role}</span>
            <span className="text-gray-600">·</span>
            <span className="text-gray-500 text-xs">{timeAgo(post.created_at)}</span>
          </div>
        </div>

        {/* Content */}
        <div className={depth > 0 ? 'ml-8' : 'ml-10'}>
          <Markdown content={post.content} />

          {/* Actions */}
          <div className="flex items-center gap-3 mt-1">
            <VoteButton
              targetType="discussion"
              targetId={post.id}
              initialCount={post.vote_count}
              initialVoted={post.user_voted}
              size="sm"
            />
            <button
              onClick={() => onReply(post.id)}
              className="flex items-center gap-1 text-xs text-gray-500 hover:text-gray-300"
            >
              <IconMessageReply size={14} />
              Reply
            </button>
          </div>
        </div>
      </div>

      {/* Children */}
      {post.children.length > 0 && (
        <div>
          {post.children.map((child) => (
            <DiscussionPostItem
              key={child.id}
              post={child}
              depth={effectiveDepth + 1}
              qaId={qaId}
              onReply={onReply}
            />
          ))}
        </div>
      )}
    </div>
  )
}
