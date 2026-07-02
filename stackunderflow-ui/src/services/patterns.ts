// ---------------------------------------------------------------------------
// Cross-session pattern mining client — `GET /api/patterns` (campaign #6).
//
// Deliberately its own module (not `api.ts`): the coding-health surface was
// built as an isolated, additive unit, and a self-contained client keeps it
// file-disjoint from parallel work on the main API client. The fetch helper
// mirrors `api.ts::fetchJson` exactly.
// ---------------------------------------------------------------------------

import type { PatternsResponse } from '../types/api'

const BASE = '/api'

/** Window selector values the tab offers. The API accepts any `<days>d` up
 * to `365d`; these are the curated presets. */
export type PatternsSince = '7d' | '30d' | '90d'

async function fetchJson<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await fetch(url, init)
  if (!res.ok) {
    const text = await res.text().catch(() => '')
    throw new Error(`${res.status} ${res.statusText}${text ? `: ${text}` : ''}`)
  }
  return res.json()
}

/**
 * Fetch the coding-health report. The server scopes to the active project
 * via `deps.current_log_path`, so — like `getForks` — the project identity
 * is folded into the caller's query key rather than passed here. Pass
 * `project` explicitly only to inspect a non-active project by slug.
 */
export async function getPatterns(
  since: PatternsSince = '90d',
  project?: string,
): Promise<PatternsResponse> {
  const params = new URLSearchParams({ since })
  if (project) params.set('project', project)
  return fetchJson(`${BASE}/patterns?${params}`)
}
