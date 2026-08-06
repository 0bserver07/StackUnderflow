import { memo, useState } from 'react'
import {
  IconWallet,
  IconCheck,
  IconAlertTriangle,
  IconTrash,
  IconCircleCheck,
} from '@tabler/icons-react'
import type { BudgetBand, BudgetLegStatus, BudgetResponse, CurrencyInfo } from '../../types/api'
import { formatCost } from '../../services/format'

// ---------------------------------------------------------------------------
// BudgetCard — spend budgets (audit #7p2).
//
// Renders the active monthly/daily ceilings, a traffic-light status bar for
// each (under / approaching / over), and the linear month-end projection with
// an overrun warning. Editing is delegated to the parent via `onSave` /
// `onClear` so the BudgetsTab owns the React Query mutation + cache
// invalidation. Memoized — only re-renders when the budget data or currency
// identity changes (or when the user types, via local input state).
// ---------------------------------------------------------------------------

interface BudgetCardProps {
  data: BudgetResponse
  currency: CurrencyInfo | null
  onSave: (monthly: number | null, daily: number | null) => void
  onClear: () => void
  isSaving: boolean
}

const BAND_META: Record<BudgetBand, { color: string; bar: string; label: string }> = {
  under: { color: 'text-green-600 dark:text-green-400', bar: 'bg-green-500', label: 'on track' },
  approaching: {
    color: 'text-amber-600 dark:text-amber-400',
    bar: 'bg-amber-500',
    label: 'approaching',
  },
  over: { color: 'text-red-600 dark:text-red-400', bar: 'bg-red-500', label: 'over budget' },
}

function StatusLeg({
  title,
  leg,
  currency,
}: {
  title: string
  leg: BudgetLegStatus
  currency: CurrencyInfo | null
}) {
  const meta = BAND_META[leg.status]
  const pctClamped = Math.min(100, Math.max(0, leg.pct))
  return (
    <div className="space-y-1.5">
      <div className="flex items-center justify-between text-xs">
        <span className="text-gray-500 uppercase tracking-wider font-medium">{title}</span>
        <span className={`font-medium ${meta.color}`}>{meta.label}</span>
      </div>
      <div className="h-2 rounded-full bg-gray-200 dark:bg-gray-700 overflow-hidden">
        <div
          className={`h-full ${meta.bar} transition-all`}
          style={{ width: `${pctClamped}%` }}
        />
      </div>
      <div className="flex items-center justify-between text-xs text-gray-600 dark:text-gray-400 tabular-nums">
        <span>
          {formatCost(leg.used, currency)} / {formatCost(leg.budget, currency)}
        </span>
        <span className={meta.color}>{leg.pct.toFixed(0)}%</span>
      </div>
    </div>
  )
}

function BudgetCard({ data, currency, onSave, onClear, isSaving }: BudgetCardProps) {
  const { budget, status } = data
  const isSet = budget.monthly_usd !== null || budget.daily_usd !== null

  // Local input state seeded from the persisted budget. Empty string = "no
  // ceiling for this leg" → null on save.
  const [monthly, setMonthly] = useState<string>(
    budget.monthly_usd !== null ? String(budget.monthly_usd) : '',
  )
  const [daily, setDaily] = useState<string>(
    budget.daily_usd !== null ? String(budget.daily_usd) : '',
  )

  const parse = (s: string): number | null => {
    const t = s.trim()
    if (t === '') return null
    const n = Number(t)
    return Number.isFinite(n) && n > 0 ? n : null
  }

  const monthlyValid = monthly.trim() === '' || parse(monthly) !== null
  const dailyValid = daily.trim() === '' || parse(daily) !== null
  const canSave = monthlyValid && dailyValid && (monthly.trim() !== '' || daily.trim() !== '')

  const handleSave = () => {
    if (!canSave) return
    onSave(parse(monthly), parse(daily))
  }

  return (
    <div className="bg-white dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-800 p-4 space-y-4">
      <div className="flex items-center gap-2">
        <IconWallet size={16} className="text-indigo-500" />
        <h3 className="text-sm font-semibold text-gray-800 dark:text-gray-200">Spend budgets</h3>
        <span className="text-xs text-gray-500">your own monthly / daily ceilings</span>
      </div>

      {/* Status (only when a budget is set) */}
      {isSet && status && (
        <div className="space-y-3">
          {status.monthly && (
            <StatusLeg title="Month to date" leg={status.monthly} currency={currency} />
          )}
          {status.daily && <StatusLeg title="Today" leg={status.daily} currency={currency} />}

          {status.projected_month_end !== null && (
            <div
              className={`flex items-start gap-2 rounded-md p-2.5 text-xs ${
                status.projection_overruns
                  ? 'bg-red-50 dark:bg-red-900/20 border border-red-300 dark:border-red-800 text-red-700 dark:text-red-300'
                  : 'bg-gray-50 dark:bg-gray-800/40 border border-gray-200 dark:border-gray-700 text-gray-600 dark:text-gray-400'
              }`}
            >
              {status.projection_overruns ? (
                <IconAlertTriangle size={14} className="flex-shrink-0 mt-0.5" />
              ) : (
                <IconCircleCheck size={14} className="flex-shrink-0 mt-0.5 text-green-500" />
              )}
              <span>
                Projected month-end:{' '}
                <span className="font-semibold tabular-nums">
                  {formatCost(status.projected_month_end, currency)}
                </span>
                {status.projection_overruns
                  ? " — on pace to exceed your monthly budget."
                  : ' — within budget at the current pace.'}
              </span>
            </div>
          )}
        </div>
      )}

      {/* Editor */}
      <div className="space-y-2 pt-1 border-t border-gray-100 dark:border-gray-800">
        <div className="grid grid-cols-2 gap-3">
          <label className="text-xs text-gray-500">
            Monthly (USD)
            <input
              type="number"
              min="0"
              step="1"
              inputMode="decimal"
              value={monthly}
              onChange={(e) => setMonthly(e.target.value)}
              placeholder="none"
              className={`mt-1 w-full rounded-md border px-2 py-1.5 text-sm bg-white dark:bg-gray-800 text-gray-900 dark:text-gray-100 ${
                monthlyValid
                  ? 'border-gray-300 dark:border-gray-700'
                  : 'border-red-400 dark:border-red-600'
              }`}
            />
          </label>
          <label className="text-xs text-gray-500">
            Daily (USD)
            <input
              type="number"
              min="0"
              step="1"
              inputMode="decimal"
              value={daily}
              onChange={(e) => setDaily(e.target.value)}
              placeholder="none"
              className={`mt-1 w-full rounded-md border px-2 py-1.5 text-sm bg-white dark:bg-gray-800 text-gray-900 dark:text-gray-100 ${
                dailyValid ? 'border-gray-300 dark:border-gray-700' : 'border-red-400 dark:border-red-600'
              }`}
            />
          </label>
        </div>
        <div className="flex items-center gap-2">
          <button
            type="button"
            onClick={handleSave}
            disabled={!canSave || isSaving}
            className="inline-flex items-center gap-1.5 rounded-md bg-indigo-500 px-3 py-1.5 text-xs font-medium text-white hover:bg-indigo-600 disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
          >
            <IconCheck size={14} />
            {isSaving ? 'Saving…' : 'Save budget'}
          </button>
          {isSet && (
            <button
              type="button"
              onClick={onClear}
              disabled={isSaving}
              className="inline-flex items-center gap-1.5 rounded-md border border-gray-300 dark:border-gray-700 px-3 py-1.5 text-xs font-medium text-gray-600 dark:text-gray-400 hover:bg-gray-50 dark:hover:bg-gray-800 disabled:opacity-50 transition-colors"
            >
              <IconTrash size={14} />
              Clear
            </button>
          )}
        </div>
        {!isSet && (
          <p className="text-[11px] text-gray-400">
            Set a monthly and/or daily ceiling to track spend against it. Budgets are
            yours alone — separate from any subscription plan, and never leave this
            machine.
          </p>
        )}
      </div>
    </div>
  )
}

export default memo(BudgetCard)
