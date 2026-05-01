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
  YieldResponse,
  PlanResponse,
  OptimizeResponse,
  ContextBudget,
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

// Projects
export async function getProjects(includeStats = false): Promise<ProjectsResponse> {
  return fetchJson(`${BASE}/projects?include_stats=${includeStats}`)
}

export async function setProjectByDir(dirName: string): Promise<SetProjectResponse> {
  return fetchJson(`${BASE}/project-by-dir`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ dir_name: dirName }),
  })
}

// Dashboard
export async function getDashboardData(timezoneOffset = 0): Promise<DashboardData> {
  return fetchJson(`${BASE}/dashboard-data?timezone_offset=${timezoneOffset}`)
}

// Messages
export async function getMessages(limit?: number): Promise<Message[]> {
  const params = limit ? `?limit=${limit}` : ''
  return fetchJson(`${BASE}/messages${params}`)
}

// JSONL files
//
// Returns the full {files, currency} envelope since v0.6.0 (multi-currency
// PR wrapped the previously bare list). Callers that only need the file
// metadata can destructure `.files`; the currency block is also propagated
// upward so consumers can render cost columns in the active currency.
export async function getJsonlFiles(project?: string): Promise<JsonlFilesResponse> {
  const params = project ? `?project=${encodeURIComponent(project)}` : ''
  return fetchJson(`${BASE}/jsonl-files${params}`)
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

export async function getCompare(period: ComparePeriod = 'month'): Promise<CompareResponse> {
  return fetchJson(`${BASE}/compare?period=${encodeURIComponent(period)}`)
}

export async function getYield(period: YieldPeriod = 'week'): Promise<YieldResponse> {
  return fetchJson(`${BASE}/yield?period=${encodeURIComponent(period)}`)
}

export async function getPlan(): Promise<PlanResponse> {
  return fetchJson(`${BASE}/plan`)
}

export async function getOptimize(period: OptimizePeriod = 'month'): Promise<OptimizeResponse> {
  return fetchJson(`${BASE}/optimize?period=${encodeURIComponent(period)}`)
}

/**
 * Global context-budget payload (no `project` query param). The route also
 * supports a per-project view; the dashboard's settings card scopes to the
 * global view because that's the only thing every install has.
 */
export async function getContextBudget(): Promise<ContextBudget> {
  return fetchJson(`${BASE}/context-budget`)
}
