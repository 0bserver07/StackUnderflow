import Markdown from '../common/Markdown'

interface QuestionCardProps {
  question: string
  tags: string[]
  timestamp: string
  model?: string
}

const TAG_COLORS = [
  'bg-blue-100 text-blue-800 border-blue-300 dark:bg-blue-900/50 dark:text-blue-300 dark:border-blue-700',
  'bg-purple-100 text-purple-800 border-purple-300 dark:bg-purple-900/50 dark:text-purple-300 dark:border-purple-700',
  'bg-emerald-100 text-emerald-800 border-emerald-300 dark:bg-emerald-900/50 dark:text-emerald-300 dark:border-emerald-700',
  'bg-amber-100 text-amber-800 border-amber-300 dark:bg-amber-900/50 dark:text-amber-300 dark:border-amber-700',
  'bg-rose-100 text-rose-800 border-rose-300 dark:bg-rose-900/50 dark:text-rose-300 dark:border-rose-700',
  'bg-cyan-100 text-cyan-800 border-cyan-300 dark:bg-cyan-900/50 dark:text-cyan-300 dark:border-cyan-700',
  'bg-indigo-100 text-indigo-800 border-indigo-300 dark:bg-indigo-900/50 dark:text-indigo-300 dark:border-indigo-700',
]

function getTagColor(tag: string): string {
  let hash = 0
  for (let i = 0; i < tag.length; i++) {
    hash = tag.charCodeAt(i) + ((hash << 5) - hash)
  }
  const idx = Math.abs(hash) % TAG_COLORS.length
  return TAG_COLORS[idx] as string
}

function formatTimestamp(ts: string): string {
  try {
    const date = new Date(ts)
    return date.toLocaleString(undefined, {
      month: 'short',
      day: 'numeric',
      year: 'numeric',
      hour: '2-digit',
      minute: '2-digit',
    })
  } catch {
    return ts
  }
}

export default function QuestionCard({ question, tags, timestamp, model }: QuestionCardProps) {
  return (
    <div className="bg-white dark:bg-gray-800 rounded-lg border-l-4 border-blue-500 p-5">
      <div className="flex items-center gap-3 mb-3 text-xs text-gray-600 dark:text-gray-400">
        <span>{formatTimestamp(timestamp)}</span>
        {model && (
          <>
            <span className="text-gray-500">|</span>
            <span className="font-mono text-gray-500">{model}</span>
          </>
        )}
      </div>

      <div className="mb-4">
        <Markdown content={question} />
      </div>

      {tags.length > 0 && (
        <div className="flex flex-wrap gap-1.5">
          {tags.map((tag) => (
            <span
              key={tag}
              className={`inline-flex items-center px-2 py-0.5 rounded text-xs border ${getTagColor(tag)}`}
            >
              {tag}
            </span>
          ))}
        </div>
      )}
    </div>
  )
}
