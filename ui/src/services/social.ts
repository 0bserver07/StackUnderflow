import type {
  AgentPersona,
  DiscussionTree,
  DiscussionPost,
  VoteToggleRequest,
  VoteToggleResponse,
  VoteCounts,
  UserVotes,
  SimulationRun,
  QASocialStats,
} from '../types/social'

const BASE = '/api'

async function fetchJson<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await fetch(url, init)
  if (!res.ok) {
    const text = await res.text().catch(() => '')
    throw new Error(`${res.status} ${res.statusText}${text ? `: ${text}` : ''}`)
  }
  return res.json()
}

// Agents
export async function getAgents(): Promise<AgentPersona[]> {
  const data = await fetchJson<{ agents: AgentPersona[] }>(`${BASE}/agents`)
  return data.agents
}

export async function getAgent(id: string): Promise<AgentPersona> {
  return fetchJson(`${BASE}/agents/${encodeURIComponent(id)}`)
}

export async function createAgent(data: Partial<AgentPersona>): Promise<AgentPersona> {
  return fetchJson(`${BASE}/agents`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(data),
  })
}

export async function updateAgent(id: string, data: Partial<AgentPersona>): Promise<AgentPersona> {
  return fetchJson(`${BASE}/agents/${encodeURIComponent(id)}`, {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(data),
  })
}

// Discussions
export async function getDiscussionTree(qaId: string): Promise<DiscussionTree> {
  return fetchJson(`${BASE}/discussions/${encodeURIComponent(qaId)}`)
}

export async function postDiscussion(
  qaId: string,
  content: string,
  parentId?: string | null
): Promise<DiscussionPost> {
  return fetchJson(`${BASE}/discussions/${encodeURIComponent(qaId)}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      content,
      parent_id: parentId ?? null,
      author_type: 'human',
      author_id: 'human',
    }),
  })
}

export async function getDiscussionCounts(qaIds: string[]): Promise<Record<string, number>> {
  const params = new URLSearchParams({ qa_ids: qaIds.join(',') })
  const data = await fetchJson<{ counts: Record<string, number> }>(`${BASE}/discussions/counts?${params}`)
  return data.counts
}

// Votes
export async function toggleVote(req: VoteToggleRequest): Promise<VoteToggleResponse> {
  return fetchJson(`${BASE}/votes/toggle`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(req),
  })
}

export async function getVoteCounts(targetType: string, targetIds: string[]): Promise<VoteCounts> {
  const params = new URLSearchParams({
    target_type: targetType,
    target_ids: targetIds.join(','),
  })
  const data = await fetchJson<{ counts: VoteCounts }>(`${BASE}/votes/counts?${params}`)
  return data.counts
}

export async function getUserVotes(targetType: string, targetIds: string[]): Promise<UserVotes> {
  const params = new URLSearchParams({
    target_type: targetType,
    target_ids: targetIds.join(','),
  })
  const data = await fetchJson<{ votes: UserVotes }>(`${BASE}/votes/user?${params}`)
  return data.votes
}

// Simulation
export async function triggerDiscussion(
  qaId: string,
  agentIds?: string[]
): Promise<SimulationRun> {
  return fetchJson(`${BASE}/simulate/discuss/${encodeURIComponent(qaId)}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ agent_ids: agentIds }),
  })
}

export async function getSimulationStatus(runId: string): Promise<SimulationRun> {
  return fetchJson(`${BASE}/simulate/status/${encodeURIComponent(runId)}`)
}

export async function cancelSimulation(runId: string): Promise<void> {
  await fetchJson(`${BASE}/simulate/cancel/${encodeURIComponent(runId)}`, {
    method: 'POST',
  })
}

// Social stats for feed
export async function getQASocialStats(qaIds: string[]): Promise<QASocialStats> {
  const params = new URLSearchParams({ qa_ids: qaIds.join(',') })
  const data = await fetchJson<{ stats: QASocialStats }>(`${BASE}/discussions/social-stats?${params}`)
  return data.stats
}
