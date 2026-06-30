import type {
  ProjectsResponse,
  SetProjectResponse,
  JsonlFilesResponse,
  JsonlContentResponse,
  DashboardData,
  MessagesPage,
  QAListResponse,
  SearchResponse,
  TagCloudResponse,
  TagBrowseResponse,
  BookmarkListResponse,
  Bookmark,
  PricingData,
  CurrencyInfo,
  CompareResponse,
  CostByProviderResponse,
  YieldResponse,
  PlanResponse,
  BudgetResponse,
  BudgetUpdate,
  WhatIfResponse,
  OptimizeResponse,
  ContextBudget,
  EtlStatusResponse,
  EtlBackfillResponse,
  EtlHealth,
  AgentTeamListResponse,
  AgentTeamGraph,
  AgentTeamTranscriptResponse,
  PlaybackResponse,
  ProjectTimelineResponse,
  PlaybackFsSnapshotResponse,
} from '../types/api'

const BASE = '/api'

async function fetchJson<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await fetch(url, init)
  if (!res.ok) {
    const text = await res.text().catch(() => '')
    throw new Error(`${res.status} ${res.statusText}${text ? `: ${text}` : ''}`)
  }
  return res.json()
}

// ---------------------------------------------------------------------------
// Filter helpers — every dashboard route that gained a `?provider=` and/or
// `?model=` query param shares the same encoding contract: lowercased on
// emit, repeated for multi-select. ``buildFilterParams`` centralises that
// so each call site doesn't reimplement it.
// ---------------------------------------------------------------------------

export interface FilterParams {
  providers?: string[]
  models?: string[]
}

function buildFilterParams(params: URLSearchParams, filters?: FilterParams): URLSearchParams {
  if (filters?.providers) {
    for (const p of filters.providers) {
      if (p && p.trim()) params.append('provider', p.toLowerCase().trim())
    }
  }
  if (filters?.models) {
    for (const m of filters.models) {
      if (m && m.trim()) params.append('model', m.toLowerCase().trim())
    }
  }
  return params
}

// Projects
//
// `/api/projects` is server-paginated (audit #12): omitting `limit` returns a
// large default cap, but heavy `include_stats=true` callers should walk pages
// via `limit`/`offset` so the hundreds-of-projects payload isn't pulled at
// once. The response echoes the resolved `limit`/`offset` plus `total_count` /
// `has_more` so callers (see Overview's `useInfiniteQuery`) can derive the next
// page without re-deriving the clamp rules.
export interface ProjectsPageQuery {
  limit?: number
  offset?: number
}

export async function getProjects(
  includeStats = false,
  filters?: FilterParams,
  page?: ProjectsPageQuery,
): Promise<ProjectsResponse> {
  const params = new URLSearchParams({ include_stats: String(includeStats) })
  if (typeof page?.limit === 'number') params.set('limit', String(page.limit))
  if (typeof page?.offset === 'number') params.set('offset', String(page.offset))
  buildFilterParams(params, filters)
  return fetchJson(`${BASE}/projects?${params}`)
}

// ---------------------------------------------------------------------------
// Provider catalogue — drives the dashboard's FilterBar chip row. Returns
// every provider currently active in the store with project + session
// counts so the UI can render counts inline next to each chip.
// ---------------------------------------------------------------------------

export interface ProviderInfo {
  provider: string
  project_count: number
  session_count: number
}

export interface ProvidersResponse {
  providers: ProviderInfo[]
  error?: string
}

export async function getProviders(): Promise<ProvidersResponse> {
  return fetchJson(`${BASE}/providers`)
}

export async function setProjectByDir(dirName: string): Promise<SetProjectResponse> {
  return fetchJson(`${BASE}/project-by-dir`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ dir_name: dirName }),
  })
}

// Dashboard
export async function getDashboardData(
  timezoneOffset = 0,
  filters?: FilterParams,
): Promise<DashboardData> {
  const params = new URLSearchParams({ timezone_offset: String(timezoneOffset) })
  buildFilterParams(params, filters)
  return fetchJson(`${BASE}/dashboard-data?${params}`)
}

// Messages
//
// `/api/messages` returns a paginated envelope (see backend
// `MESSAGES_DEFAULT_PER_PAGE` = 100, max 500). Earlier releases returned
// the full message list unbounded; on a 26K-message project that ballooned
// the response to ~37 MB and OOMed the Messages tab. Callers now walk
// pages explicitly via the `page` / `perPage` knobs.
export interface MessagesQuery {
  page?: number
  perPage?: number
}

export async function getMessages(
  query?: MessagesQuery,
  filters?: FilterParams,
): Promise<MessagesPage> {
  const params = new URLSearchParams()
  if (typeof query?.page === 'number') params.set('page', String(query.page))
  if (typeof query?.perPage === 'number') params.set('per_page', String(query.perPage))
  buildFilterParams(params, filters)
  const qs = params.toString()
  return fetchJson(`${BASE}/messages${qs ? `?${qs}` : ''}`)
}

// JSONL files
//
// Returns the full {files, currency} envelope since v0.6.0 (multi-currency
// PR wrapped the previously bare list). Callers that only need the file
// metadata can destructure `.files`; the currency block is also propagated
// upward so consumers can render cost columns in the active currency.
export async function getJsonlFiles(
  project?: string,
  filters?: FilterParams,
): Promise<JsonlFilesResponse> {
  const params = new URLSearchParams()
  if (project) params.set('project', project)
  buildFilterParams(params, filters)
  const qs = params.toString()
  return fetchJson(`${BASE}/jsonl-files${qs ? `?${qs}` : ''}`)
}

export async function getJsonlContent(file: string, project?: string): Promise<JsonlContentResponse> {
  const params = new URLSearchParams({ file })
  if (project) params.set('project', project)
  return fetchJson(`${BASE}/jsonl-content?${params}`)
}

// Q&A
export async function getQAList(params: {
  project?: string
  date_from?: string
  date_to?: string
  search?: string
  resolution_status?: 'resolved' | 'looped' | 'abandoned' | 'open'
  page?: number
  per_page?: number
}): Promise<QAListResponse> {
  const searchParams = new URLSearchParams()
  if (params.project) searchParams.set('project', params.project)
  if (params.date_from) searchParams.set('date_from', params.date_from)
  if (params.date_to) searchParams.set('date_to', params.date_to)
  if (params.search) searchParams.set('search', params.search)
  if (params.resolution_status) searchParams.set('resolution_status', params.resolution_status)
  if (params.page) searchParams.set('page', String(params.page))
  if (params.per_page) searchParams.set('per_page', String(params.per_page))
  return fetchJson(`${BASE}/qa?${searchParams}`)
}

// Search
export async function searchMessages(params: {
  q: string
  project?: string
  date_from?: string
  date_to?: string
  model?: string
  role?: string
  page?: number
  per_page?: number
}): Promise<SearchResponse> {
  const searchParams = new URLSearchParams()
  searchParams.set('q', params.q)
  if (params.project) searchParams.set('project', params.project)
  if (params.date_from) searchParams.set('date_from', params.date_from)
  if (params.date_to) searchParams.set('date_to', params.date_to)
  if (params.model) searchParams.set('model', params.model)
  if (params.role) searchParams.set('role', params.role)
  if (params.page) searchParams.set('page', String(params.page))
  if (params.per_page) searchParams.set('per_page', String(params.per_page))
  return fetchJson(`${BASE}/search?${searchParams}`)
}

// Tags
export async function getTagCloud(): Promise<TagCloudResponse> {
  return fetchJson(`${BASE}/tags`)
}

export async function browseTag(tag: string): Promise<TagBrowseResponse> {
  return fetchJson(`${BASE}/tags/browse/${encodeURIComponent(tag)}`)
}

// Bookmarks
export async function getBookmarks(tag?: string, sortBy = 'created_at'): Promise<BookmarkListResponse> {
  const params = new URLSearchParams({ sort_by: sortBy })
  if (tag) params.set('tag', tag)
  return fetchJson(`${BASE}/bookmarks?${params}`)
}

export async function removeBookmark(bookmarkId: string): Promise<Bookmark | unknown> {
  return fetchJson(`${BASE}/bookmarks/${encodeURIComponent(bookmarkId)}`, {
    method: 'DELETE',
  })
}

export async function updateBookmark(bookmarkId: string, data: { title?: string; notes?: string; tags?: string[] }) {
  return fetchJson(`${BASE}/bookmarks/${encodeURIComponent(bookmarkId)}`, {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(data),
  })
}

// Refresh
export async function refreshData(timezoneOffset = 0) {
  return fetchJson(`${BASE}/refresh`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ timezone_offset: timezoneOffset }),
  })
}

// Global stats (cross-project overview)
export async function getGlobalStats(): Promise<Record<string, unknown>> {
  return fetchJson(`${BASE}/global-stats`)
}

// Per-day user-command counts — windows the Overview "Commands" KPI (#25).
// `daily` is oldest-day-first `{date: "YYYY-MM-DD", commands: N}`; the caller
// sums `commands` over the days inside its selected date range the same way it
// sums daily_token_usage / daily_costs. Cross-project when no `logPath` is
// passed (the global Overview), project-scoped otherwise.
export interface CommandDailyResponse {
  daily: Array<{ date: string; commands: number }>
  total: number
  scope: 'global' | 'project'
}

export async function getCommandsDaily(logPath?: string): Promise<CommandDailyResponse> {
  const params = new URLSearchParams()
  if (logPath) params.set('log_path', logPath)
  const qs = params.toString()
  return fetchJson(`${BASE}/commands/daily${qs ? `?${qs}` : ''}`)
}

// Reindex (manual cache rebuilds)
export async function reindexSearch(): Promise<Record<string, unknown>> {
  return fetchJson(`${BASE}/search/reindex`, { method: 'POST' })
}

export async function reindexQA(): Promise<Record<string, unknown>> {
  return fetchJson(`${BASE}/qa/reindex`, { method: 'POST' })
}

export async function reindexTags(): Promise<Record<string, unknown>> {
  return fetchJson(`${BASE}/tags/reindex`, { method: 'POST' })
}

// Pricing
export async function getPricing(): Promise<PricingData> {
  return fetchJson(`${BASE}/pricing`)
}

// ---------------------------------------------------------------------------
// Settings / configuration (v0.6.0 — currency, model aliases)
// ---------------------------------------------------------------------------

export interface CfgResponse {
  settings: Record<string, unknown>
  currency: CurrencyInfo
}

export async function getCfg(): Promise<CfgResponse> {
  return fetchJson(`${BASE}/cfg`)
}

export interface CurrenciesResponse {
  common: string[]
  supported: string[]
  current: CurrencyInfo
}

export async function getCurrencies(): Promise<CurrenciesResponse> {
  return fetchJson(`${BASE}/cfg/currencies`)
}

export async function setCurrency(code: string): Promise<{ currency: CurrencyInfo }> {
  return fetchJson(`${BASE}/cfg/currency`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ code }),
  })
}

export interface ModelAliasesResponse {
  aliases: Record<string, string>
}

export async function getModelAliases(): Promise<ModelAliasesResponse> {
  return fetchJson(`${BASE}/cfg/model-aliases`)
}

export async function setModelAlias(from: string, to: string): Promise<ModelAliasesResponse> {
  return fetchJson(`${BASE}/cfg/model-aliases`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ from, to }),
  })
}

export async function deleteModelAlias(from: string): Promise<ModelAliasesResponse> {
  return fetchJson(`${BASE}/cfg/model-aliases?from=${encodeURIComponent(from)}`, {
    method: 'DELETE',
  })
}

// ---------------------------------------------------------------------------
// v0.6.0 follow-up surfaces
// ---------------------------------------------------------------------------

/** Period selector accepted by `/api/compare`. */
export type ComparePeriod = 'today' | 'week' | 'month' | 'all'

/**
 * Period selector accepted by `/api/yield`. Backend accepts a friendlier
 * superset (today/week/month/all + 7days/30days); the UI only passes the
 * canonical four.
 */
export type YieldPeriod = 'today' | 'week' | 'month' | 'all'

/** Period selector accepted by `/api/optimize`. */
export type OptimizePeriod = 'today' | '7days' | '30days' | 'month' | 'all'

export async function getCompare(
  period: ComparePeriod = 'month',
  filters?: FilterParams,
): Promise<CompareResponse> {
  const params = new URLSearchParams({ period })
  // /api/compare accepts a single `provider` (not list) — pick the first
  // active filter value. Multi-provider filtering is done client-side on
  // the resulting rows so the user can still narrow Compare to "claude +
  // codex" via the filter bar.
  if (filters?.providers && filters.providers.length === 1) {
    const first = filters.providers[0]
    if (first) params.set('provider', first.toLowerCase())
  }
  return fetchJson(`${BASE}/compare?${params}`)
}

/**
 * Fetch per-provider cost rollup for the active period. Powers the
 * `CostByProviderCard` widget at the top of the Cost tab. Reuses the
 * `ComparePeriod` enum so card + table stay in sync.
 */
export async function getCostByProvider(
  period: ComparePeriod = 'month',
  filters?: FilterParams,
): Promise<CostByProviderResponse> {
  const params = new URLSearchParams({ period })
  buildFilterParams(params, filters)
  return fetchJson(`${BASE}/cost-data/by-provider?${params}`)
}

export async function getYield(
  period: YieldPeriod = 'week',
  filters?: FilterParams,
): Promise<YieldResponse> {
  const params = new URLSearchParams({ period })
  // /api/yield accepts repeated `?project=` — provider isn't on the route
  // contract, but project filter would be redundant since the dashboard
  // already scopes to one project. We pass providers via a custom param the
  // route doesn't read, which is harmless; client-side row filter does the
  // narrow.
  buildFilterParams(params, filters)
  return fetchJson(`${BASE}/yield?${params}`)
}

export async function getPlan(): Promise<PlanResponse> {
  return fetchJson(`${BASE}/plan`)
}

// ---------------------------------------------------------------------------
// Spend budgets (audit #7p2). The status query takes the browser's UTC offset
// so month/today spend buckets on the user's local day, matching Cost/Live.
// `tzOffsetMinutes` is "minutes east of UTC" — note Date.getTimezoneOffset()
// returns the *opposite* sign, so we negate it (same convention the other
// tz-aware routes use).
// ---------------------------------------------------------------------------

function localTzOffset(): number {
  return -new Date().getTimezoneOffset()
}

export async function getBudgets(): Promise<BudgetResponse> {
  const params = new URLSearchParams({ timezone_offset: String(localTzOffset()) })
  return fetchJson(`${BASE}/budgets?${params}`)
}

export async function setBudgets(update: BudgetUpdate): Promise<BudgetResponse> {
  const params = new URLSearchParams({ timezone_offset: String(localTzOffset()) })
  return fetchJson(`${BASE}/budgets?${params}`, {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(update),
  })
}

export async function clearBudgets(): Promise<BudgetResponse> {
  const params = new URLSearchParams({ timezone_offset: String(localTzOffset()) })
  return fetchJson(`${BASE}/budgets?${params}`, { method: 'DELETE' })
}

/**
 * Cross-provider what-if repricing. With a project active the route scopes to
 * it via `deps.current_log_path`, so no param is needed for the per-project
 * Budgets tab view.
 */
export async function getWhatIf(): Promise<WhatIfResponse> {
  return fetchJson(`${BASE}/whatif`)
}

export async function getOptimize(
  period: OptimizePeriod = 'month',
  filters?: FilterParams,
): Promise<OptimizeResponse> {
  const params = new URLSearchParams({ period })
  buildFilterParams(params, filters)
  return fetchJson(`${BASE}/optimize?${params}`)
}

/**
 * Global context-budget payload (no `project` query param). The route also
 * supports a per-project view; the dashboard's settings card scopes to the
 * global view because that's the only thing every install has.
 */
export async function getContextBudget(): Promise<ContextBudget> {
  return fetchJson(`${BASE}/context-budget`)
}

// ---------------------------------------------------------------------------
// Wave 4F — ETL pipeline status + manual backfill.
//
// The Wave 4C `/api/etl/status` route may not be merged yet; the badge calls
// this fetcher and gracefully degrades to a "not ready" state on 404. The
// backfill POST may also 404 (Wave 4B exposes the orchestrator as a CLI
// command first); in that case the UI surfaces the equivalent CLI command
// rather than the route error.
//
// `EtlPipelineNotReadyError` is the sentinel the UI uses to distinguish a
// missing route from a real network/parse error. Tested in
// `tests/services/etl-status.test.ts`.
// ---------------------------------------------------------------------------

export class EtlPipelineNotReadyError extends Error {
  constructor(message = 'ETL pipeline route not available') {
    super(message)
    this.name = 'EtlPipelineNotReadyError'
  }
}

// Raised when POST /api/etl/backfill returns 409 because another job
// is already running. Carries the existing job's id so callers can
// surface "Backfill abc123 already running" without re-fetching status.
export class EtlBackfillInProgressError extends Error {
  jobId: string
  constructor(jobId: string) {
    super(`A backfill is already running (job ${jobId.slice(0, 8)}…)`)
    this.name = 'EtlBackfillInProgressError'
    this.jobId = jobId
  }
}

export async function getEtlStatus(): Promise<EtlStatusResponse> {
  const res = await fetch(`${BASE}/etl/status`)
  if (res.status === 404) {
    throw new EtlPipelineNotReadyError()
  }
  if (!res.ok) {
    const text = await res.text().catch(() => '')
    throw new Error(`${res.status} ${res.statusText}${text ? `: ${text}` : ''}`)
  }
  return res.json()
}

export async function triggerEtlBackfill(force = false): Promise<EtlBackfillResponse> {
  const res = await fetch(`${BASE}/etl/backfill`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ force }),
  })
  if (res.status === 404) {
    throw new EtlPipelineNotReadyError(
      'Backfill route not available — run `stackunderflow etl backfill` from the CLI instead.',
    )
  }
  if (res.status === 409) {
    // Backfill already running. Pull the existing job_id out of the
    // body so the UI can mention it; default to a placeholder if the
    // body is malformed.
    let jobId = 'unknown'
    try {
      const body = (await res.json()) as { job_id?: string }
      if (typeof body?.job_id === 'string' && body.job_id.length > 0) {
        jobId = body.job_id
      }
    } catch {
      // Malformed JSON — keep the placeholder; the toast will still
      // tell the user a backfill is in progress.
    }
    throw new EtlBackfillInProgressError(jobId)
  }
  if (!res.ok) {
    const text = await res.text().catch(() => '')
    throw new Error(`${res.status} ${res.statusText}${text ? `: ${text}` : ''}`)
  }
  // 202 Accepted — body is `{job_id, started_at}`.
  return res.json()
}

// ---------------------------------------------------------------------------
// ETL helpers — pure functions used by EtlStatusBadge + tests. Kept here so
// the fetcher and its presentation helpers stay together; the badge imports
// `formatEtlBadgeText` and `etlHealthColor` directly.
// ---------------------------------------------------------------------------

/**
 * Compact human-readable duration: `7s`, `2m`, `1h`, `1d`.
 * Single-unit (no minutes:seconds), rounded down to the largest unit so the
 * badge stays narrow.
 */
export function formatLagDuration(seconds: number | null | undefined): string {
  if (seconds == null || !Number.isFinite(seconds) || seconds < 0) return '—'
  const s = Math.floor(seconds)
  if (s < 60) return `${s}s`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m}m`
  const h = Math.floor(m / 60)
  if (h < 24) return `${h}h`
  const d = Math.floor(h / 24)
  return `${d}d`
}

/** Maps a health state to the chip's tailwind colour token + dot shade. */
export function etlHealthColor(health: EtlHealth): {
  badge: 'green' | 'blue' | 'yellow' | 'red'
  dot: string
  pulse: boolean
} {
  switch (health) {
    case 'live':
      return { badge: 'green', dot: 'bg-green-500', pulse: false }
    case 'syncing':
      return { badge: 'blue', dot: 'bg-blue-500', pulse: true }
    case 'stale':
      return { badge: 'yellow', dot: 'bg-yellow-500', pulse: false }
    case 'error':
      return { badge: 'red', dot: 'bg-red-500', pulse: false }
  }
}

/**
 * Compact secondary text for the badge — hidden under 600px (CSS); always
 * rendered for screen readers via aria-label.
 */
export function formatEtlBadgeText(status: EtlStatusResponse): string {
  const { health, lag_seconds, watcher, events, last_job } = status
  switch (health) {
    case 'live':
      return `Live (synced ${formatLagDuration(watcher.seconds_since_refresh ?? lag_seconds)} ago)`
    case 'syncing': {
      // backlog estimate: events in last cycle + max_id - sum of watermarks?
      // The route ships `events_in_last_cycle` directly; that's the friendliest
      // number to surface.
      const behind = watcher.events_in_last_cycle
      return `Syncing (${behind} event${behind === 1 ? '' : 's'} behind)`
    }
    case 'stale':
      return `Stale by ${formatLagDuration(lag_seconds)}`
    case 'error':
      // A recent backfill failure is a more specific (and more
      // actionable) message than the generic ETL error. The assembler
      // escalates ``health`` to ``error`` while the failed last_job
      // is inside its TTL window, so this branch handles both the
      // dead-watcher case and the failed-backfill case.
      if (last_job?.status === 'failed') {
        return `Backfill failed (job ${last_job.job_id.slice(0, 8)}…)`
      }
      return 'ETL error — see /etl/status'
  }
  // Defensive — TS exhaustiveness already catches missing branches above, but
  // an unexpected runtime value should not crash the badge.
  return `Unknown (${events.total} events)`
}

// ---------------------------------------------------------------------------
// Agent-teams — Claude Code parallel-agent topology surface.
//
// Three read-only endpoints:
//   * GET /api/agent-teams                                — list view
//   * GET /api/agent-teams/{session}                      — full graph
//   * GET /api/agent-teams/{session}/agent/{agent_session} — drill-in
//
// Empty stores return {teams: []} cleanly; the route layer never raises 500
// when no sidechain messages are present. See docs/specs/agent-teams.md.
// ---------------------------------------------------------------------------

export async function listAgentTeams(
  limit = 50,
  project?: string | null,
): Promise<AgentTeamListResponse> {
  const params = new URLSearchParams({ limit: String(limit) })
  if (project) params.set('project', project)
  return fetchJson(`${BASE}/agent-teams?${params.toString()}`)
}

export async function getAgentTeam(sessionId: string): Promise<AgentTeamGraph> {
  return fetchJson(`${BASE}/agent-teams/${encodeURIComponent(sessionId)}`)
}

export async function getAgentTeamTranscript(
  sessionId: string,
  agentSessionId: string,
): Promise<AgentTeamTranscriptResponse> {
  return fetchJson(
    `${BASE}/agent-teams/${encodeURIComponent(sessionId)}` +
      `/agent/${encodeURIComponent(agentSessionId)}`,
  )
}

// ---------------------------------------------------------------------------
// URL-state helpers for the Agents tab — keeps the AgentsTab component thin
// and gives the test suite a pure surface to round-trip the encoding.
//
// Contract: ?session=<lead> selects the lead session in the left rail;
// ?agent=<sub> selects one of its sub-agents in the right pane. Either may
// be absent (no selection) or non-string (defensive: URL params always come
// back as `string | null` from the URLSearchParams API).
// ---------------------------------------------------------------------------

export interface AgentTeamSelection {
  session: string | null
  agent: string | null
}

export function readAgentTeamSelection(search: string): AgentTeamSelection {
  const params = new URLSearchParams(search)
  const session = params.get('session')
  const agent = params.get('agent')
  return {
    session: session && session.length > 0 ? session : null,
    agent: agent && agent.length > 0 ? agent : null,
  }
}

export function writeAgentTeamSelection(
  search: string,
  selection: AgentTeamSelection,
): string {
  const params = new URLSearchParams(search)
  if (selection.session) {
    params.set('session', selection.session)
  } else {
    params.delete('session')
  }
  if (selection.agent) {
    params.set('agent', selection.agent)
  } else {
    params.delete('agent')
  }
  const out = params.toString()
  return out.length > 0 ? `?${out}` : ''
}

// ---------------------------------------------------------------------------
// Playback — per-session (and per-project) tool-call timeline.
//
//   * GET /api/playback/{session}                  — ordered event stream
//   * GET /api/playback/project/{slug}             — cross-session timeline
//
// 404 → wrong session/slug; 200 + empty `events` → nothing to play back. The
// `?include_payload=0` knob trims the per-event 200-char excerpts (the
// project endpoint defaults it off — a project-wide stream is large). See
// .notes/specs/10-playback-timeline.md.
// ---------------------------------------------------------------------------

export interface PlaybackQuery {
  /** Restrict to a subset of tool names (exact match). */
  toolFilter?: string[]
  /** Cap on the number of events returned. */
  limit?: number
  /** Whether to include the per-event `payload_excerpt`. */
  includePayload?: boolean
}

function buildPlaybackParams(q?: PlaybackQuery): URLSearchParams {
  const params = new URLSearchParams()
  if (q?.toolFilter && q.toolFilter.length > 0) {
    params.set('tool_filter', q.toolFilter.join(','))
  }
  if (typeof q?.limit === 'number') params.set('limit', String(q.limit))
  if (typeof q?.includePayload === 'boolean') {
    params.set('include_payload', q.includePayload ? '1' : '0')
  }
  return params
}

export async function getPlayback(
  sessionId: string,
  q?: PlaybackQuery,
): Promise<PlaybackResponse> {
  const qs = buildPlaybackParams(q).toString()
  return fetchJson(`${BASE}/playback/${encodeURIComponent(sessionId)}${qs ? `?${qs}` : ''}`)
}

export async function getProjectTimeline(
  projectSlug: string,
  q?: PlaybackQuery & { since?: string },
): Promise<ProjectTimelineResponse> {
  const params = buildPlaybackParams(q)
  if (q?.since) params.set('since', q.since)
  const qs = params.toString()
  return fetchJson(`${BASE}/playback/project/${encodeURIComponent(projectSlug)}${qs ? `?${qs}` : ''}`)
}

// ---------------------------------------------------------------------------
// Playback v2 — virtual-filesystem reconstruction at a point in time.
//
//   * GET /api/playback/{session}/fs?at=<iso>&paths=<csv>&include_content=…
//
// 404 → unknown session; 422 → unparseable `at`; 200 + `files: {}` → the
// session exists but issued no file-touching tool calls before `at`.
//
// `include_content=false` returns metadata only (byte counts + operation
// labels) without the file bodies, which is useful when scrubbing rapidly.
// See stackunderflow/services/playback_fs.py.
// ---------------------------------------------------------------------------

export interface PlaybackFsQuery {
  /** Cutoff timestamp (ISO-8601 / RFC-3339). Required by the backend. */
  at: string
  /** Restrict to a subset of file paths (relative to the session cwd). */
  paths?: string[]
  /** When false, the response omits `content` (metadata-only). Default true. */
  includeContent?: boolean
}

/**
 * Sentinel thrown when the backend says `at` couldn't be parsed (422). The
 * panel surfaces this with a "Bad timestamp" warning rather than the generic
 * "Failed to load…" message.
 */
export class PlaybackFsBadTimestampError extends Error {
  constructor(detail: string) {
    super(`Unparseable timestamp: ${detail}`)
    this.name = 'PlaybackFsBadTimestampError'
  }
}

export async function getPlaybackFsSnapshot(
  sessionId: string,
  q: PlaybackFsQuery,
): Promise<PlaybackFsSnapshotResponse> {
  const params = new URLSearchParams({ at: q.at })
  if (q.paths && q.paths.length > 0) {
    params.set('paths', q.paths.join(','))
  }
  if (typeof q.includeContent === 'boolean') {
    params.set('include_content', q.includeContent ? 'true' : 'false')
  }
  const res = await fetch(
    `${BASE}/playback/${encodeURIComponent(sessionId)}/fs?${params.toString()}`,
  )
  if (res.status === 422) {
    const body = await res.text().catch(() => '')
    throw new PlaybackFsBadTimestampError(body || q.at)
  }
  if (!res.ok) {
    const text = await res.text().catch(() => '')
    throw new Error(`${res.status} ${res.statusText}${text ? `: ${text}` : ''}`)
  }
  return res.json()
}

// ---------------------------------------------------------------------------
// URL-state helpers for the Playback tab — keep PlaybackTab thin and give the
// test suite a pure surface to round-trip the encoding.
//
// Contract: ?session=<id> selects the session being played back; ?seq=<n>
// positions the scrubber at the n-th event. `seq` parses to a non-negative
// integer or `null` (anything else — negative, NaN, absent — is `null`).
// ---------------------------------------------------------------------------

export interface PlaybackSelection {
  session: string | null
  seq: number | null
}

function parseSeq(raw: string | null): number | null {
  if (raw === null || raw.trim() === '') return null
  const n = Number(raw)
  if (!Number.isInteger(n) || n < 0) return null
  return n
}

export function readPlaybackSelection(search: string): PlaybackSelection {
  const params = new URLSearchParams(search)
  const session = params.get('session')
  return {
    session: session && session.length > 0 ? session : null,
    seq: parseSeq(params.get('seq')),
  }
}

export function writePlaybackSelection(
  search: string,
  selection: PlaybackSelection,
): string {
  const params = new URLSearchParams(search)
  if (selection.session) {
    params.set('session', selection.session)
  } else {
    params.delete('session')
  }
  if (selection.seq !== null && selection.seq >= 0 && Number.isInteger(selection.seq)) {
    params.set('seq', String(selection.seq))
  } else {
    params.delete('seq')
  }
  const out = params.toString()
  return out.length > 0 ? `?${out}` : ''
}
