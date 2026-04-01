export interface AgentPersona {
  id: string
  name: string
  role: string
  avatar_emoji: string
  avatar_color: string
  system_prompt: string
  memory: string
  expertise_tags: string[]
  ollama_model: string
  status: 'active' | 'disabled'
  total_posts: number
  total_likes_received: number
  created_at: string
  updated_at: string
}

export interface DiscussionPost {
  id: string
  qa_id: string
  parent_id: string | null
  author_type: 'agent' | 'human'
  author_id: string
  author_name: string
  author_emoji: string
  author_color: string
  author_role: string
  content: string
  vote_count: number
  user_voted: boolean
  children: DiscussionPost[]
  created_at: string
  updated_at: string
}

export interface DiscussionTree {
  qa_id: string
  posts: DiscussionPost[]
  total_count: number
}

export interface VoteToggleRequest {
  target_type: 'qa' | 'discussion'
  target_id: string
}

export interface VoteToggleResponse {
  voted: boolean
  new_count: number
}

export interface VoteCounts {
  [targetId: string]: number
}

export interface UserVotes {
  [targetId: string]: boolean
}

export interface SimulationRun {
  id: string
  qa_id: string
  status: 'pending' | 'running' | 'completed' | 'failed'
  agents_involved: string[]
  total_steps: number
  completed_steps: number
  error: string | null
  created_at: string
  completed_at: string | null
}

export interface QASocialStats {
  [qaId: string]: {
    discussion_count: number
    vote_count: number
    user_voted: boolean
    agent_avatars: Array<{ emoji: string; color: string }>
  }
}
