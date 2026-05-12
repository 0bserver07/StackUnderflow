// ---------------------------------------------------------------------------
// Analytics expansion re-exports (see ./analytics.ts)
// ---------------------------------------------------------------------------

export type {
  SessionCost,
  CommandCost,
  ToolCost,
  TokenComposition,
  OutlierCommand,
  Outliers,
  RetrySignal,
  SessionEfficiency,
  ErrorCost,
  TrendMetrics,
  Trends,
} from './analytics'

import type {
  SessionCost,
  CommandCost,
  ToolCost,
  TokenComposition,
  Outliers,
  RetrySignal,
  SessionEfficiency,
  ErrorCost,
  Trends,
} from './analytics'

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
  // Multi-provider polish (spec.md §6 Step 5). The backend currently merges
  // provider-duplicates of one slug at the API layer, so this is optional —
  // a single string when one provider, or a comma-joined list (e.g.
  // `"claude,codex"`) when a slug appears under multiple providers. Renders
  // as the "unknown" chip when missing.
  provider?: string
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
  // Multi-provider polish (spec.md §6 Step 5). Source provider — `claude`,
  // `codex`, `cursor`, `cline`, etc. Optional during rollout: when missing,
  // ProviderChip renders as gray "unknown".
  provider?: string
  // Multi-provider polish (spec.md §3.1). Cursor's vscdb does not surface
  // per-message tokens, so its cost is derived from estimated token counts.
  // Future Cline / opencode adapters may set this too. Optional until the
  // backend (sessions route + aggregator) propagates the flag.
  cost_source?: 'estimated' | 'actual'
}

/**
 * Top-level currency block stamped onto every monetary API response since
 * v0.6.0 (multi-currency PR). The UI reads `code` for labels, `symbol` for
 * inline rendering via `formatCost(usd, currency)`, and `rate_from_usd`
 * for any client-side conversions (rare — backend pre-converts).
 *
 * `warning` is non-null when the backend had to fall back past the live
 * Frankfurter feed (e.g. 403, offline, or rate-limited). The dashboard
 * surfaces it as a banner so non-USD users aren't shown numbers labelled
 * with the wrong currency without explanation. It self-clears the next
 * time a successful fetch produces a warning-free payload.
 */
export interface CurrencyInfo {
  code: string
  symbol: string
  rate_from_usd: number
  warning?: string | null
}

/**
 * `/api/jsonl-files` response shape since v0.6.0. The bare list of files
 * was wrapped to also expose the active currency for the dashboard's
 * cost-per-session column.
 */
export interface JsonlFilesResponse {
  files: JsonlFile[]
  currency: CurrencyInfo
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
  // Stamped at the top level of /api/dashboard-data since v0.6.0. The UI
  // reads this to render cost figures in the active currency without
  // re-fetching settings on every component.
  currency?: CurrencyInfo
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
  // Analytics expansion — optional during rollout (spec §1)
  session_costs?: SessionCost[]
  command_costs?: CommandCost[]
  tool_costs?: Record<string, ToolCost>
  token_composition?: TokenComposition
  outliers?: Outliers
  retry_signals?: RetrySignal[]
  session_efficiency?: SessionEfficiency[]
  error_cost?: ErrorCost
  trends?: Trends
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
  command_details: CommandDetail[]
}

// §D2: shape of /api/tool-distribution — split off /api/dashboard-data so the
// Overview chart can lazy-fetch the bucket map post-mount.
export interface ToolDistributionResponse {
  tool_count_distribution: Record<string, number>
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

export type ResolutionStatus = 'resolved' | 'looped' | 'abandoned' | 'open'

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
  resolution_status: ResolutionStatus
  loop_count: number
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

// ---------------------------------------------------------------------------
// v0.6.0 follow-up surfaces — Compare, Yield, Plan, Optimize, Context-budget.
// These shapes mirror the Python dataclasses returned by the matching routes
// (`stackunderflow/routes/{compare,yield_route,plan,optimize,context_budget}.py`).
// Keep them in lockstep with the route response bodies; missing/optional
// fields are noted inline.
// ---------------------------------------------------------------------------

/**
 * Per-model row from `GET /api/compare`.
 * Source: `services/compare.py::ModelStats` (frozen dataclass, all fields
 * required). `total_cost` is the column the API sorts by, descending.
 */
export interface ModelStats {
  model: string
  provider: string
  sessions: number
  calls: number
  one_shot_pct: number
  retry_rate: number
  cache_hit_rate: number
  cost_per_call: number
  cost_per_session: number
  total_cost: number
  total_tokens: number
}

export interface CompareResponse {
  period: string
  models: ModelStats[]
  generated: number
}

/**
 * One row of the per-provider cost rollup served by
 * `/api/cost-data/by-provider`. Powers the Cost tab's
 * `CostByProviderCard`.
 *
 * `cost_usd` is already converted into the active currency by the route
 * (despite the name — kept for parity with the rest of the codebase). The
 * `currency` block on the parent response carries the symbol/code.
 */
export interface CostByProviderRow {
  provider: string
  cost_usd: number
  message_count: number
  session_count: number
}

export interface CostByProviderResponse {
  period: string
  rows: CostByProviderRow[]
  currency: CurrencyInfo
}

/**
 * One session's yield classification — `services/yield_tracker.py::YieldEntry`
 * with `cost_usd` already converted to the active currency by the route.
 */
export type YieldClassification = 'productive' | 'reverted' | 'abandoned' | 'no_repo'

export interface YieldEntry {
  session_id: string
  project_slug: string
  cwd: string
  started_at: string
  cost_usd: number
  classification: YieldClassification
  follow_commit_sha: string | null
  follow_commit_msg: string | null
  follow_commit_age_hours: number | null
}

export interface YieldSummary {
  productive: number
  reverted: number
  abandoned: number
  no_repo: number
  total: number
  productive_cost: number
  reverted_cost: number
  abandoned_cost: number
  no_repo_cost: number
  total_cost: number
}

export interface YieldResponse {
  period: string
  summary: YieldSummary
  entries: YieldEntry[]
  currency: CurrencyInfo
  warning: string
}

/**
 * Plan + usage payload from `GET /api/plan`. Both fields are nullable —
 * when no plan is configured, the route returns `{plan: null, usage: null}`
 * and the UI should hide the widget rather than render an empty card.
 */
export interface Plan {
  name: string
  monthly_usd: number
  reset_day: number
}

export interface PlanUsage {
  used: number
  budget: number
  remaining: number
  pct: number
  projected: number
  status: 'ok' | 'warn' | 'over'
  period_start: string
  period_end: string
  days_so_far: number
  days_in_period: number
}

export interface PlanResponse {
  plan: Plan | null
  usage: PlanUsage | null
}

/**
 * One structural finding from `GET /api/optimize`. Mirrors
 * `reports/optimize.py::Finding`. The route also returns a legacy `waste`
 * list (looped Q&A); the new dashboard panel only uses `patterns`.
 */
export type FindingSeverity = 'high' | 'medium' | 'low'

export interface Finding {
  pattern_id: string
  severity: FindingSeverity
  title: string
  description: string
  affected_count: number
  suggested_fix: string
  estimated_waste_tokens: number | null
  details: Record<string, unknown>
}

export interface OptimizeResponse {
  scope: string
  waste: unknown[]
  patterns: Finding[]
}

/**
 * Per-session context-budget payload from `GET /api/context-budget`. Source:
 * `services/context_budget.py::ContextBudget`. `slices` is the per-source
 * breakdown the panel renders.
 */
export interface ContextSlice {
  name: string
  tokens: number
  source_path: string | null
}

export interface ContextBudget {
  total_tokens: number
  slices: ContextSlice[]
  cost_per_session_usd: number
  estimated_monthly_cost_usd: number
  heuristic: string
}

// ---------------------------------------------------------------------------
// Wave 4F — ETL pipeline status (powering the dashboard header badge + the
// Settings backfill section). Mirrors the response body of `GET /api/etl/status`
// shipped by Wave 4C.
//
// Health states:
//   - live    → caught up; dashboard data reflects the latest events
//   - syncing → watcher is actively chewing through a backlog
//   - stale   → no refresh in a while (likely watcher down or no events)
//   - error   → ETL is in a failure state; user must investigate
//
// The badge polls this endpoint every 10s. When the route is missing (Wave 4C
// hasn't merged), the fetcher surfaces a 404 and the badge renders a disabled
// "ETL pipeline not ready" state rather than crashing the dashboard.
// ---------------------------------------------------------------------------

export type EtlHealth = 'live' | 'syncing' | 'stale' | 'error'

export interface EtlWatcherStatus {
  enabled: boolean
  running: boolean
  last_refresh_ts: string | null
  seconds_since_refresh: number | null
  events_in_last_cycle: number
}

export interface EtlMartStatus {
  watermark: number
  row_count: number
  last_refresh_ts: string | null
}

export interface EtlEventsStatus {
  total: number
  max_id: number
  by_provider: Record<string, number>
  by_cost_source: Record<string, number>
}

// Live backfill job — populated by the route module's process-local
// slot from `stackunderflow.etl.backfill_jobs` while a POST /api/etl/backfill
// run is in flight, and `null` otherwise.
export interface EtlBackfillJob {
  job_id: string
  started_at: string
  force: boolean
  status: string
}

// Most-recently completed backfill — populated by the same module's
// last-job slot. Retained for `LAST_JOB_TTL_SECONDS` (30s) so the
// dashboard has a chance to render the outcome before the slot
// garbage-collects on read. ``error`` is populated only when
// ``status === "failed"``; success completions omit it.
export type EtlBackfillFinalStatus = 'complete' | 'failed'

export interface EtlCompletedJob {
  job_id: string
  started_at: string
  completed_at: string
  force: boolean
  status: EtlBackfillFinalStatus
  error?: string | null
}

export interface EtlStatusResponse {
  watcher: EtlWatcherStatus
  marts: Record<string, EtlMartStatus>
  events: EtlEventsStatus
  lag_seconds: number
  health: EtlHealth
  current_job: EtlBackfillJob | null
  last_job: EtlCompletedJob | null
}

// 202 Accepted body returned by POST /api/etl/backfill once the
// background task has been queued. Poll /api/etl/status to track
// progress via `current_job`.
export interface EtlBackfillResponse {
  job_id: string
  started_at: string
}

// ---------------------------------------------------------------------------
// Agent-teams — Claude Code parallel-agent topology surface.
// Mirrors the response bodies of /api/agent-teams/*. Since the v013
// migration the team graph is materialised at ingest time, so the member
// objects also carry `spawn_prompt` / `agent_role` and the graph carries
// the team `description` (all null on stores whose ~/.claude/teams/
// artefacts haven't been ingested — the backend falls back to a
// heuristic there). See .notes/specs/09-multi-agent-fs-recognition.md.
// ---------------------------------------------------------------------------

export interface AgentTeamSummary {
  session_id: string
  project_slug: string
  project_display_name: string
  team_name: string | null
  first_ts: string | null
  last_ts: string | null
  agent_count: number
  sub_agent_message_count: number
  lead_message_count: number
  description?: string | null
}

export interface AgentTeamMember {
  session_id: string
  agent_id: string | null
  agent_name: string | null
  is_lead: boolean
  parent_session_id: string | null
  message_count: number
  first_ts: string | null
  last_ts: string | null
  first_user_prompt: string | null
  model: string | null
  cost_usd: number
  // v013 materialised extras (null/absent on un-materialised stores).
  spawn_prompt?: string | null
  agent_role?: 'lead' | 'subagent' | null
}

export interface AgentTeamGraph {
  session_id: string
  team_name: string | null
  description?: string | null
  project_slug: string
  project_display_name: string
  lead: AgentTeamMember
  agents: AgentTeamMember[]
}

export interface AgentTeamListResponse {
  teams: AgentTeamSummary[]
}

export interface AgentTeamTranscriptResponse {
  session_id: string
  agent_session_id: string
  message_count: number
  messages: Array<{
    id: number
    seq: number
    timestamp: string
    role: string
    model: string | null
    content_text: string
    is_sidechain: boolean
    uuid: string | null
    parent_uuid: string | null
    [k: string]: unknown
  }>
}

// ---------------------------------------------------------------------------
// Playback — per-session (and per-project) tool-call timeline. One row per
// tool call; the dashboard "Playback" tab steps through them with a scrubber.
// See .notes/specs/10-playback-timeline.md (v1: event stream only).
// ---------------------------------------------------------------------------

export interface PlaybackEvent {
  /** 0-based index of this tool call in the full (unfiltered) stream. */
  seq: number
  /** ISO-8601 UTC timestamp of the message that issued the call. */
  ts: string
  /** `messages.id` of the issuing assistant message. */
  message_id: number
  tool_name: string
  /** One-line label, e.g. "Edit routes/cost.py", "Bash: pytest". */
  summary: string
  /** File path the tool operated on, when applicable. */
  target_path: string | null
  /** Payload size in bytes (result text, or written content for writes). */
  byte_count: number | null
  /** Outcome — `false` on a recorded failure, `true` on success, `null` unknown. */
  success: boolean | null
  /** Wall-clock from call to result, in ms, when both timestamps are present. */
  duration_ms: number | null
  /** Up-to-200-char excerpt of the input/output (empty when not requested). */
  payload_excerpt: string
  /** Owning session id (redundant per-session; meaningful for project timelines). */
  session_id: string
}

export interface PlaybackResponse {
  session_id: string
  events: PlaybackEvent[]
  total: number
  /** `true` when `limit` capped the stream — more events exist. */
  truncated: boolean
}

export interface ProjectTimelineResponse {
  project_slug: string
  events: PlaybackEvent[]
  total: number
  truncated: boolean
}

