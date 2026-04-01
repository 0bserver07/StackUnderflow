import { useEffect, useRef } from 'react'
import { useQuery } from '@tanstack/react-query'
import { getSimulationStatus } from '../../services/social'
import AgentAvatar from './AgentAvatar'

interface SimulationProgressProps {
  runId: string
  onComplete: () => void
}

export default function SimulationProgress({ runId, onComplete }: SimulationProgressProps) {
  const calledComplete = useRef(false)

  const { data: run } = useQuery({
    queryKey: ['simulation-status', runId],
    queryFn: () => getSimulationStatus(runId),
    refetchInterval: (query) => {
      const status = query.state.data?.status
      if (status === 'completed' || status === 'failed') return false
      return 2000
    },
  })

  useEffect(() => {
    if (run && (run.status === 'completed' || run.status === 'failed') && !calledComplete.current) {
      calledComplete.current = true
      onComplete()
    }
  }, [run, onComplete])

  if (!run) return null

  const progress = run.total_steps > 0 ? (run.completed_steps / run.total_steps) * 100 : 0

  return (
    <div className="border border-gray-700 rounded-lg p-4 bg-gray-900/50">
      {/* Agent avatars */}
      <div className="flex items-center gap-2 mb-3">
        <div className={`flex -space-x-2 ${run.status === 'running' || run.status === 'pending' ? 'animate-pulse' : ''}`}>
          {run.agents_involved.map((agentId, i) => (
            <AgentAvatar
              key={agentId}
              emoji="🤖"
              color={['#3b82f6', '#10b981', '#f59e0b', '#ef4444', '#8b5cf6'][i % 5] ?? '#3b82f6'}
              size="sm"
            />
          ))}
        </div>
        <span className="text-sm text-gray-400 ml-2">
          {run.agents_involved.length} agent{run.agents_involved.length !== 1 ? 's' : ''}
        </span>
      </div>

      {/* Progress bar */}
      <div className="w-full bg-gray-800 rounded-full h-2 mb-2">
        <div
          className="bg-blue-500 h-2 rounded-full transition-all duration-300"
          style={{ width: `${progress}%` }}
        />
      </div>

      {/* Status text */}
      <p className="text-xs text-gray-400">
        {run.status === 'pending' && 'Preparing agents...'}
        {run.status === 'running' && `Agents are thinking... (${run.completed_steps}/${run.total_steps})`}
        {run.status === 'completed' && 'Discussion complete'}
        {run.status === 'failed' && `Failed: ${run.error || 'Unknown error'}`}
      </p>
    </div>
  )
}
