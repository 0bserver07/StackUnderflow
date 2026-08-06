function formatTimeAgo(timestamp: string | number): string {
  const date = typeof timestamp === 'number'
    ? new Date(timestamp < 1e12 ? timestamp * 1000 : timestamp)
    : new Date(timestamp)

  const now = Date.now()
  const diffMs = now - date.getTime()

  if (diffMs < 0) return 'just now'

  const seconds = Math.floor(diffMs / 1000)
  const minutes = Math.floor(seconds / 60)
  const hours = Math.floor(minutes / 60)
  const days = Math.floor(hours / 24)

  if (seconds < 60) return 'just now'
  if (minutes < 60) return `${minutes}m ago`
  if (hours < 24) return `${hours}h ago`
  if (days < 30) return `${days}d ago`

  return date.toLocaleDateString('en-US', {
    month: 'short',
    day: 'numeric',
    year: date.getFullYear() !== new Date().getFullYear() ? 'numeric' : undefined,
  })
}

export default function TimeAgo({ timestamp }: { timestamp: string | number }) {
  const text = formatTimeAgo(timestamp)

  const date = typeof timestamp === 'number'
    ? new Date(timestamp < 1e12 ? timestamp * 1000 : timestamp)
    : new Date(timestamp)

  return (
    <time
      dateTime={date.toISOString()}
      title={date.toLocaleString()}
      className="text-gray-500 text-xs"
    >
      {text}
    </time>
  )
}
