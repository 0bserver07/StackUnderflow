import { IconX } from '@tabler/icons-react'

interface TagChipProps {
  tag: string
  onClick?: () => void
  onRemove?: () => void
  size?: 'sm' | 'md'
}

const tagColors = [
  'bg-blue-100 text-blue-800 border-blue-300 dark:bg-blue-900/40 dark:text-blue-300 dark:border-blue-800/60',
  'bg-green-100 text-green-800 border-green-300 dark:bg-green-900/40 dark:text-green-300 dark:border-green-800/60',
  'bg-purple-100 text-purple-800 border-purple-300 dark:bg-purple-900/40 dark:text-purple-300 dark:border-purple-800/60',
  'bg-yellow-100 text-yellow-800 border-yellow-300 dark:bg-yellow-900/40 dark:text-yellow-300 dark:border-yellow-800/60',
  'bg-pink-100 text-pink-800 border-pink-300 dark:bg-pink-900/40 dark:text-pink-300 dark:border-pink-800/60',
  'bg-cyan-100 text-cyan-800 border-cyan-300 dark:bg-cyan-900/40 dark:text-cyan-300 dark:border-cyan-800/60',
  'bg-orange-100 text-orange-800 border-orange-300 dark:bg-orange-900/40 dark:text-orange-300 dark:border-orange-800/60',
  'bg-indigo-100 text-indigo-800 border-indigo-300 dark:bg-indigo-900/40 dark:text-indigo-300 dark:border-indigo-800/60',
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
          className="hover:text-gray-900 dark:hover:text-white transition-colors -mr-0.5"
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
