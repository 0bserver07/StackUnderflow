/**
 * App-wide currency state.
 *
 * Resolution: a single React Query keyed on ``['cfg']`` fetches the current
 * settings + currency block from ``/api/cfg``. The provider exposes:
 *
 *   - ``currency`` — the active {code, symbol, rate_from_usd} block, or null
 *     while the initial fetch is in flight.
 *   - ``setCurrencyCode(code)`` — POSTs to ``/api/cfg/currency``, then
 *     invalidates ``['cfg']`` and ``['dashboardData', *]`` so any open tab
 *     re-fetches with the new currency baked into its cost figures.
 *
 * `formatCost` accepts a `CurrencyInfo` directly; components that already
 * receive `stats` from React Query can pass the dashboard payload's
 * currency block. Components elsewhere can call `useCurrency()` here.
 */

import { createContext, useContext, useCallback, type ReactNode } from 'react'
import { useQuery, useQueryClient, useMutation } from '@tanstack/react-query'
import { getCfg, setCurrency as setCurrencyApi } from './api'
import type { CurrencyInfo } from '../types/api'

interface CurrencyContextValue {
  currency: CurrencyInfo | null
  isLoading: boolean
  setCurrencyCode: (code: string) => Promise<void>
}

const Ctx = createContext<CurrencyContextValue | null>(null)

export function CurrencyProvider({ children }: { children: ReactNode }) {
  const queryClient = useQueryClient()

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
    },
  })

  const setCurrencyCode = useCallback(
    async (code: string) => {
      await mutation.mutateAsync(code)
    },
    [mutation],
  )

  const value: CurrencyContextValue = {
    currency: cfgQuery.data?.currency ?? null,
    isLoading: cfgQuery.isLoading,
    setCurrencyCode,
  }

  return <Ctx.Provider value={value}>{children}</Ctx.Provider>
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
