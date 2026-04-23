/**
 * Cross-tab navigation service for the StackUnderflow dashboard.
 *
 * Provides a small, framework-agnostic API for moving between tabs
 * (Overview / Sessions / Messages / Cost / etc.) and deep-linking to
 * a specific session or interaction. URL state is the source of truth;
 * a `stackunderflow:nav` CustomEvent is dispatched on every mutation so
 * any React component (or plain listener) can react without a full reload.
 *
 * Pure module — no React. Safe to import from anywhere.
 *
 * Spec: docs/specs/analytics-polish.md §A8
 */

export const NAV_EVENT = 'stackunderflow:nav'

export interface NavDetail {
  tab: string
  [key: string]: string
}

function hasWindow(): boolean {
  return typeof window !== 'undefined' && typeof window.history !== 'undefined'
}

function dispatch(detail: NavDetail): void {
  if (!hasWindow()) return
  window.dispatchEvent(new CustomEvent<NavDetail>(NAV_EVENT, { detail }))
}

/**
 * Push a new URL with the given query params, preserving the path/hash.
 * No-op (besides the event) if the resulting URL matches the current one,
 * so repeated calls don't pollute the history stack.
 */
function pushParams(params: Record<string, string>): string {
  if (!hasWindow()) return ''
  const url = new URL(window.location.href)
  // Reset existing params so callers get the exact set they asked for.
  url.search = ''
  for (const [key, value] of Object.entries(params)) {
    url.searchParams.set(key, value)
  }
  const next = `${url.pathname}${url.search}${url.hash}`
  const current = `${window.location.pathname}${window.location.search}${window.location.hash}`
  if (next !== current) {
    window.history.pushState({}, '', next)
  }
  return url.search
}

/**
 * Switch to the Messages tab and deep-link to a single interaction.
 *
 * Updates the URL to `?tab=messages&interaction=<id>` (preserving path)
 * and emits a {@link NAV_EVENT} CustomEvent so listeners can react.
 */
export function openInteraction(interactionId: string): void {
  pushParams({ tab: 'messages', interaction: interactionId })
  dispatch({ tab: 'messages', interaction: interactionId })
}

/**
 * Switch to the Sessions tab and deep-link to a single session.
 *
 * Updates the URL to `?tab=sessions&session=<id>` and emits {@link NAV_EVENT}.
 */
export function openSession(sessionId: string): void {
  pushParams({ tab: 'sessions', session: sessionId })
  dispatch({ tab: 'sessions', session: sessionId })
}

/**
 * Switch to an arbitrary tab, optionally with extra query params.
 *
 * Example: `setTab('cost', { range: '7d' })` → `?tab=cost&range=7d`.
 */
export function setTab(tab: string, extraParams?: Record<string, string>): void {
  const params: Record<string, string> = { tab, ...(extraParams ?? {}) }
  pushParams(params)
  dispatch(params as NavDetail)
}

/** Read the `tab` query param, or null if missing / SSR. */
export function getTabFromURL(): string | null {
  if (!hasWindow()) return null
  return new URLSearchParams(window.location.search).get('tab')
}

/** Read a single query param by name, or null if missing / SSR. */
export function getParam(name: string): string | null {
  if (!hasWindow()) return null
  return new URLSearchParams(window.location.search).get(name)
}

/**
 * Remove one query param via `history.replaceState` (no new history entry).
 * Useful after handling a deep-link so a refresh doesn't re-trigger it.
 */
export function clearParam(name: string): void {
  if (!hasWindow()) return
  const url = new URL(window.location.href)
  if (!url.searchParams.has(name)) return
  url.searchParams.delete(name)
  const next = `${url.pathname}${url.search}${url.hash}`
  window.history.replaceState({}, '', next)
}
