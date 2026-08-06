import { useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { IconScale, IconAlertCircle } from '@tabler/icons-react'
import { getBenchmark, type BenchmarkPeriod } from '../../services/api'
import type {
  BenchmarkReportData,
  BenchmarkStratum,
  BenchmarkModelRow,
  BenchmarkConfidence,
  BenchmarkCellVerdict,
} from '../../types/api'
import LoadingSpinner from '../common/LoadingSpinner'
import EmptyState from '../common/EmptyState'
import { formatCost } from '../../services/format'
import { useCurrency } from '../../services/currency'

// ---------------------------------------------------------------------------
// ModelWinsPanel — "Which model wins" (spec 26 / issue #99).
//
// Renders `GET /api/benchmark` beneath the Compare tab's model×cost table: an
// observational benchmark over the user's own history. The point of the panel
// is honesty, made visual — every row carries n, coverage, a Wilson success CI,
// and a confidence chip; under-powered rows render greyed as "insufficient
// evidence" rather than a fake rank; and a method banner states the natural-
// experiment caveat up front.
// ---------------------------------------------------------------------------

const PERIODS: { id: BenchmarkPeriod; label: string }[] = [
  { id: 'today', label: 'Today' },
  { id: 'week', label: '7d' },
  { id: 'month', label: '30d' },
  { id: 'all', label: 'All' },
]

function pct(x: number | null | undefined): string {
  if (x === null || x === undefined || !Number.isFinite(x)) return '—'
  return `${(x * 100).toFixed(0)}%`
}

const CONFIDENCE_CLASS: Record<BenchmarkConfidence, string> = {
  high: 'bg-green-500/10 text-green-600 dark:text-green-400',
  medium: 'bg-blue-500/10 text-blue-600 dark:text-blue-400',
  low: 'bg-yellow-500/10 text-yellow-700 dark:text-yellow-400',
  none: 'bg-gray-500/10 text-gray-500 dark:text-gray-400',
}

const CELL_VERDICT_CLASS: Record<BenchmarkCellVerdict, string> = {
  clear: 'bg-green-500/10 text-green-600 dark:text-green-400',
  weak: 'bg-yellow-500/10 text-yellow-700 dark:text-yellow-400',
  'insufficient evidence': 'bg-gray-500/10 text-gray-500 dark:text-gray-400',
}

function Chip({ label, className }: { label: string; className: string }) {
  return (
    <span className={`inline-flex items-center rounded px-1.5 py-0.5 text-[10px] font-medium ${className}`}>
      {label}
    </span>
  )
}

function MethodBanner({ report }: { report: BenchmarkReportData }) {
  const cov = report.coverage
  return (
    <div className="flex items-start gap-2 bg-blue-50 dark:bg-blue-900/20 border border-blue-200 dark:border-blue-800 rounded-md p-3 text-blue-800 dark:text-blue-300 text-xs">
      <IconAlertCircle size={14} className="flex-shrink-0 mt-0.5" />
      <span>
        Based on {cov.sessions_total.toLocaleString()} sessions you already ran —
        a natural experiment, not a controlled trial. Success measured on{' '}
        {cov.sessions_scored.toLocaleString()}/{cov.sessions_total.toLocaleString()}{' '}
        sessions · grade coverage {pct(cov.grade_coverage)}. Weights: success{' '}
        {report.weights.success}, cost {report.weights.cost}, effort{' '}
        {report.weights.effort} · {Math.round(report.ci_level * 100)}% CI.
      </span>
    </div>
  )
}

function VerdictCard({
  report,
  currency,
}: {
  report: BenchmarkReportData
  currency: ReturnType<typeof useCurrency>['currency']
}) {
  const v = report.verdict
  if (!v.winning_model) {
    return (
      <div className="rounded-lg border border-gray-200 dark:border-gray-800 bg-white dark:bg-gray-900 p-4">
        <div className="flex items-center gap-2">
          <Chip label="insufficient evidence" className={CONFIDENCE_CLASS.none} />
          <span className="text-sm text-gray-600 dark:text-gray-400">
            No cross-task winner yet
          </span>
        </div>
        {v.caveats[0] && (
          <p className="text-xs text-gray-500 mt-2">{v.caveats[0]}</p>
        )}
      </div>
    )
  }
  return (
    <div className="rounded-lg border border-gray-200 dark:border-gray-800 bg-white dark:bg-gray-900 p-4">
      <div className="flex items-center gap-2 flex-wrap">
        <IconScale size={16} className="text-yellow-500" />
        <span className="text-sm font-semibold text-gray-900 dark:text-gray-100">
          {v.headline}
        </span>
        <Chip label={v.confidence} className={CONFIDENCE_CLASS[v.confidence]} />
      </div>
      <div className="text-xs text-gray-600 dark:text-gray-400 mt-2 tabular-nums">
        {v.cost_per_outcome_usd !== null && (
          <span>{formatCost(v.cost_per_outcome_usd, currency)} / successful outcome</span>
        )}
        {v.runner_up && <span> · runner-up {v.runner_up}</span>}
      </div>
    </div>
  )
}

function ModelRow({
  row,
  isWinner,
  currency,
}: {
  row: BenchmarkModelRow
  isWinner: boolean
  currency: ReturnType<typeof useCurrency>['currency']
}) {
  const wilson = row.success_rate.ci_wilson
  const cpo = row.cost_per_outcome.point
  const dim = row.qualified ? '' : 'opacity-50'
  return (
    <tr className={`border-t border-gray-200 dark:border-gray-800 ${dim}`}>
      <td className="px-3 py-2 text-xs font-medium text-gray-900 dark:text-gray-100 whitespace-nowrap">
        {isWinner && <IconScale size={12} className="inline mr-1 text-yellow-500" />}
        {row.model}
        {!row.qualified && (
          <span className="ml-2 text-[10px] text-gray-400">insufficient evidence</span>
        )}
      </td>
      <td className="px-3 py-2 text-xs text-gray-600 dark:text-gray-400 text-right tabular-nums">
        {row.n}
      </td>
      <td className="px-3 py-2 text-xs text-gray-600 dark:text-gray-400 text-right tabular-nums whitespace-nowrap">
        {pct(row.success_rate.point)}
        {wilson && (
          <span className="text-gray-400">
            {' '}
            [{pct(wilson[0])}–{pct(wilson[1])}]
          </span>
        )}
      </td>
      <td className="px-3 py-2 text-xs text-gray-600 dark:text-gray-400 text-right tabular-nums whitespace-nowrap">
        {cpo !== null ? `${formatCost(cpo, currency)}/outcome` : '—'}
      </td>
      <td className="px-3 py-2 text-xs text-gray-600 dark:text-gray-400 text-right tabular-nums">
        {pct(row.coverage)}
      </td>
      <td className="px-3 py-2 text-xs font-medium text-gray-900 dark:text-gray-100 text-right tabular-nums">
        {row.composite.toFixed(2)}
      </td>
    </tr>
  )
}

function StratumCard({
  stratum,
  currency,
}: {
  stratum: BenchmarkStratum
  currency: ReturnType<typeof useCurrency>['currency']
}) {
  return (
    <div className="rounded-lg border border-gray-200 dark:border-gray-800 overflow-hidden">
      <div className="flex items-center justify-between gap-2 px-3 py-2 bg-gray-50 dark:bg-gray-800/60">
        <span className="text-xs font-semibold text-gray-800 dark:text-gray-200">
          {stratum.intent} × {stratum.size_band}
        </span>
        <Chip
          label={stratum.cell_verdict}
          className={CELL_VERDICT_CLASS[stratum.cell_verdict]}
        />
      </div>
      <div className="overflow-x-auto">
        <table className="w-full text-sm">
          <thead>
            <tr>
              <th className="px-3 py-1.5 text-left text-[10px] uppercase tracking-wider text-gray-500 font-medium">Model</th>
              <th className="px-3 py-1.5 text-right text-[10px] uppercase tracking-wider text-gray-500 font-medium">n</th>
              <th className="px-3 py-1.5 text-right text-[10px] uppercase tracking-wider text-gray-500 font-medium">Success (90% CI)</th>
              <th className="px-3 py-1.5 text-right text-[10px] uppercase tracking-wider text-gray-500 font-medium">Cost / outcome</th>
              <th className="px-3 py-1.5 text-right text-[10px] uppercase tracking-wider text-gray-500 font-medium">Coverage</th>
              <th className="px-3 py-1.5 text-right text-[10px] uppercase tracking-wider text-gray-500 font-medium">Composite</th>
            </tr>
          </thead>
          <tbody>
            {stratum.models.map(m => (
              <ModelRow
                key={m.model}
                row={m}
                isWinner={m.model === stratum.winner}
                currency={currency}
              />
            ))}
          </tbody>
        </table>
      </div>
    </div>
  )
}

export default function ModelWinsPanel() {
  const { currency } = useCurrency()
  const [period, setPeriod] = useState<BenchmarkPeriod>('all')

  const { data, isLoading, error } = useQuery({
    queryKey: ['benchmark', period],
    queryFn: () => getBenchmark(period),
    staleTime: 60_000,
  })

  const report = data?.report

  return (
    <section className="space-y-3 pt-2 border-t border-gray-200 dark:border-gray-800">
      <div className="flex items-center justify-between gap-3 flex-wrap">
        <div className="flex items-center gap-2">
          <IconScale size={16} className="text-gray-500" />
          <h2 className="text-sm font-semibold text-gray-800 dark:text-gray-200">
            Which model wins
          </h2>
          <span className="text-xs text-gray-500">
            cost per successful outcome, per task type — from your own history
          </span>
        </div>
        <div
          className="inline-flex rounded-md border border-gray-200 dark:border-gray-700 overflow-hidden"
          role="group"
          aria-label="Benchmark period"
        >
          {PERIODS.map(p => (
            <button
              key={p.id}
              type="button"
              onClick={() => setPeriod(p.id)}
              className={`px-3 py-1.5 text-xs font-medium transition-colors ${
                p.id === period
                  ? 'bg-indigo-500/10 text-indigo-600 dark:text-indigo-400'
                  : 'bg-white dark:bg-gray-900 text-gray-600 dark:text-gray-400 hover:text-gray-900 dark:hover:text-gray-200'
              }`}
            >
              {p.label}
            </button>
          ))}
        </div>
      </div>

      {isLoading && <LoadingSpinner message="Analyzing your model history..." />}

      {error && (
        <div className="bg-red-50 dark:bg-red-900/20 border border-red-300 dark:border-red-800 rounded-lg p-3 text-red-700 dark:text-red-400 text-sm">
          Failed to load benchmark: {error instanceof Error ? error.message : 'Unknown error'}
        </div>
      )}

      {!isLoading && !error && report && report.coverage.sessions_total === 0 && (
        <EmptyState
          icon={<IconScale size={28} />}
          title="Not enough history to compare models yet"
          description="Once you've run a few sessions across more than one model, this panel compares them by task type — honestly."
        />
      )}

      {!isLoading && !error && report && report.coverage.sessions_total > 0 && (
        <>
          <MethodBanner report={report} />
          <VerdictCard report={report} currency={currency} />
          {report.strata.length > 0 && (
            <div className="space-y-2">
              <h3 className="text-xs uppercase tracking-wider text-gray-500 font-medium">
                Per task type (intent × size)
              </h3>
              {report.strata.map(s => (
                <StratumCard
                  key={`${s.intent}:${s.size_band}`}
                  stratum={s}
                  currency={currency}
                />
              ))}
            </div>
          )}
        </>
      )}
    </section>
  )
}
