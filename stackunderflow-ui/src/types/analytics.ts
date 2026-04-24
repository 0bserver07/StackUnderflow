// ---------------------------------------------------------------------------
// Analytics expansion types — mirror Python TypedDicts in
// stackunderflow/stats/aggregator.py. See docs/specs/analytics-expansion.md §1.
// All ISO-8601 timestamps are typed as `string`.
// ---------------------------------------------------------------------------

export interface SessionCost {
  session_id: string
  started_at: string
  ended_at: string
  duration_s: number
  cost: number
  tokens: Record<string, number>
  messages: number
  commands: number
  errors: number
  first_prompt_preview: string
  models_used: string[]
}

export interface CommandCost {
  interaction_id: string
  session_id: string
  timestamp: string
  prompt_preview: string
  cost: number
  tokens: Record<string, number>
  tools_used: number
  steps: number
  models_used: string[]
  had_error: boolean
}

export interface ToolCost {
  calls: number
  input_tokens: number
  output_tokens: number
  cache_read_tokens: number
  cache_creation_tokens: number
  cost: number
}

export interface TokenComposition {
  daily: Record<string, Record<string, number>>
  totals: Record<string, number>
  per_session: Record<string, Record<string, number>>
}

export interface OutlierCommand {
  interaction_id: string
  session_id: string
  timestamp: string
  prompt_preview: string
  tool_count: number
  step_count: number
  cost: number
}

export interface Outliers {
  high_tool_commands: OutlierCommand[]
  high_step_commands: OutlierCommand[]
}

export interface RetrySignal {
  interaction_id: string
  session_id: string
  timestamp: string
  tool: string
  consecutive_failures: number
  total_invocations: number
  estimated_wasted_tokens: number
  estimated_wasted_cost: number
}

export interface SessionEfficiency {
  session_id: string
  search_ratio: number
  edit_ratio: number
  read_ratio: number
  bash_ratio: number
  idle_gap_total_s: number
  idle_gap_max_s: number
  classification: string
}

export interface ErrorCost {
  total_errors: number
  estimated_retry_tokens: number
  estimated_retry_cost: number
  errors_by_tool: Record<string, number>
  top_error_commands: OutlierCommand[]
}

export interface TrendMetrics {
  cost_per_command: number
  errors_per_command: number
  tools_per_command: number
  tokens_per_command: number
  commands: number
  cost: number
}

export interface Trends {
  current_week: TrendMetrics
  prior_week: TrendMetrics
  delta_pct: TrendMetrics
}
