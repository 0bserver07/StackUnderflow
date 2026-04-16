export interface Project {
  dir_name: string
  display_name: string
  log_path: string
  last_modified: number
  first_seen: number
  total_size_mb: number
  file_count: number
  in_cache: boolean
  url_slug: string
  stats?: ProjectStats | null
}

export interface ProjectStats {
  total_input_tokens: number
  total_output_tokens: number
  total_cache_read: number
  total_cache_write: number
  total_commands: number
  avg_tokens_per_command: number
  avg_steps_per_command: number
  compact_summary_count: number
  first_message_date: string | null
  last_message_date: string | null
  total_cost: number
}

export interface ProjectsResponse {
  projects: Project[]
  total_count: number
  has_more: boolean
  cache_status: {
    cached_count: number
    total_projects: number
  }
}

export interface SetProjectResponse {
  status: string
  project_path: string
  log_path: string
  log_dir_name: string
  message: string
}

export interface JsonlFile {
  name: string
  created: number
  modified: number
  size: number
  messages?: number
  user_messages?: number
  assistant_messages?: number
  input_tokens?: number
  output_tokens?: number
  model?: string | null
  title?: string | null
  tool_calls?: number
  is_subagent?: boolean
  estimated_cost?: number
}

export interface JsonlContentResponse {
  lines: Record<string, unknown>[]
  total_lines: number
  user_count: number
  assistant_count: number
  metadata: {
    session_id: string
    file_size: number
    created: number
    modified: number
    first_timestamp: string | null
    last_timestamp: string | null
    duration_minutes: number | null
    cwd: string
  }
}

// ---------------------------------------------------------------------------
// Message (from /api/messages and messages_page in /api/dashboard-data)
// ---------------------------------------------------------------------------

export interface MessageTokens {
  input: number
  output: number
  cache_creation: number
  cache_read: number
}

export interface MessageTool {
  name: string
  input: unknown
  id: string
}

export interface Message {
  session_id: string
  type: string // "user", "assistant", "tool_use", "tool_result", etc.
  timestamp: string
  model: string | null
  content: string
  tools: MessageTool[]
  tokens: MessageTokens | null
  cwd: string | null
  uuid: string
  parent_uuid: string | null
  is_sidechain: boolean
  has_tool_result: boolean
  error: boolean
  message_id: string
  _raw_data: unknown
}

// ---------------------------------------------------------------------------
// Paginated messages returned inside DashboardData
// ---------------------------------------------------------------------------

export interface MessagesPage {
  messages: Message[]
  total: number
  page: number
  per_page: number
  total_pages: number
  start_index: number
  end_index: number
}

// ---------------------------------------------------------------------------
// Dashboard top-level shape (GET /api/dashboard-data)
// ---------------------------------------------------------------------------

export interface DashboardData {
  statistics: DashboardStats
  messages_page: MessagesPage
  message_count: number
  is_reindexing: boolean
  config: {
    messages_initial_load: number
    max_date_range_days: number
  }
}

// ---------------------------------------------------------------------------
// statistics sub-objects
// ---------------------------------------------------------------------------

export interface DashboardStats {
  overview: OverviewStats
  tools: ToolStats
  sessions: SessionStats
  daily_stats: Record<string, DailyData>
  hourly_pattern: HourlyPattern
  errors: ErrorStats
  models: Record<string, ModelData>
  user_interactions: UserInteractionStats
  cache: CacheStats
}

export interface OverviewStats {
  project_name: string
  log_dir_name: string
  project_path: string
  total_messages: number
  date_range: {
    start: string
    end: string
  }
  sessions: number
  message_types: Record<string, number>
  total_tokens: {
    input: number
    output: number
    cache_creation: number
    cache_read: number
  }
  total_cost: number
}

export interface ToolStats {
  usage_counts: Record<string, number>
  error_counts: Record<string, number>
  error_rates: Record<string, number>
}

export interface SessionStats {
  count: number
  average_duration_seconds: number
  average_messages: number
  sessions_with_errors: number
}

export interface DailyModelCost {
  input_cost: number
  output_cost: number
  cache_creation_cost: number
  cache_read_cost: number
  total_cost: number
}

export interface DailyData {
  messages: number
  sessions: number
  tokens: {
    input: number
    output: number
    cache_creation: number
    cache_read: number
  }
  cost: {
    total: number
    by_model: Record<string, DailyModelCost>
  }
  user_commands: number
  interrupted_commands: number
  interruption_rate: number
  errors: number
  assistant_messages: number
  error_rate: number
}

export interface HourlyPattern {
  messages: Record<string, number>
  tokens: Record<string, {
    input: number
    output: number
    cache_creation: number
    cache_read: number
  }>
}

export interface ErrorStats {
  total: number
  rate: number
  by_type: Record<string, number>
  by_category: Record<string, number>
  error_details: unknown[]
  assistant_details: unknown[]
}

export interface ModelData {
  count: number
  input_tokens: number
  output_tokens: number
  cache_creation_tokens: number
  cache_read_tokens: number
}

export interface CommandDetail {
  user_message: string
  user_message_truncated: string
  timestamp: string
  session_id: string
  tools_used: number
  tool_names: string[]
  has_tools: boolean
  assistant_steps: number
  model: string
  is_interruption: boolean
  followed_by_interruption: boolean
  estimated_tokens: number
  search_tools_used: number
}

export interface UserInteractionStats {
  real_user_messages: number
  user_commands_analyzed: number
  commands_requiring_tools: number
  commands_without_tools: number
  percentage_requiring_tools: number
  total_tools_used: number
  total_search_tools: number
  search_tool_percentage: number
  total_assistant_steps: number
  avg_tools_per_command: number
  avg_tools_when_used: number
  avg_steps_per_command: number
  avg_tokens_per_command: number
  percentage_steps_with_tools: number
  tool_count_distribution: Record<string, number>
  command_details: CommandDetail[]
}

export interface CacheStats {
  total_created: number
  total_read: number
  messages_with_cache_read: number
  messages_with_cache_created: number
  assistant_messages: number
  hit_rate: number
  efficiency: number
  tokens_saved: number
  cost_saved_base_units: number
  break_even_achieved: boolean
}

// ---------------------------------------------------------------------------
// Q&A types
// ---------------------------------------------------------------------------

export interface QAPair {
  id: string
  session_id: string
  project: string
  question_text: string
  answer_text: string
  question_snippet: string | null
  answer_snippet: string | null
  code_snippets: string[]
  tools_used: string[]
  timestamp: string
  model?: string
  num_attempts: number
  // These may appear on detail responses
  tags?: string[]
  has_code?: boolean
  code_languages?: string[]
  complexity_score?: number
}

export interface QAListResponse {
  results: QAPair[]
  total: number
  page: number
  per_page: number
  total_pages: number
}

export interface QADetailResponse {
  id: string
  session_id: string
  project: string
  question_text: string
  answer_text: string
  code_snippets: string[]
  tools_used: string[]
  timestamp: string
  model?: string
  num_attempts: number
  created_at?: string
}

// ---------------------------------------------------------------------------
// Search types
// ---------------------------------------------------------------------------

export interface SearchResult {
  id: number
  session_id: string
  project: string
  role: string
  content: string
  snippet: string
  timestamp: string
  model?: string
  tokens_input: number
  tokens_output: number
  relevance: number
}

export interface SearchResponse {
  results: SearchResult[]
  total: number
  page: number
  per_page: number
  total_pages: number
  query: string
}

// ---------------------------------------------------------------------------
// Tag types
// ---------------------------------------------------------------------------

export interface Tag {
  name: string
  count: number
  category: string
  color: string
}

export interface TagCloudResponse {
  tags: Tag[]
  total_sessions: number
}

export interface TagBrowseResponse {
  tag: string
  sessions: TagBrowseSession[]
  count: number
}

export interface TagBrowseSession {
  session_id: string
  source: string[]
}

export interface SessionTags {
  session_id: string
  auto_tags: string[]
  manual_tags: string[]
  all_tags: string[]
}

// ---------------------------------------------------------------------------
// Bookmark types
// ---------------------------------------------------------------------------

export interface Bookmark {
  id: string
  session_id: string
  title: string
  message_index?: number
  notes: string
  tags: string[]
  created_at: string
}

export interface BookmarkListResponse {
  bookmarks: Bookmark[]
}

// ---------------------------------------------------------------------------
// Related sessions types
// ---------------------------------------------------------------------------

export interface RelatedSession {
  session_id: string
  project: string
  score: number
  shared_tags: string[]
  preview: string
  timestamp: string
}

export interface RelatedResponse {
  session_id: string
  related: RelatedSession[]
  count: number
}

// ---------------------------------------------------------------------------
// Pricing types
// ---------------------------------------------------------------------------

export interface PricingData {
  pricing: Record<string, ModelPricing>
  source: string
  timestamp: string
  is_stale: boolean
}

export interface ModelPricing {
  input_cost_per_token: number
  output_cost_per_token: number
  cache_read_cost_per_token?: number
  cache_creation_cost_per_token?: number
}

