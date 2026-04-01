import type {
  ProjectsResponse,
  SetProjectResponse,
  JsonlFile,
  JsonlContentResponse,
  DashboardData,
  Message,
  QAListResponse,
  QADetailResponse,
  SearchResponse,
  TagCloudResponse,
  TagBrowseResponse,
  SessionTags,
  BookmarkListResponse,
  Bookmark,
  RelatedResponse,
  PricingData,
  Curriculum,
  CurriculumStatus,
  ErrorExercise,
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

export async function getCurrentProject() {
  return fetchJson<{ status: string; project_path?: string; log_path?: string; log_dir_name?: string }>(
    `${BASE}/project`
  )
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

export async function getQADetail(qaId: string): Promise<QADetailResponse> {
  return fetchJson(`${BASE}/qa/${encodeURIComponent(qaId)}`)
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

export async function getSessionTags(sessionId: string): Promise<SessionTags> {
  return fetchJson(`${BASE}/tags/session/${encodeURIComponent(sessionId)}`)
}

export async function addManualTag(sessionId: string, tag: string) {
  return fetchJson(`${BASE}/tags/session/${encodeURIComponent(sessionId)}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ tag }),
  })
}

export async function removeManualTag(sessionId: string, tag: string) {
  return fetchJson(`${BASE}/tags/session/${encodeURIComponent(sessionId)}/${encodeURIComponent(tag)}`, {
    method: 'DELETE',
  })
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

export async function addBookmark(data: {
  session_id: string
  title: string
  message_index?: number
  notes?: string
  tags?: string[]
}): Promise<Bookmark> {
  return fetchJson(`${BASE}/bookmarks`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(data),
  })
}

export async function removeBookmark(bookmarkId: string) {
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

export async function toggleBookmark(data: { session_id: string; title?: string; message_index?: number }) {
  return fetchJson(`${BASE}/bookmarks/toggle`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(data),
  })
}

// Related sessions
export async function getRelatedSessions(sessionId: string, limit = 5): Promise<RelatedResponse> {
  return fetchJson(`${BASE}/related/${encodeURIComponent(sessionId)}?limit=${limit}`)
}

// Pricing
export async function getPricing(): Promise<PricingData> {
  return fetchJson(`${BASE}/pricing`)
}

// Health
export async function healthCheck(): Promise<{ status: string }> {
  return fetchJson(`${BASE}/health`)
}

// Refresh
export async function refreshData(timezoneOffset = 0) {
  return fetchJson(`${BASE}/refresh`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ timezone_offset: timezoneOffset }),
  })
}

// Global Stats
export async function getGlobalStats(): Promise<Record<string, unknown>> {
  return fetchJson(`${BASE}/global-stats`)
}

// Reindex endpoints
export async function reindexSearch(): Promise<Record<string, unknown>> {
  return fetchJson(`${BASE}/search/reindex`, { method: 'POST' })
}

export async function getSearchStats(): Promise<Record<string, unknown>> {
  return fetchJson(`${BASE}/search/stats`)
}

export async function getQAStats(): Promise<Record<string, unknown>> {
  return fetchJson(`${BASE}/qa/stats`)
}

export async function reindexQA(): Promise<Record<string, unknown>> {
  return fetchJson(`${BASE}/qa/reindex`, { method: 'POST' })
}

export async function reindexTags(): Promise<Record<string, unknown>> {
  return fetchJson(`${BASE}/tags/reindex`, { method: 'POST' })
}

export async function getSessionBookmarks(sessionId: string): Promise<BookmarkListResponse> {
  return fetchJson(`${BASE}/bookmarks/session/${encodeURIComponent(sessionId)}`)
}

// Curriculum (Modal-powered learning)
export async function getCurriculum(options?: {
  focus?: string
  difficulty?: 'beginner' | 'intermediate' | 'advanced'
  refresh?: boolean
}): Promise<Curriculum> {
  const params = new URLSearchParams()
  if (options?.focus) params.set('focus', options.focus)
  if (options?.difficulty) params.set('difficulty', options.difficulty)
  if (options?.refresh) params.set('refresh', 'true')

  const query = params.toString()
  return fetchJson(`${BASE}/curriculum${query ? `?${query}` : ''}`)
}

export async function getCurriculumStatus(): Promise<CurriculumStatus> {
  return fetchJson(`${BASE}/curriculum/status`)
}

export async function getExerciseForError(errorCategory: string): Promise<ErrorExercise> {
  return fetchJson(`${BASE}/curriculum/exercise/${encodeURIComponent(errorCategory)}`)
}
