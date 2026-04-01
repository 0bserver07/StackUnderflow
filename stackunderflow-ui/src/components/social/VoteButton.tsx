import { useState } from 'react'
import { IconArrowBigUp, IconArrowBigUpFilled } from '@tabler/icons-react'
import { toggleVote } from '../../services/social'

interface VoteButtonProps {
  targetType: 'qa' | 'discussion'
  targetId: string
  initialCount: number
  initialVoted: boolean
  size?: 'sm' | 'md'
}

export default function VoteButton({
  targetType,
  targetId,
  initialCount,
  initialVoted,
  size = 'md',
}: VoteButtonProps) {
  const [count, setCount] = useState(initialCount)
  const [voted, setVoted] = useState(initialVoted)
  const [loading, setLoading] = useState(false)

  async function handleClick() {
    if (loading) return
    const prevCount = count
    const prevVoted = voted

    // Optimistic update
    setVoted(!voted)
    setCount(voted ? count - 1 : count + 1)
    setLoading(true)

    try {
      const res = await toggleVote({ target_type: targetType, target_id: targetId })
      setVoted(res.voted)
      setCount(res.new_count)
    } catch {
      // Revert on error
      setVoted(prevVoted)
      setCount(prevCount)
    } finally {
      setLoading(false)
    }
  }

  const iconSize = size === 'sm' ? 16 : 20
  const Icon = voted ? IconArrowBigUpFilled : IconArrowBigUp

  if (size === 'sm') {
    return (
      <button
        onClick={handleClick}
        disabled={loading}
        className="flex items-center gap-1 text-xs disabled:opacity-50"
      >
        <Icon size={iconSize} className={voted ? 'text-blue-400' : 'text-gray-500'} />
        <span className={voted ? 'text-blue-400' : 'text-gray-500'}>{count}</span>
      </button>
    )
  }

  return (
    <button
      onClick={handleClick}
      disabled={loading}
      className="flex flex-col items-center gap-0.5 disabled:opacity-50"
    >
      <Icon size={iconSize} className={voted ? 'text-blue-400' : 'text-gray-500'} />
      <span className={`text-xs ${voted ? 'text-blue-400' : 'text-gray-500'}`}>{count}</span>
    </button>
  )
}
