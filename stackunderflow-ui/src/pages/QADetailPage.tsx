import { useParams, useNavigate } from 'react-router-dom'
import { useQuery } from '@tanstack/react-query'
import { IconArrowLeft } from '@tabler/icons-react'
import { getQADetail } from '../services/api'
import { getVoteCounts, getUserVotes } from '../services/social'
import LoadingSpinner from '../components/common/LoadingSpinner'
import EmptyState from '../components/common/EmptyState'
import QuestionCard from '../components/qa/QuestionCard'
import AnswerCard from '../components/qa/AnswerCard'
import VoteButton from '../components/social/VoteButton'
import DiscussionThread from '../components/social/DiscussionThread'

export default function QADetailPage() {
  const { name, qaId } = useParams<{ name: string; qaId: string }>()
  const navigate = useNavigate()

  const { data: qa, isLoading, error } = useQuery({
    queryKey: ['qa-detail', qaId],
    queryFn: () => getQADetail(qaId!),
    enabled: !!qaId,
  })

  const { data: voteCounts } = useQuery({
    queryKey: ['vote-counts', 'qa', qaId],
    queryFn: () => getVoteCounts('qa', [qaId!]),
    enabled: !!qaId,
  })

  const { data: userVotes } = useQuery({
    queryKey: ['user-votes', 'qa', qaId],
    queryFn: () => getUserVotes('qa', [qaId!]),
    enabled: !!qaId,
  })

  if (isLoading) return <LoadingSpinner message="Loading Q&A..." />
  if (error) {
    return (
      <div className="max-w-4xl mx-auto p-6">
        <button
          onClick={() => navigate(`/project/${encodeURIComponent(name!)}?tab=qa`)}
          className="flex items-center gap-1.5 text-sm text-gray-400 hover:text-gray-200 mb-4"
        >
          <IconArrowLeft size={16} />
          Back to Q&A
        </button>
        <EmptyState
          title="Failed to load Q&A"
          description={error instanceof Error ? error.message : 'An unknown error occurred'}
        />
      </div>
    )
  }
  if (!qa) return <EmptyState title="Q&A not found" description="The requested Q&A pair could not be found." />

  const voteCount = qaId && voteCounts ? (voteCounts[qaId] ?? 0) : 0
  const hasVoted = qaId && userVotes ? (userVotes[qaId] ?? false) : false

  return (
    <div className="max-w-4xl mx-auto p-6 space-y-6">
      {/* Navigation */}
      <div className="flex items-center justify-between">
        <button
          onClick={() => navigate(`/project/${encodeURIComponent(name!)}?tab=qa`)}
          className="flex items-center gap-1.5 text-sm text-gray-400 hover:text-gray-200"
        >
          <IconArrowLeft size={16} />
          Back to Q&A
        </button>
      </div>

      {/* Metadata */}
      <div className="flex items-center gap-3 text-xs text-gray-500 flex-wrap">
        <span className="font-mono">Project: {qa.project}</span>
        <span className="text-gray-700">|</span>
        <span className="font-mono truncate">Session: {qa.session_id}</span>
        {qa.model && (
          <>
            <span className="text-gray-700">|</span>
            <span className="font-mono">Model: {qa.model}</span>
          </>
        )}
        {qa.tools_used.length > 0 && (
          <>
            <span className="text-gray-700">|</span>
            <span>Tools: {qa.tools_used.join(', ')}</span>
          </>
        )}
      </div>

      {/* Question */}
      <div>
        <h3 className="text-xs font-semibold text-blue-400 uppercase tracking-wide mb-2">Question</h3>
        <QuestionCard question={qa.question_text} tags={qa.tools_used} timestamp={qa.timestamp} model={qa.model} />
      </div>

      {/* Answer */}
      <div>
        <h3 className="text-xs font-semibold text-green-400 uppercase tracking-wide mb-2">Answer</h3>
        <AnswerCard answer={qa.answer_text} hasCode={qa.code_snippets.length > 0} codeLanguages={[]} />
      </div>

      {/* Vote */}
      <div className="flex items-center gap-2">
        <VoteButton targetType="qa" targetId={qa.id} initialCount={voteCount} initialVoted={hasVoted} />
        <span className="text-xs text-gray-500">Vote on this Q&A</span>
      </div>

      {/* Discussion Thread */}
      <DiscussionThread qaId={qa.id} qa={qa} />
    </div>
  )
}
