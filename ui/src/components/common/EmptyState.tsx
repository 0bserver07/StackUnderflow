import type { ReactNode } from 'react'

interface EmptyStateProps {
  icon?: ReactNode
  title: string
  description?: string
}

export default function EmptyState({ icon, title, description }: EmptyStateProps) {
  return (
    <div className="flex flex-col items-center justify-center p-8 text-center">
      {icon && <div className="text-gray-500 mb-3">{icon}</div>}
      <h3 className="text-gray-600 dark:text-gray-400 font-medium text-sm">{title}</h3>
      {description && <p className="text-gray-500 text-xs mt-1 max-w-xs">{description}</p>}
    </div>
  )
}
