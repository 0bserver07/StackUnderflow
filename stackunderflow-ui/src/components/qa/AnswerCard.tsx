import Markdown from '../common/Markdown'

interface AnswerCardProps {
  answer: string
  hasCode: boolean
  codeLanguages: string[]
}

export default function AnswerCard({ answer, hasCode, codeLanguages }: AnswerCardProps) {
  return (
    <div className="bg-gray-800 rounded-lg border-l-4 border-green-500 p-5">
      {hasCode && codeLanguages.length > 0 && (
        <div className="flex flex-wrap gap-1.5 mb-3">
          {codeLanguages.map((lang) => (
            <span
              key={lang}
              className="inline-flex items-center px-2 py-0.5 rounded text-xs bg-green-100 text-green-800 border border-green-300 dark:bg-green-900/40 dark:text-green-300 dark:border-green-700 font-mono"
            >
              {lang}
            </span>
          ))}
        </div>
      )}

      <div>
        <Markdown content={answer} />
      </div>
    </div>
  )
}
