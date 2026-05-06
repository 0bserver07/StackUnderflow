import type {
  ProjectsResponse,
  SetProjectResponse,
  JsonlFilesResponse,
  JsonlContentResponse,
  DashboardData,
  Message,
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
  OptimizeResponse,
  ContextBudget,
  EtlStatusResponse,
  EtlBackfillResponse,
  EtlHealth,
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
export async function getProjects(
  includeStats = false,
  filters?: FilterParams,
): Promise<ProjectsResponse> {
  const params = new URLSearchParams({ include_stats: String(includeStats) })
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
export async function getMessages(
  limit?: number,
  filters?: FilterParams,
): Promise<Message[]> {
  const params = new URLSearchParams()
  if (limit) params.set('limit', String(limit))
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
  if (!res.ok) {
    const text = await res.text().catch(() => '')
    throw new Error(`${res.status} ${res.statusText}${text ? `: ${text}` : ''}`)
  }
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
  const { health, lag_seconds, watcher, events } = status
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
      return 'ETL error — see /etl/status'
  }
  // Defensive — TS exhaustiveness already catches missing branches above, but
  // an unexpected runtime value should not crash the badge.
  return `Unknown (${events.total} events)`
}
