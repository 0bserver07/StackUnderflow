// ---------------------------------------------------------------------------
// Worktree intelligence client — `GET /api/worktrees` + the fragment
// attribution POST (campaign #8).
//
// Deliberately its own module (not `api.ts`): same isolation rationale as
// `patterns.ts` — the worktree surface was built as an additive unit, and a
// self-contained client keeps it file-disjoint from parallel work on the
// main API client. The fetch helper mirrors `api.ts::fetchJson` exactly.
// ---------------------------------------------------------------------------

import type { WorktreeAttributeResponse, WorktreesResponse } from '../types/api'

const BASE = '/api'

async function fetchJson<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await fetch(url, init)
  if (!res.ok) {
    const text = await res.text().catch(() => '')
    throw new Error(`${res.status} ${res.statusText}${text ? `: ${text}` : ''}`)
  }
  return res.json()
}

/**
 * Scan the project's worktrees. The scan runs live against git (read-only —
 * detection + verdicts only, never mutation), so results reflect the repo as
 * of the request. `logPath` scopes to a project (the active project's
 * `log_path` from the setProject response); omit it for whole-store scope —
 * the response's `scope` field echoes what the server used. Cost fields
 * arrive pre-converted to the active currency (same contract as /api/forks).
 */
export async function getWorktrees(logPath?: string): Promise<WorktreesResponse> {
  const params = new URLSearchParams()
  if (logPath) params.set('log_path', logPath)
  const qs = params.toString()
  return fetchJson(`${BASE}/worktrees${qs ? `?${qs}` : ''}`)
}

/**
 * Attribute worktree-fragment sessions to their parent project so phantom
 * sibling "projects" fold into the real one's analytics. Store-only writes
 * (the additive attribution column) — never touches git state. Returns the
 * number of records the pass updated.
 */
export async function attributeWorktrees(logPath?: string): Promise<WorktreeAttributeResponse> {
  return fetchJson(`${BASE}/worktrees/attribute`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(logPath ? { log_path: logPath } : {}),
  })
}
