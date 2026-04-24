import Markdown from '../common/Markdown'

interface QuestionCardProps {
  question: string
  tags: string[]
  timestamp: string
  model?: string
}

const TAG_COLORS = [
  'bg-blue-900/50 text-blue-300 border-blue-700',
  'bg-purple-900/50 text-purple-300 border-purple-700',
  'bg-emerald-900/50 text-emerald-300 border-emerald-700',
  'bg-amber-900/50 text-amber-300 border-amber-700',
  'bg-rose-900/50 text-rose-300 border-rose-700',
  'bg-cyan-900/50 text-cyan-300 border-cyan-700',
  'bg-indigo-900/50 text-indigo-300 border-indigo-700',
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
    <div className="bg-gray-800 rounded-lg border-l-4 border-blue-500 p-5">
      <div className="flex items-center gap-3 mb-3 text-xs text-gray-400">
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
