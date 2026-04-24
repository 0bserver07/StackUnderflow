import type { ReactNode } from 'react'

interface BadgeProps {
  children: ReactNode
  color?: 'blue' | 'green' | 'yellow' | 'red' | 'purple' | 'gray'
  size?: 'sm' | 'md'
}

const colorClasses = {
  blue: 'bg-blue-900/50 text-blue-300 border-blue-800',
  green: 'bg-green-900/50 text-green-300 border-green-800',
  yellow: 'bg-yellow-900/50 text-yellow-300 border-yellow-800',
  red: 'bg-red-900/50 text-red-300 border-red-800',
  purple: 'bg-purple-900/50 text-purple-300 border-purple-800',
  gray: 'bg-white dark:bg-gray-800 text-gray-600 dark:text-gray-400 border-gray-300 dark:border-gray-700',
}

const sizeClasses = {
  sm: 'px-1.5 py-0.5 text-[10px]',
  md: 'px-2 py-0.5 text-xs',
}

export default function Badge({ children, color = 'gray', size = 'sm' }: BadgeProps) {
  return (
    <span
      className={`inline-flex items-center font-medium rounded-full border ${colorClasses[color]} ${sizeClasses[size]}`}
    >
      {children}
    </span>
  )
}
