// ---------------------------------------------------------------------------
// Multi-device sync client — `GET /api/sync/status` + `GET /api/sync/overview`
// (#100 Phase 2, the union read overlay).
//
// Its own module (not `api.ts`): same isolation rationale as `worktrees.ts` /
// `patterns.ts` — the sync surface is an additive, self-contained unit, so a
// disjoint client keeps it file-separate from parallel work on the main API
// client. The fetch helper mirrors `api.ts::fetchJson` exactly.
//
// Both endpoints are read-only. `getSyncStatus` is a pure local read (safe with
// sync off). `getSyncOverview` only runs the cross-device union on the opt-in
// `?scope=all-devices` path — the default `this-device` returns a tiny stub.
// ---------------------------------------------------------------------------

import type { SyncOverview, SyncStatus } from '../types/api'

const BASE = '/api'

async function fetchJson<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await fetch(url, init)
  if (!res.ok) {
    const text = await res.text().catch(() => '')
    throw new Error(`${res.status} ${res.statusText}${text ? `: ${text}` : ''}`)
  }
  return res.json()
}

/** Scope selector for {@link getSyncOverview}. `this-device` (the default) is
 *  the cheap stub path; `all-devices` triggers the union roll-up. */
export type SyncScope = 'this-device' | 'all-devices'

/**
 * Local sync config + known peers + whether any cross-device data has been
 * pulled. Pure local read — never hits the network or a bucket, and works
 * whether sync is configured or not (returns `enabled: false` when it isn't).
 */
export async function getSyncStatus(): Promise<SyncStatus> {
  return fetchJson(`${BASE}/sync/status`)
}

/**
 * The cross-device overview. Defaults to `this-device`, which returns a tiny
 * not-merged stub and runs no union (so a store with sync off behaves as if the
 * feature were absent). Only `scope='all-devices'` (with sync enabled) computes
 * the `local UNION ALL <mart>_remote` roll-up; its cost fields arrive
 * pre-converted to the active currency (same contract as /api/forks,
 * /api/worktrees). The response is discriminated on `merged`.
 */
export async function getSyncOverview(scope: SyncScope = 'this-device'): Promise<SyncOverview> {
  const params = new URLSearchParams({ scope })
  return fetchJson(`${BASE}/sync/overview?${params.toString()}`)
}
