/**
 * App-wide currency state.
 *
 * Resolution: a single React Query keyed on ``['cfg']`` fetches the current
 * settings + currency block from ``/api/cfg``. The provider exposes:
 *
 *   - ``currency`` — the active {code, symbol, rate_from_usd, warning} block,
 *     or null while the initial fetch is in flight.
 *   - ``setCurrencyCode(code)`` — POSTs to ``/api/cfg/currency``, then
 *     invalidates ``['cfg']`` and ``['dashboardData', *]`` so any open tab
 *     re-fetches with the new currency baked into its cost figures.
 *
 * `formatCost` accepts a `CurrencyInfo` directly; components that already
 * receive `stats` from React Query can pass the dashboard payload's
 * currency block. Components elsewhere can call `useCurrency()` here.
 *
 * When the backend reports a fallback (live Frankfurter feed unreachable,
 * rate stale, snapshot in use, or unknown code degraded to USD) the
 * payload's ``warning`` field is non-null. The provider renders a thin
 * yellow banner across the top of the dashboard with that message; it
 * auto-clears on the next successful fetch (no warning ⇒ no banner).
 */

import { createContext, useContext, useCallback, useState, type ReactNode } from 'react'
import { useQuery, useQueryClient, useMutation } from '@tanstack/react-query'
import { getCfg, setCurrency as setCurrencyApi } from './api'
import type { CurrencyInfo } from '../types/api'

interface CurrencyContextValue {
  currency: CurrencyInfo | null
  isLoading: boolean
  setCurrencyCode: (code: string) => Promise<void>
}

const Ctx = createContext<CurrencyContextValue | null>(null)

/**
 * Yellow banner shown above the app whenever ``currency.warning`` is set.
 * The user can dismiss it with the X — the dismissal is keyed on the
 * warning text, so a *new* warning (e.g. fresh fetch produces a different
 * staleness message) re-shows itself automatically.
 */
function CurrencyWarningBanner({
  warning,
  onDismiss,
}: {
  warning: string
  onDismiss: () => void
}) {
  return (
    <div
      role="alert"
      className="bg-yellow-100 dark:bg-yellow-900/40 border-b border-yellow-300 dark:border-yellow-800 text-yellow-900 dark:text-yellow-100 text-sm px-4 py-2 flex items-start gap-3"
    >
      <span aria-hidden="true" className="font-semibold mt-0.5">
        FX:
      </span>
      <span className="flex-1 leading-snug">{warning}</span>
      <button
        type="button"
        onClick={onDismiss}
        aria-label="Dismiss currency warning"
        className="ml-2 text-yellow-900/70 dark:text-yellow-100/70 hover:text-yellow-900 dark:hover:text-yellow-100 font-bold"
      >
        ×
      </button>
    </div>
  )
}

export function CurrencyProvider({ children }: { children: ReactNode }) {
  const queryClient = useQueryClient()

  // Per-warning dismissal state. We key on the warning *text* rather than a
  // boolean so the banner re-appears if the backend produces a new message
  // on a later refresh (e.g. "rate is 5 days old" → "rate is 12 days old").
  const [dismissedWarning, setDismissedWarning] = useState<string | null>(null)

  const cfgQuery = useQuery({
    queryKey: ['cfg'],
    queryFn: getCfg,
    // Currency rarely changes; cache for 5 minutes so route changes don't
    // re-fetch. The mutation invalidates explicitly when the user changes
    // the active currency.
    staleTime: 5 * 60_000,
  })

  const mutation = useMutation({
    mutationFn: (code: string) => setCurrencyApi(code),
    onSuccess: () => {
      // Invalidate every cost-bearing query so values re-fetch in the new
      // currency. Dashboard and project queries are the canonical entry
      // points; downstream tabs get their data from the dashboard payload.
      queryClient.invalidateQueries({ queryKey: ['cfg'] })
      queryClient.invalidateQueries({ queryKey: ['dashboardData'] })
      queryClient.invalidateQueries({ queryKey: ['projects'] })
      queryClient.invalidateQueries({ queryKey: ['globalStats'] })
      queryClient.invalidateQueries({ queryKey: ['jsonlFiles'] })
      // A currency change may produce a fresh, no-warning payload — reset
      // the dismissal so the banner is ready to appear again if needed.
      setDismissedWarning(null)
    },
  })

  const setCurrencyCode = useCallback(
    async (code: string) => {
      await mutation.mutateAsync(code)
    },
    [mutation],
  )

  const currency = cfgQuery.data?.currency ?? null
  const value: CurrencyContextValue = {
    currency,
    isLoading: cfgQuery.isLoading,
    setCurrencyCode,
  }

  const warning = currency?.warning ?? null
  const showBanner = !!warning && warning !== dismissedWarning

  return (
    <Ctx.Provider value={value}>
      {showBanner && warning && (
        <CurrencyWarningBanner
          warning={warning}
          onDismiss={() => setDismissedWarning(warning)}
        />
      )}
      {children}
    </Ctx.Provider>
  )
}

export function useCurrency(): CurrencyContextValue {
  const v = useContext(Ctx)
  if (!v) {
    // Defensive default for tests / Storybook / any tree without the
    // provider wrapped — falls back to USD, never throws so a missing
    // provider degrades gracefully.
    return {
      currency: null,
      isLoading: false,
      setCurrencyCode: async () => {
        /* no-op */
      },
    }
  }
  return v
}
