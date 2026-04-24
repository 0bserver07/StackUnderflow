import { useCallback, useEffect, useState } from 'react'
import { Link } from 'react-router-dom'
import { IconArrowLeft, IconMoon, IconSun } from '@tabler/icons-react'
import { useTheme } from '../hooks/useTheme'

// ---------------------------------------------------------------------------
// TODO(beta-foundation): the two blocks below are inline stubs that MUST be
// replaced with real imports once G1 (`beta-foundation`) has landed on main:
//
//   import { useBetaFeatures } from '../hooks/useBetaFeatures'
//   import BetaBadge from '../components/common/BetaBadge'
//
// Delete everything between the STUB-START/STUB-END banners when you swap them
// in. The real module lives at `stackunderflow-ui/src/hooks/useBetaFeatures.ts`
// (see docs/specs/beta-features.md §API) and the badge at
// `stackunderflow-ui/src/components/common/BetaBadge.tsx`.
// ---------------------------------------------------------------------------

// --- STUB-START: useBetaFeatures ------------------------------------------
const BETA_ENABLED_KEY = 'suf:beta'
const TAB_VISIBILITY_KEY = 'suf:tabs'

type TabVisibility = 'shown' | 'hidden' | 'default'

interface BetaFeaturesState {
  betaEnabled: boolean
  tabOverrides: Record<string, TabVisibility>
  setBetaEnabled: (v: boolean) => void
  setTabVisibility: (tabId: string, v: TabVisibility) => void
  isTabVisible: (tabId: string, isBeta: boolean) => boolean
  reset: () => void
}

function readBetaEnabled(): boolean {
  if (typeof window === 'undefined') return true
  try {
    const raw = window.localStorage.getItem(BETA_ENABLED_KEY)
    if (raw === null) return true
    return raw === 'true'
  } catch {
    return true
  }
}

function readTabOverrides(): Record<string, TabVisibility> {
  if (typeof window === 'undefined') return {}
  try {
    const raw = window.localStorage.getItem(TAB_VISIBILITY_KEY)
    if (!raw) return {}
    const parsed = JSON.parse(raw)
    if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) {
      return parsed as Record<string, TabVisibility>
    }
  } catch {
    // fall through
  }
  return {}
}

function useBetaFeatures(): BetaFeaturesState {
  const [betaEnabled, setBetaEnabledState] = useState<boolean>(() => readBetaEnabled())
  const [tabOverrides, setTabOverridesState] = useState<Record<string, TabVisibility>>(
    () => readTabOverrides()
  )

  useEffect(() => {
    try {
      window.localStorage.setItem(BETA_ENABLED_KEY, String(betaEnabled))
    } catch {
      /* ignore */
    }
  }, [betaEnabled])

  useEffect(() => {
    try {
      window.localStorage.setItem(TAB_VISIBILITY_KEY, JSON.stringify(tabOverrides))
    } catch {
      /* ignore */
    }
  }, [tabOverrides])

  const setBetaEnabled = useCallback((v: boolean) => setBetaEnabledState(v), [])

  const setTabVisibility = useCallback((tabId: string, v: TabVisibility) => {
    setTabOverridesState(prev => {
      const next = { ...prev }
      if (v === 'default') delete next[tabId]
      else next[tabId] = v
      return next
    })
  }, [])

  const isTabVisible = useCallback(
    (tabId: string, isBeta: boolean): boolean => {
      const ov = tabOverrides[tabId]
      if (ov === 'shown') return true
      if (ov === 'hidden') return false
      if (!isBeta) return true
      return betaEnabled
    },
    [tabOverrides, betaEnabled]
  )

  const reset = useCallback(() => {
    try {
      window.localStorage.removeItem(BETA_ENABLED_KEY)
      window.localStorage.removeItem(TAB_VISIBILITY_KEY)
    } catch {
      /* ignore */
    }
    setBetaEnabledState(true)
    setTabOverridesState({})
  }, [])

  return { betaEnabled, tabOverrides, setBetaEnabled, setTabVisibility, isTabVisible, reset }
}
// --- STUB-END: useBetaFeatures --------------------------------------------

// --- STUB-START: BetaBadge ------------------------------------------------
function BetaBadge({ className }: { className?: string }) {
  const base =
    'bg-amber-100 text-amber-800 dark:bg-amber-900/40 dark:text-amber-300 ' +
    'text-[10px] px-1.5 py-0.5 rounded uppercase tracking-wider font-semibold'
  return <span className={className ? `${base} ${className}` : base}>BETA</span>
}
// --- STUB-END: BetaBadge --------------------------------------------------

// Hardcoded mirror of pages/ProjectDashboard.tsx TABS. Keep in sync when that
// list changes. Order and beta flags from docs/specs/beta-features.md §Design.
interface TabMeta {
  id: string
  label: string
  isBeta: boolean
}

const TABS: readonly TabMeta[] = [
  { id: 'overview', label: 'Overview', isBeta: false },
  { id: 'sessions', label: 'Sessions', isBeta: false },
  { id: 'cost', label: 'Cost', isBeta: false },
  { id: 'commands', label: 'Commands', isBeta: false },
  { id: 'messages', label: 'Messages', isBeta: false },
  { id: 'search', label: 'Search', isBeta: false },
  { id: 'qa', label: 'Q&A', isBeta: true },
  { id: 'tags', label: 'Tags', isBeta: true },
  { id: 'bookmarks', label: 'Bookmarks', isBeta: false },
] as const

export default function Settings() {
  const { theme, toggle: toggleTheme } = useTheme()
  const {
    betaEnabled,
    tabOverrides,
    setBetaEnabled,
    setTabVisibility,
    reset,
  } = useBetaFeatures()

  const handleReset = () => {
    reset()
    // Reload so every mounted consumer re-reads localStorage with defaults.
    if (typeof window !== 'undefined') window.location.reload()
  }

  const ThemeIcon = theme === 'dark' ? IconSun : IconMoon

  return (
    <div className="max-w-3xl mx-auto p-6 space-y-8">
      {/* Back link */}
      <div>
        <Link
          to="/"
          className="inline-flex items-center gap-1.5 text-sm text-gray-600 dark:text-gray-400 hover:text-gray-900 dark:hover:text-gray-100"
        >
          <IconArrowLeft size={16} />
          Back to Overview
        </Link>
      </div>

      <div>
        <h1 className="text-2xl font-bold text-gray-900 dark:text-gray-100">Settings</h1>
        <p className="text-sm text-gray-500 mt-1">
          Customize appearance and which dashboard tabs are visible.
        </p>
      </div>

      {/* 1. Appearance --------------------------------------------------- */}
      <section className="bg-white dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-800 p-5">
        <h2 className="text-base font-semibold text-gray-900 dark:text-gray-100">Appearance</h2>
        <p className="text-xs text-gray-500 mt-1">
          Switch between dark and light mode. Persists across reloads.
        </p>
        <div className="mt-4 flex items-center justify-between">
          <div className="flex items-center gap-2">
            <ThemeIcon size={18} className="text-gray-600 dark:text-gray-400" />
            <div>
              <div className="text-sm font-medium text-gray-900 dark:text-gray-100">Theme</div>
              <div className="text-xs text-gray-500">
                Current: <span className="font-mono">{theme}</span>
              </div>
            </div>
          </div>
          <button
            onClick={toggleTheme}
            className="px-3 py-1.5 text-sm rounded border border-gray-300 dark:border-gray-700 bg-white dark:bg-gray-800 text-gray-700 dark:text-gray-200 hover:border-gray-400 dark:hover:border-gray-600"
          >
            Switch to {theme === 'dark' ? 'light' : 'dark'}
          </button>
        </div>
      </section>

      {/* 2. Beta features ------------------------------------------------ */}
      <section className="bg-white dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-800 p-5">
        <h2 className="text-base font-semibold text-gray-900 dark:text-gray-100">Beta features</h2>
        <p className="text-xs text-gray-500 mt-1">
          Heuristic features that may not be fully reliable yet. Turn this off to hide BETA-tagged
          tabs on project dashboards.
        </p>
        <label className="mt-4 flex items-center justify-between cursor-pointer">
          <div>
            <div className="text-sm font-medium text-gray-900 dark:text-gray-100">
              Show beta features
            </div>
            <div className="text-xs text-gray-500">
              {betaEnabled
                ? 'Beta tabs are visible by default.'
                : 'Beta tabs are hidden by default.'}
            </div>
          </div>
          <input
            type="checkbox"
            checked={betaEnabled}
            onChange={e => setBetaEnabled(e.target.checked)}
            className="h-4 w-4 accent-indigo-600"
            aria-label="Show beta features"
          />
        </label>
      </section>

      {/* 3. Tab visibility ----------------------------------------------- */}
      <section className="bg-white dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-800 p-5">
        <h2 className="text-base font-semibold text-gray-900 dark:text-gray-100">Tab visibility</h2>
        <p className="text-xs text-gray-500 mt-1">
          <span className="font-medium">Default</span> follows the beta toggle for BETA tabs
          (shown if the toggle is on) and always shows stable tabs.{' '}
          <span className="font-medium">Shown</span> and{' '}
          <span className="font-medium">Hidden</span> override that.
        </p>
        <div className="mt-4 divide-y divide-gray-200 dark:divide-gray-800">
          {TABS.map(tab => {
            const current: TabVisibility = tabOverrides[tab.id] ?? 'default'
            return (
              <div
                key={tab.id}
                className="flex items-center justify-between py-2.5 first:pt-0 last:pb-0"
              >
                <div className="flex items-center gap-2">
                  <span className="text-sm text-gray-900 dark:text-gray-100">{tab.label}</span>
                  {tab.isBeta && <BetaBadge />}
                </div>
                <select
                  value={current}
                  onChange={e => setTabVisibility(tab.id, e.target.value as TabVisibility)}
                  className="bg-white dark:bg-gray-800 border border-gray-300 dark:border-gray-700 rounded px-2 py-1 text-xs text-gray-700 dark:text-gray-300 focus:outline-none focus:border-indigo-500"
                  aria-label={`Visibility for ${tab.label} tab`}
                >
                  <option value="default">Default</option>
                  <option value="shown">Shown</option>
                  <option value="hidden">Hidden</option>
                </select>
              </div>
            )
          })}
        </div>
      </section>

      {/* 4. Danger zone / reset ------------------------------------------ */}
      <section className="bg-white dark:bg-gray-900 rounded-lg border border-red-200 dark:border-red-900/50 p-5">
        <h2 className="text-base font-semibold text-red-700 dark:text-red-400">Danger zone</h2>
        <p className="text-xs text-gray-500 mt-1">
          Clears beta toggle and tab overrides, then reloads the page. Your theme, bookmarks,
          and project data are not touched.
        </p>
        <button
          onClick={handleReset}
          className="mt-4 px-3 py-1.5 text-sm rounded border border-red-300 dark:border-red-800 text-red-700 dark:text-red-400 bg-white dark:bg-gray-900 hover:bg-red-50 dark:hover:bg-red-900/20"
        >
          Reset all settings to defaults
        </button>
      </section>
    </div>
  )
}
