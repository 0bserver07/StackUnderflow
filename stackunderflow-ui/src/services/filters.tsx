/**
 * Provider/model filter state — global, URL-synced source of truth.
 *
 * Everywhere the dashboard needs to know "which providers / models is the
 * user currently scoped to?" it should read from `useFilters()`. The state
 * lives at the top of the dashboard (mounted in `App.tsx`) and exposes a
 * minimal API:
 *
 *   const { filters, setProviders, setModels, addProvider, addModel,
 *           clearFilters, isFiltered, queryString } = useFilters()
 *
 * The two arrays are case-normalised (lowercased on write) so callers can
 * round-trip "?provider=Cursor" without worrying about casing. `filters`
 * is referentially stable per change — components can put it directly
 * into a React Query key:
 *
 *   useQuery({ queryKey: ['compare', period, filters], ... })
 *
 * When the filter set changes:
 *   - the URL is rewritten via `history.replaceState` (no new history entry,
 *     so toggling a chip never traps the back button), preserving every
 *     other param (`tab`, `session`, `interaction`, …)
 *   - `queryString` (e.g. `"&provider=cursor&model=opus-4-7"`) is updated
 *     so route-passing helpers can splice it onto fetch URLs
 *
 * On initial mount, the provider returns the URL-derived state immediately
 * — no flashing-to-empty before hydration.
 */

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from 'react'
import {
  type ActiveFilters,
  buildQueryString,
  normalize,
  readFromURL,
  writeToURL,
} from './filterUrl'

// Re-export so existing callers can keep importing from `services/filters`.
export type { ActiveFilters } from './filterUrl'

export interface FiltersContextValue {
  filters: ActiveFilters
  /** Replace the active provider list (already-lowercased on write). */
  setProviders: (providers: string[]) => void
  /** Replace the active model list (already-lowercased on write). */
  setModels: (models: string[]) => void
  /** Add a provider if not already present. No-op when present. */
  addProvider: (provider: string) => void
  /** Add a model if not already present. No-op when present. */
  addModel: (model: string) => void
  /** Remove a provider by exact id (case-insensitive). */
  removeProvider: (provider: string) => void
  /** Remove a model by exact id (case-insensitive). */
  removeModel: (model: string) => void
  /** Clear all filters (providers + models). */
  clearFilters: () => void
  /** True when at least one provider or model is active. */
  isFiltered: boolean
  /**
   * URLSearchParams-encoded fragment for splicing onto an outbound URL.
   * Includes the leading `&` so callers can write `${BASE}/foo?bar=1${qs}`.
   * Returns empty string when no filters are active.
   */
  queryString: string
}

// ---------------------------------------------------------------------------
// Internal — used only inside the provider. The pure helpers live in
// `filterUrl.ts` so they can be unit-tested under `node --test` without a
// JSX transformer in the path.
// ---------------------------------------------------------------------------

function hasWindow(): boolean {
  return typeof window !== 'undefined' && typeof window.history !== 'undefined'
}

// ---------------------------------------------------------------------------
// Context + provider
// ---------------------------------------------------------------------------

const FiltersContext = createContext<FiltersContextValue | undefined>(undefined)

export function FiltersProvider({ children }: { children: ReactNode }) {
  // Hydrate from URL on first render so a refresh / shared link starts in
  // the right state (no empty-state flash before useEffect fires).
  const [filters, setFilters] = useState<ActiveFilters>(() => readFromURL())

  // Keep URL in sync whenever the set mutates. The early-out inside
  // `writeToURL` makes this idempotent on a no-op render.
  useEffect(() => {
    writeToURL(filters)
  }, [filters])

  // React to back/forward navigation: when the URL is mutated externally
  // (browser back, NAV_EVENT navigation, etc.), re-read so chips stay in
  // step with the URL the user is actually looking at.
  useEffect(() => {
    if (!hasWindow()) return
    const sync = () => {
      const next = readFromURL()
      setFilters((prev) => {
        if (
          prev.providers.length === next.providers.length &&
          prev.providers.every((v, i) => v === next.providers[i]) &&
          prev.models.length === next.models.length &&
          prev.models.every((v, i) => v === next.models[i])
        ) {
          return prev
        }
        return next
      })
    }
    window.addEventListener('popstate', sync)
    return () => window.removeEventListener('popstate', sync)
  }, [])

  const setProviders = useCallback((providers: string[]) => {
    setFilters((prev) => {
      const next = normalize(providers)
      // Cheap referential-equality check so identical arrays don't trigger
      // a re-render on every callback invocation.
      if (
        prev.providers.length === next.length &&
        prev.providers.every((v, i) => v === next[i])
      ) {
        return prev
      }
      return { ...prev, providers: next }
    })
  }, [])

  const setModels = useCallback((models: string[]) => {
    setFilters((prev) => {
      const next = normalize(models)
      if (
        prev.models.length === next.length &&
        prev.models.every((v, i) => v === next[i])
      ) {
        return prev
      }
      return { ...prev, models: next }
    })
  }, [])

  const addProvider = useCallback((provider: string) => {
    const v = provider.toLowerCase().trim()
    if (!v) return
    setFilters((prev) => {
      if (prev.providers.includes(v)) return prev
      return { ...prev, providers: [...prev.providers, v] }
    })
  }, [])

  const addModel = useCallback((model: string) => {
    const v = model.toLowerCase().trim()
    if (!v) return
    setFilters((prev) => {
      if (prev.models.includes(v)) return prev
      return { ...prev, models: [...prev.models, v] }
    })
  }, [])

  const removeProvider = useCallback((provider: string) => {
    const v = provider.toLowerCase().trim()
    setFilters((prev) => {
      if (!prev.providers.includes(v)) return prev
      return { ...prev, providers: prev.providers.filter((p) => p !== v) }
    })
  }, [])

  const removeModel = useCallback((model: string) => {
    const v = model.toLowerCase().trim()
    setFilters((prev) => {
      if (!prev.models.includes(v)) return prev
      return { ...prev, models: prev.models.filter((m) => m !== v) }
    })
  }, [])

  const clearFilters = useCallback(() => {
    setFilters((prev) => {
      if (prev.providers.length === 0 && prev.models.length === 0) return prev
      return { providers: [], models: [] }
    })
  }, [])

  const value = useMemo<FiltersContextValue>(
    () => ({
      filters,
      setProviders,
      setModels,
      addProvider,
      addModel,
      removeProvider,
      removeModel,
      clearFilters,
      isFiltered:
        filters.providers.length > 0 || filters.models.length > 0,
      queryString: buildQueryString(filters),
    }),
    [
      filters,
      setProviders,
      setModels,
      addProvider,
      addModel,
      removeProvider,
      removeModel,
      clearFilters,
    ],
  )

  return (
    <FiltersContext.Provider value={value}>{children}</FiltersContext.Provider>
  )
}

/**
 * Hook into the active filter set. Throws if called outside a
 * `<FiltersProvider>` so missing-mount mistakes fail loudly in dev.
 */
export function useFilters(): FiltersContextValue {
  const ctx = useContext(FiltersContext)
  if (!ctx) {
    throw new Error('useFilters() must be used inside a <FiltersProvider>')
  }
  return ctx
}

// Re-export the pure helpers so existing callers don't have to import from
// two modules; tests should still import from `./filterUrl` directly.
export { normalize, readFromURL, writeToURL, buildQueryString } from './filterUrl'
