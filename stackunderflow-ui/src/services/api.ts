import type {
  ProjectsResponse,
  SetProjectResponse,
  JsonlFile,
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
export async function getJsonlFiles(project?: string): Promise<JsonlFile[]> {
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
  page?: number
  per_page?: number
}): Promise<QAListResponse> {
  const searchParams = new URLSearchParams()
  if (params.project) searchParams.set('project', params.project)
  if (params.date_from) searchParams.set('date_from', params.date_from)
  if (params.date_to) searchParams.set('date_to', params.date_to)
  if (params.search) searchParams.set('search', params.search)
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
