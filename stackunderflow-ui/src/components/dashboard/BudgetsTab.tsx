import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import { IconWallet } from '@tabler/icons-react'
import {
  getBudgets,
  setBudgets,
  clearBudgets,
  getWhatIf,
} from '../../services/api'
import type { BudgetResponse } from '../../types/api'
import { useCurrency } from '../../services/currency'
import LoadingSpinner from '../common/LoadingSpinner'
import EmptyState from '../common/EmptyState'
import BudgetCard from '../cost/BudgetCard'
import WhatIfComparison from '../cost/WhatIfComparison'

// ---------------------------------------------------------------------------
// BudgetsTab — cost-intelligence (audit #7 part 2).
//
// Two stacked cards:
//   1. BudgetCard       — set/track a monthly + daily spend budget (/api/budgets).
//   2. WhatIfComparison — reprice this project's token workload across providers
//                         (/api/whatif).
//
// This tab owns the React Query lifecycles; the cards are presentational and
// memoized. Budget mutations invalidate the `['budgets']` query so the status
// bar refreshes after a save/clear without a manual refetch.
// ---------------------------------------------------------------------------

export default function BudgetsTab() {
  const { currency: ctxCurrency } = useCurrency()
  const queryClient = useQueryClient()

  const budgetQuery = useQuery({
    queryKey: ['budgets'],
    queryFn: getBudgets,
    staleTime: 30_000,
  })

  const whatIfQuery = useQuery({
    queryKey: ['whatif'],
    queryFn: getWhatIf,
    staleTime: 60_000,
  })

  const saveMutation = useMutation({
    mutationFn: (vars: { monthly: number | null; daily: number | null }) =>
      setBudgets({ monthly_usd: vars.monthly, daily_usd: vars.daily }),
    onSuccess: (data) => {
      // Seed the cache with the route's fresh payload so the bar updates
      // immediately, then invalidate to stay consistent with any background poll.
      queryClient.setQueryData(['budgets'], data)
      queryClient.invalidateQueries({ queryKey: ['budgets'] })
    },
  })

  const clearMutation = useMutation({
    mutationFn: clearBudgets,
    onSuccess: (data) => {
      queryClient.setQueryData(['budgets'], data)
      queryClient.invalidateQueries({ queryKey: ['budgets'] })
    },
  })

  const isSaving = saveMutation.isPending || clearMutation.isPending
  const budgetData: BudgetResponse | undefined = budgetQuery.data
  // Prefer the budget payload's own currency (it stamps one), falling back to
  // the app-wide currency context so the cards still render before first load.
  const currency = budgetData?.currency ?? whatIfQuery.data?.currency ?? ctxCurrency

  if (budgetQuery.isLoading && !budgetData) {
    return <LoadingSpinner message="Loading budgets…" />
  }

  if (budgetQuery.error && !budgetData) {
    return (
      <EmptyState
        icon={<IconWallet size={28} />}
        title="Couldn't load budgets"
        description={
          budgetQuery.error instanceof Error ? budgetQuery.error.message : 'Unknown error'
        }
      />
    )
  }

  return (
    <div className="space-y-4">
      {budgetData && (
        <BudgetCard
          data={budgetData}
          currency={currency}
          isSaving={isSaving}
          onSave={(monthly, daily) => saveMutation.mutate({ monthly, daily })}
          onClear={() => clearMutation.mutate()}
        />
      )}

      {(saveMutation.error || clearMutation.error) && (
        <div className="bg-red-50 dark:bg-red-900/20 border border-red-300 dark:border-red-800 rounded-lg p-3 text-red-700 dark:text-red-400 text-sm">
          Failed to update budget:{' '}
          {(saveMutation.error ?? clearMutation.error) instanceof Error
            ? (saveMutation.error ?? clearMutation.error)!.message
            : 'Unknown error'}
        </div>
      )}

      {whatIfQuery.isLoading && <LoadingSpinner message="Repricing across providers…" />}

      {whatIfQuery.error && (
        <div className="bg-red-50 dark:bg-red-900/20 border border-red-300 dark:border-red-800 rounded-lg p-3 text-red-700 dark:text-red-400 text-sm">
          Failed to load what-if:{' '}
          {whatIfQuery.error instanceof Error ? whatIfQuery.error.message : 'Unknown error'}
        </div>
      )}

      {whatIfQuery.data && (
        <WhatIfComparison data={whatIfQuery.data} currency={currency} />
      )}
    </div>
  )
}
