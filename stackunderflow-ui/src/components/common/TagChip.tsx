import { IconX } from '@tabler/icons-react'

interface TagChipProps {
  tag: string
  onClick?: () => void
  onRemove?: () => void
  size?: 'sm' | 'md'
}

const tagColors = [
  'bg-blue-900/40 text-blue-300 border-blue-800/60',
  'bg-green-900/40 text-green-300 border-green-800/60',
  'bg-purple-900/40 text-purple-300 border-purple-800/60',
  'bg-yellow-900/40 text-yellow-300 border-yellow-800/60',
  'bg-pink-900/40 text-pink-300 border-pink-800/60',
  'bg-cyan-900/40 text-cyan-300 border-cyan-800/60',
  'bg-orange-900/40 text-orange-300 border-orange-800/60',
  'bg-indigo-900/40 text-indigo-300 border-indigo-800/60',
]

function hashString(str: string): number {
  let hash = 0
  for (let i = 0; i < str.length; i++) {
    hash = ((hash << 5) - hash + str.charCodeAt(i)) | 0
  }
  return Math.abs(hash)
}

const sizeClasses = {
  sm: 'px-1.5 py-0.5 text-[10px] gap-1',
  md: 'px-2 py-0.5 text-xs gap-1.5',
}

export default function TagChip({ tag, onClick, onRemove, size = 'sm' }: TagChipProps) {
  const colorIndex = hashString(tag) % tagColors.length
  const colorClass = tagColors[colorIndex]

  const baseClasses = `inline-flex items-center font-medium rounded-full border ${colorClass} ${sizeClasses[size]}`
  const interactiveClasses = onClick ? 'cursor-pointer hover:brightness-125 transition-all' : ''

  const content = (
    <>
      <span>{tag}</span>
      {onRemove && (
        <button
          onClick={(e) => {
            e.stopPropagation()
            onRemove()
          }}
          className="hover:text-white transition-colors -mr-0.5"
          aria-label={`Remove ${tag}`}
        >
          <IconX size={size === 'sm' ? 10 : 12} />
        </button>
      )}
    </>
  )

  if (onClick) {
    return (
      <button onClick={onClick} className={`${baseClasses} ${interactiveClasses}`}>
        {content}
      </button>
    )
  }

  return <span className={baseClasses}>{content}</span>
}
