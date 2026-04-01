interface AgentAvatarProps {
  emoji: string
  color: string
  size?: 'sm' | 'md' | 'lg'
  className?: string
}

const sizeClasses = {
  sm: 'w-6 h-6 text-xs',
  md: 'w-8 h-8 text-sm',
  lg: 'w-10 h-10 text-base',
}

export default function AgentAvatar({ emoji, color, size = 'md', className = '' }: AgentAvatarProps) {
  return (
    <div
      className={`rounded-full border-2 flex items-center justify-center ${sizeClasses[size]} ${className}`}
      style={{ borderColor: color }}
    >
      {emoji}
    </div>
  )
}
