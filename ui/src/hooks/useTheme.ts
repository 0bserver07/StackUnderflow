import { useCallback, useEffect, useState } from 'react'

/**
 * Theme values supported by the app.
 */
export type Theme = 'dark' | 'light'

/**
 * localStorage key used to persist the user's theme preference.
 * Exported so the pre-paint initializer in `main.tsx` uses the same key.
 */
export const THEME_STORAGE_KEY = 'suf:theme'

/**
 * Default theme when nothing is stored. Matches the app's current look.
 */
export const DEFAULT_THEME: Theme = 'dark'

function readStoredTheme(): Theme {
  if (typeof window === 'undefined') return DEFAULT_THEME
  try {
    const raw = window.localStorage.getItem(THEME_STORAGE_KEY)
    if (raw === 'light' || raw === 'dark') return raw
  } catch {
    // localStorage may be unavailable (e.g. privacy mode); fall through.
  }
  return DEFAULT_THEME
}

function applyThemeClass(theme: Theme): void {
  if (typeof document === 'undefined') return
  const root = document.documentElement
  if (theme === 'dark') {
    root.classList.add('dark')
  } else {
    root.classList.remove('dark')
  }
}

export interface UseThemeResult {
  /** The currently active theme. */
  theme: Theme
  /** Flip between `'dark'` and `'light'`. */
  toggle: () => void
  /** Set the theme explicitly. */
  setTheme: (t: Theme) => void
}

/**
 * Hook for reading and updating the app theme.
 *
 * - Reads the initial theme from `localStorage[suf:theme]`, defaulting to `'dark'`.
 * - Persists every change to `localStorage`.
 * - Side effect: keeps the `dark` class on `<html>` in sync with the current theme.
 *
 * The first-paint class is applied by the initializer in `main.tsx` so there is
 * no flash of the wrong theme. This hook re-applies on mount to stay correct
 * even if something external mutated the class.
 */
export function useTheme(): UseThemeResult {
  const [theme, setThemeState] = useState<Theme>(() => readStoredTheme())

  useEffect(() => {
    applyThemeClass(theme)
    if (typeof window === 'undefined') return
    try {
      window.localStorage.setItem(THEME_STORAGE_KEY, theme)
    } catch {
      // Ignore persistence failures — the in-memory state still works.
    }
  }, [theme])

  const setTheme = useCallback((t: Theme) => {
    setThemeState(t)
  }, [])

  const toggle = useCallback(() => {
    setThemeState((prev) => (prev === 'dark' ? 'light' : 'dark'))
  }, [])

  return { theme, toggle, setTheme }
}
