/**
 * Pure URL <-> filter-state helpers, free of any JSX so they can be
 * tested under `node --test` without a transpiler step.
 *
 * These functions are the wire format that the React `<FiltersProvider>`
 * (in `filters.tsx`) reads and writes. Keeping the parser separate from
 * the React glue means we can verify the case-insensitive read /
 * lowercased-on-emit contract without spinning up a DOM.
 */

export interface ActiveFilters {
  /** Lowercased list of provider ids. Empty = "all providers". */
  providers: string[]
  /** Lowercased list of model ids. Empty = "all models". */
  models: string[]
}

export const PROVIDER_PARAM = 'provider'
export const MODEL_PARAM = 'model'

function hasWindow(): boolean {
  return typeof window !== 'undefined' && typeof window.history !== 'undefined'
}

/** Lowercase + dedupe + drop-empties. Keeps array referentially distinct. */
export function normalize(values: readonly string[]): string[] {
  const seen = new Set<string>()
  const out: string[] = []
  for (const raw of values) {
    if (typeof raw !== 'string') continue
    const v = raw.toLowerCase().trim()
    if (!v) continue
    if (seen.has(v)) continue
    seen.add(v)
    out.push(v)
  }
  return out
}

/** Read the current filter set from `window.location.search`. */
export function readFromURL(): ActiveFilters {
  if (!hasWindow()) {
    return { providers: [], models: [] }
  }
  const params = new URLSearchParams(window.location.search)
  return {
    providers: normalize(params.getAll(PROVIDER_PARAM)),
    models: normalize(params.getAll(MODEL_PARAM)),
  }
}

/**
 * Mirror filter state into the URL via `history.replaceState`. Never push,
 * so toggling chips doesn't pollute the back/forward stack. Other params
 * (`tab`, `session`, `interaction`, …) are preserved.
 */
export function writeToURL(filters: ActiveFilters): void {
  if (!hasWindow()) return
  const url = new URL(window.location.href)
  url.searchParams.delete(PROVIDER_PARAM)
  url.searchParams.delete(MODEL_PARAM)
  for (const p of filters.providers) {
    url.searchParams.append(PROVIDER_PARAM, p)
  }
  for (const m of filters.models) {
    url.searchParams.append(MODEL_PARAM, m)
  }
  const next = `${url.pathname}${url.search}${url.hash}`
  const current = `${window.location.pathname}${window.location.search}${window.location.hash}`
  if (next !== current) {
    window.history.replaceState({}, '', next)
  }
}

/** Build the `&provider=…&model=…` fragment used by API helpers. */
export function buildQueryString(filters: ActiveFilters): string {
  if (filters.providers.length === 0 && filters.models.length === 0) return ''
  const params = new URLSearchParams()
  for (const p of filters.providers) params.append(PROVIDER_PARAM, p)
  for (const m of filters.models) params.append(MODEL_PARAM, m)
  return `&${params.toString()}`
}
