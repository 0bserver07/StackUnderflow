import { useCallback, useEffect, useState } from 'react'

/**
 * localStorage key used to persist the global beta-features toggle.
 */
export const BETA_ENABLED_KEY = 'suf:beta'

/**
 * localStorage key used to persist per-tab visibility overrides.
 */
export const TAB_VISIBILITY_KEY = 'suf:tabs'

/**
 * Per-tab visibility override.
 *
 * - `'shown'`  — force the tab visible, regardless of the beta flag.
 * - `'hidden'` — force the tab hidden, regardless of the beta flag.
 * - `'default'`— clear the override and fall back to the beta-flag logic.
 */
export type TabVisibility = 'shown' | 'hidden' | 'default'

/**
 * Default value of {@link BETA_ENABLED_KEY} when nothing is stored.
 *
 * Defaults to `true` so upgrading users don't silently lose tabs they were
 * already using. New installs can opt out via the Settings page.
 */
export const DEFAULT_BETA_ENABLED = true

export interface BetaFeaturesState {
  /** Whether beta-tagged tabs are globally enabled. */
  betaEnabled: boolean
  /** Map of tab id → per-tab visibility override. */
  tabOverrides: Record<string, TabVisibility>
  /** Update the global beta flag (state + localStorage). */
  setBetaEnabled: (v: boolean) => void
  /**
   * Set the override for a single tab. Passing `'default'` removes the
   * override so the tab falls back to the beta-flag logic.
   */
  setTabVisibility: (tabId: string, v: TabVisibility) => void
  /**
   * Resolve whether a tab should be visible right now.
   *
   * - `'shown'`  override → `true`
   * - `'hidden'` override → `false`
   * - no override, stable tab (`isBeta === false`) → `true`
   * - no override, beta tab → `betaEnabled`
   */
  isTabVisible: (tabId: string, isBeta: boolean) => boolean
  /** Clear both localStorage keys and reset state to defaults. */
  reset: () => void
}

function readStoredBetaEnabled(): boolean {
  if (typeof window === 'undefined') return DEFAULT_BETA_ENABLED
  try {
    const raw = window.localStorage.getItem(BETA_ENABLED_KEY)
    if (raw === null) return DEFAULT_BETA_ENABLED
    const parsed = JSON.parse(raw)
    if (typeof parsed === 'boolean') return parsed
  } catch {
    // localStorage may be unavailable or value may be malformed; fall through.
  }
  return DEFAULT_BETA_ENABLED
}

function readStoredTabOverrides(): Record<string, TabVisibility> {
  if (typeof window === 'undefined') return {}
  try {
    const raw = window.localStorage.getItem(TAB_VISIBILITY_KEY)
    if (raw === null) return {}
    const parsed = JSON.parse(raw)
    if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) {
      const out: Record<string, TabVisibility> = {}
      for (const [k, v] of Object.entries(parsed as Record<string, unknown>)) {
        if (v === 'shown' || v === 'hidden') {
          out[k] = v
        }
      }
      return out
    }
  } catch {
    // Malformed JSON or unavailable storage; fall through.
  }
  return {}
}

/**
 * Hook for reading and updating beta-feature visibility state.
 *
 * - Reads initial state from `localStorage[suf:beta]` and `localStorage[suf:tabs]`.
 * - Persists every change to `localStorage`.
 * - SSR-safe: all `window.localStorage` access is guarded.
 *
 * The hook mirrors the style of {@link ./useTheme.ts} — initializer + a
 * `useEffect` sync on change.
 */
export function useBetaFeatures(): BetaFeaturesState {
  const [betaEnabled, setBetaEnabledState] = useState<boolean>(() => readStoredBetaEnabled())
  const [tabOverrides, setTabOverridesState] = useState<Record<string, TabVisibility>>(() =>
    readStoredTabOverrides(),
  )

  useEffect(() => {
    if (typeof window === 'undefined') return
    try {
      window.localStorage.setItem(BETA_ENABLED_KEY, JSON.stringify(betaEnabled))
    } catch {
      // Ignore persistence failures — the in-memory state still works.
    }
  }, [betaEnabled])

  useEffect(() => {
    if (typeof window === 'undefined') return
    try {
      window.localStorage.setItem(TAB_VISIBILITY_KEY, JSON.stringify(tabOverrides))
    } catch {
      // Ignore persistence failures — the in-memory state still works.
    }
  }, [tabOverrides])

  const setBetaEnabled = useCallback((v: boolean) => {
    setBetaEnabledState(v)
  }, [])

  const setTabVisibility = useCallback((tabId: string, v: TabVisibility) => {
    setTabOverridesState((prev) => {
      if (v === 'default') {
        if (!(tabId in prev)) return prev
        const next = { ...prev }
        delete next[tabId]
        return next
      }
      if (prev[tabId] === v) return prev
      return { ...prev, [tabId]: v }
    })
  }, [])

  const isTabVisible = useCallback(
    (tabId: string, isBeta: boolean) => {
      const override = tabOverrides[tabId]
      if (override === 'shown') return true
      if (override === 'hidden') return false
      if (!isBeta) return true
      return betaEnabled
    },
    [betaEnabled, tabOverrides],
  )

  const reset = useCallback(() => {
    if (typeof window !== 'undefined') {
      try {
        window.localStorage.removeItem(BETA_ENABLED_KEY)
        window.localStorage.removeItem(TAB_VISIBILITY_KEY)
      } catch {
        // Ignore; state reset below still works in-memory.
      }
    }
    setBetaEnabledState(DEFAULT_BETA_ENABLED)
    setTabOverridesState({})
  }, [])

  return {
    betaEnabled,
    tabOverrides,
    setBetaEnabled,
    setTabVisibility,
    isTabVisible,
    reset,
  }
}
