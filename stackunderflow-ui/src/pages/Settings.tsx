import { useState } from 'react'
import { Link } from 'react-router-dom'
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import {
  IconArrowLeft,
  IconMoon,
  IconSun,
  IconCurrencyDollar,
  IconArrowRight,
  IconTrash,
  IconPlus,
  IconRefresh,
  IconAlertTriangle,
} from '@tabler/icons-react'
import { useTheme } from '../hooks/useTheme'
import { useBetaFeatures, type TabVisibility } from '../hooks/useBetaFeatures'
import { useCurrency } from '../services/currency'
import {
  getCurrencies,
  getModelAliases,
  setModelAlias,
  deleteModelAlias,
  getEtlStatus,
  triggerEtlBackfill,
  EtlPipelineNotReadyError,
  EtlBackfillInProgressError,
} from '../services/api'
import BetaBadge from '../components/common/BetaBadge'
import ContextBudgetCard from '../components/settings/ContextBudgetCard'

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
  // v0.6.0 follow-up tabs.
  { id: 'compare', label: 'Compare', isBeta: false },
  { id: 'yield', label: 'Yield', isBeta: true },
  { id: 'commands', label: 'Commands', isBeta: false },
  { id: 'messages', label: 'Messages', isBeta: false },
  { id: 'search', label: 'Search', isBeta: false },
  { id: 'qa', label: 'Q&A', isBeta: true },
  { id: 'tags', label: 'Tags', isBeta: true },
  { id: 'bookmarks', label: 'Bookmarks', isBeta: false },
] as const

// Common currencies always shown in the dropdown. Anything else can be
// typed into the "Other" input. Mirrors the backend's _COMMON_CURRENCIES
// in routes/cfg.py — keep them aligned when editing.
const COMMON_CURRENCIES: string[] = [
  'USD', 'EUR', 'GBP', 'JPY', 'CHF', 'CAD', 'AUD', 'CNY', 'INR',
  'KRW', 'MXN', 'BRL', 'SEK', 'NOK', 'DKK', 'PLN', 'RUB', 'TRY',
  'ZAR', 'AED', 'SAR', 'SGD', 'HKD', 'NZD',
]

function CurrencySection() {
  const { currency, isLoading, setCurrencyCode } = useCurrency()
  const { data: catalogs } = useQuery({
    queryKey: ['currencies'],
    queryFn: getCurrencies,
    staleTime: 60 * 60_000, // FX list is stable enough to cache for an hour
  })
  const [otherCode, setOtherCode] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [pending, setPending] = useState(false)

  const current = currency?.code ?? 'USD'
  const isOther = !COMMON_CURRENCIES.includes(current)

  const handleSelect = async (code: string) => {
    setError(null)
    if (code === 'OTHER') return
    setPending(true)
    try {
      await setCurrencyCode(code)
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Failed to set currency')
    } finally {
      setPending(false)
    }
  }

  const handleOtherSubmit = async () => {
    setError(null)
    const code = otherCode.trim().toUpperCase()
    if (!/^[A-Z]{3}$/.test(code)) {
      setError('Enter a 3-letter ISO 4217 code (e.g. EUR, GBP, JPY).')
      return
    }
    setPending(true)
    try {
      await setCurrencyCode(code)
      setOtherCode('')
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Failed to set currency')
    } finally {
      setPending(false)
    }
  }

  return (
    <section className="bg-white dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-800 p-5">
      <h2 className="text-base font-semibold text-gray-900 dark:text-gray-100">Currency</h2>
      <p className="text-xs text-gray-500 mt-1">
        Costs are computed in USD using model rate cards, then converted on the fly via Frankfurter
        (ECB) rates. Cached for 24h; falls back to USD if the network is unavailable.
      </p>

      <div className="mt-4 flex items-center gap-3">
        <IconCurrencyDollar size={18} className="text-gray-600 dark:text-gray-400 flex-shrink-0" />
        <div className="flex-1 min-w-0">
          <div className="text-sm font-medium text-gray-900 dark:text-gray-100">
            Active: <span className="font-mono">{current}</span>{' '}
            <span className="text-gray-500">({currency?.symbol ?? '$'})</span>
          </div>
          <div className="text-xs text-gray-500">
            {isLoading
              ? 'Loading…'
              : currency?.rate_from_usd === 1
                ? 'No conversion (USD-equivalent)'
                : `1 USD ≈ ${currency?.rate_from_usd?.toFixed(4) ?? '—'} ${current}`}
          </div>
        </div>
        <select
          value={isOther ? 'OTHER' : current}
          onChange={e => handleSelect(e.target.value)}
          disabled={pending}
          className="bg-white dark:bg-gray-800 border border-gray-300 dark:border-gray-700 rounded px-2 py-1.5 text-sm text-gray-700 dark:text-gray-300 focus:outline-none focus:border-indigo-500 disabled:opacity-50"
          aria-label="Active currency"
        >
          {COMMON_CURRENCIES.map(code => (
            <option key={code} value={code}>{code}</option>
          ))}
          <option value="OTHER">Other (any 3-letter ISO)…</option>
        </select>
      </div>

      {(isOther || otherCode.length > 0) && (
        <div className="mt-3 flex items-center gap-2">
          <input
            type="text"
            value={otherCode}
            onChange={e => setOtherCode(e.target.value.toUpperCase())}
            placeholder={isOther ? current : 'e.g. CZK'}
            maxLength={3}
            className="bg-white dark:bg-gray-800 border border-gray-300 dark:border-gray-700 rounded px-2 py-1.5 text-sm text-gray-700 dark:text-gray-300 placeholder-gray-500 focus:outline-none focus:border-indigo-500 font-mono w-24"
            aria-label="Custom ISO 4217 currency code"
          />
          <button
            type="button"
            onClick={handleOtherSubmit}
            disabled={pending || otherCode.length !== 3}
            className="px-3 py-1.5 text-sm rounded border border-gray-300 dark:border-gray-700 bg-white dark:bg-gray-800 text-gray-700 dark:text-gray-200 hover:border-gray-400 dark:hover:border-gray-600 disabled:opacity-50"
          >
            Apply
          </button>
          <span className="text-[11px] text-gray-500">
            {catalogs?.supported.length
              ? `Supported: ${catalogs.supported.length} codes (cached)`
              : 'Frankfurter cache empty — fetched on first conversion.'}
          </span>
        </div>
      )}

      {error && <div className="mt-3 text-xs text-red-600 dark:text-red-400">{error}</div>}
    </section>
  )
}

function ModelAliasSection() {
  const queryClient = useQueryClient()
  const aliasesQuery = useQuery({
    queryKey: ['modelAliases'],
    queryFn: getModelAliases,
  })
  const aliases = aliasesQuery.data?.aliases ?? {}

  const [draftFrom, setDraftFrom] = useState('')
  const [draftTo, setDraftTo] = useState('')
  const [error, setError] = useState<string | null>(null)

  const addMutation = useMutation({
    mutationFn: ({ from, to }: { from: string; to: string }) => setModelAlias(from, to),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['modelAliases'] })
      // Aliases affect cost lookup on every project — invalidate dashboard
      // queries so existing tabs re-fetch with the new mapping.
      queryClient.invalidateQueries({ queryKey: ['dashboardData'] })
      setDraftFrom('')
      setDraftTo('')
      setError(null)
    },
    onError: (e: unknown) => {
      setError(e instanceof Error ? e.message : 'Failed to add alias')
    },
  })

  const deleteMutation = useMutation({
    mutationFn: (from: string) => deleteModelAlias(from),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['modelAliases'] })
      queryClient.invalidateQueries({ queryKey: ['dashboardData'] })
    },
  })

  const handleAdd = () => {
    setError(null)
    const from = draftFrom.trim()
    const to = draftTo.trim()
    if (!from || !to) {
      setError('Both fields are required.')
      return
    }
    addMutation.mutate({ from, to })
  }

  return (
    <section className="bg-white dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-800 p-5">
      <h2 className="text-base font-semibold text-gray-900 dark:text-gray-100">Model aliases</h2>
      <p className="text-xs text-gray-500 mt-1">
        Map a proxy-rewritten model id (e.g. <span className="font-mono">openrouter/claude-opus</span>)
        to a canonical id we have rate-card data for, so costs come out non-zero.
      </p>

      {/* Existing aliases */}
      <div className="mt-4">
        {aliasesQuery.isLoading ? (
          <div className="text-xs text-gray-500 py-3">Loading aliases…</div>
        ) : Object.keys(aliases).length === 0 ? (
          <div className="text-xs text-gray-500 py-3 italic">No aliases configured yet.</div>
        ) : (
          <div className="overflow-hidden rounded border border-gray-200 dark:border-gray-800">
            <table className="w-full text-sm">
              <thead className="bg-gray-50 dark:bg-gray-800/60 text-[10px] uppercase tracking-wider text-gray-500">
                <tr>
                  <th className="text-left px-3 py-2">From (proxy id)</th>
                  <th className="px-2 py-2 w-6" aria-hidden="true" />
                  <th className="text-left px-3 py-2">To (canonical id)</th>
                  <th className="px-3 py-2 w-12" aria-label="Actions" />
                </tr>
              </thead>
              <tbody>
                {Object.entries(aliases).sort(([a], [b]) => a.localeCompare(b)).map(([from, to]) => (
                  <tr
                    key={from}
                    className="border-t border-gray-200 dark:border-gray-800"
                  >
                    <td className="px-3 py-2 font-mono text-xs text-gray-800 dark:text-gray-200 break-all">{from}</td>
                    <td className="px-2 py-2 text-gray-400">
                      <IconArrowRight size={12} />
                    </td>
                    <td className="px-3 py-2 font-mono text-xs text-gray-800 dark:text-gray-200 break-all">{to}</td>
                    <td className="px-3 py-2 text-right">
                      <button
                        type="button"
                        onClick={() => deleteMutation.mutate(from)}
                        disabled={deleteMutation.isPending}
                        className="text-gray-500 hover:text-red-500 disabled:opacity-50"
                        title={`Remove alias ${from}`}
                        aria-label={`Remove alias ${from}`}
                      >
                        <IconTrash size={14} />
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>

      {/* Add new */}
      <div className="mt-4 grid grid-cols-1 sm:grid-cols-[1fr_auto_1fr_auto] gap-2 items-center">
        <input
          type="text"
          value={draftFrom}
          onChange={e => setDraftFrom(e.target.value)}
          placeholder="openrouter/claude-opus"
          className="bg-white dark:bg-gray-800 border border-gray-300 dark:border-gray-700 rounded px-2 py-1.5 text-xs text-gray-700 dark:text-gray-300 placeholder-gray-500 focus:outline-none focus:border-indigo-500 font-mono"
          aria-label="Proxy model id"
        />
        <IconArrowRight size={14} className="text-gray-500 hidden sm:block" aria-hidden="true" />
        <input
          type="text"
          value={draftTo}
          onChange={e => setDraftTo(e.target.value)}
          placeholder="claude-opus-4-6"
          className="bg-white dark:bg-gray-800 border border-gray-300 dark:border-gray-700 rounded px-2 py-1.5 text-xs text-gray-700 dark:text-gray-300 placeholder-gray-500 focus:outline-none focus:border-indigo-500 font-mono"
          aria-label="Canonical model id"
        />
        <button
          type="button"
          onClick={handleAdd}
          disabled={addMutation.isPending || !draftFrom.trim() || !draftTo.trim()}
          className="inline-flex items-center gap-1 px-3 py-1.5 text-xs rounded border border-gray-300 dark:border-gray-700 bg-white dark:bg-gray-800 text-gray-700 dark:text-gray-200 hover:border-gray-400 dark:hover:border-gray-600 disabled:opacity-50"
        >
          <IconPlus size={12} />
          Add
        </button>
      </div>

      {error && <div className="mt-2 text-xs text-red-600 dark:text-red-400">{error}</div>}
    </section>
  )
}

// ---------------------------------------------------------------------------
// Wave 4F — ETL pipeline section.
//
// Read-only summary (event counts + per-mart row counts) plus a "Backfill now"
// button that POSTs `/api/etl/backfill`. If the route is missing (Wave 4B has
// not exposed it yet) the button surfaces the equivalent CLI command. A
// confirmation dialog warns about the runtime before triggering.
// ---------------------------------------------------------------------------

function EtlPipelineSection() {
  const queryClient = useQueryClient()
  const { data, error, isLoading } = useQuery({
    queryKey: ['etl-status'],
    queryFn: getEtlStatus,
    // Match the badge's 10s refresh — they share a key so the popover and
    // settings stay consistent.
    refetchInterval: 10_000,
    staleTime: 5_000,
    retry: (count, err) => (err instanceof EtlPipelineNotReadyError ? false : count < 2),
  })
  const [confirming, setConfirming] = useState(false)
  const [feedback, setFeedback] = useState<{ kind: 'ok' | 'warn' | 'error'; message: string } | null>(
    null,
  )

  const backfillMutation = useMutation({
    mutationFn: () => triggerEtlBackfill(false),
    onSuccess: (res) => {
      // 202 Accepted: the orchestrator is running on the server. The
      // dashboard polls /api/etl/status every 10s; the badge + this
      // section will reflect the in-progress state on the next tick.
      setFeedback({
        kind: 'ok',
        message: `Backfill started (job ${res.job_id.slice(0, 8)}…). The dashboard will refresh as it progresses.`,
      })
      queryClient.invalidateQueries({ queryKey: ['etl-status'] })
    },
    onError: (err: unknown) => {
      if (err instanceof EtlBackfillInProgressError) {
        // 409 Conflict — surface the running job_id so the user can
        // tell whether this is "their" backfill from a prior session.
        setFeedback({
          kind: 'warn',
          message: `A backfill is already running (job ${err.jobId.slice(0, 8)}…). Wait for it to finish before triggering another.`,
        })
        // Refresh the status query so the in-progress state surfaces
        // in the table + badge without waiting for the next 10s tick.
        queryClient.invalidateQueries({ queryKey: ['etl-status'] })
        return
      }
      if (err instanceof EtlPipelineNotReadyError) {
        setFeedback({
          kind: 'warn',
          message:
            'Backfill API not available on this build. Run `stackunderflow etl backfill` from the CLI to refresh manually.',
        })
        return
      }
      setFeedback({
        kind: 'error',
        message: err instanceof Error ? err.message : 'Backfill failed.',
      })
    },
  })

  // The status payload tells us whether a job is in flight regardless
  // of whether *this* tab kicked it off. Reflect that in the button so
  // a second tab doesn't lie about the system state.
  const isJobRunning =
    backfillMutation.isPending || (!!data?.current_job && data.current_job.status === 'running')

  const handleConfirm = () => {
    setConfirming(false)
    setFeedback(null)
    backfillMutation.mutate()
  }

  const notReady = error instanceof EtlPipelineNotReadyError

  return (
    <section className="bg-white dark:bg-gray-900 rounded-lg border border-gray-200 dark:border-gray-800 p-5">
      <h2 className="text-base font-semibold text-gray-900 dark:text-gray-100">ETL pipeline</h2>
      <p className="text-xs text-gray-500 mt-1">
        Watcher status + mart watermarks + a manual backfill trigger. The watcher refreshes
        marts incrementally as new messages land; the backfill rebuilds them from scratch
        (can take minutes on large stores).
      </p>

      {/* Status summary */}
      <div className="mt-4 grid grid-cols-1 sm:grid-cols-3 gap-3">
        <StatusTile
          label="Health"
          value={notReady ? '—' : data?.health ?? (isLoading ? '…' : 'unknown')}
          mono
        />
        <StatusTile
          label="Events"
          value={notReady ? '—' : data ? data.events.total.toLocaleString() : isLoading ? '…' : '—'}
        />
        <StatusTile
          label="Watcher"
          value={
            notReady
              ? '—'
              : data
                ? data.watcher.enabled
                  ? data.watcher.running
                    ? 'running'
                    : 'idle'
                  : 'disabled'
                : isLoading
                  ? '…'
                  : '—'
          }
        />
      </div>

      {/* Per-mart breakdown */}
      {!notReady && data && Object.keys(data.marts).length > 0 && (
        <div className="mt-4 overflow-hidden rounded border border-gray-200 dark:border-gray-800">
          <table className="w-full text-sm">
            <thead className="bg-gray-50 dark:bg-gray-800/60 text-[10px] uppercase tracking-wider text-gray-500">
              <tr>
                <th className="text-left px-3 py-2">Mart</th>
                <th className="text-right px-3 py-2">Watermark</th>
                <th className="text-right px-3 py-2">Rows</th>
              </tr>
            </thead>
            <tbody>
              {Object.entries(data.marts)
                .sort(([a], [b]) => a.localeCompare(b))
                .map(([name, mart]) => (
                  <tr key={name} className="border-t border-gray-200 dark:border-gray-800">
                    <td className="px-3 py-2 font-mono text-xs text-gray-800 dark:text-gray-200">{name}</td>
                    <td className="px-3 py-2 text-right font-mono text-xs tabular-nums text-gray-700 dark:text-gray-300">
                      {mart.watermark.toLocaleString()} / {data.events.max_id.toLocaleString()}
                    </td>
                    <td className="px-3 py-2 text-right font-mono text-xs tabular-nums text-gray-700 dark:text-gray-300">
                      {mart.row_count.toLocaleString()}
                    </td>
                  </tr>
                ))}
            </tbody>
          </table>
        </div>
      )}

      {notReady && (
        <div className="mt-4 px-3 py-2 rounded border border-gray-300 dark:border-gray-700 bg-gray-50 dark:bg-gray-800/40 text-xs text-gray-600 dark:text-gray-400">
          The <span className="font-mono">/api/etl/status</span> route is not available on this build.
          Run the CLI to manage the ETL pipeline:
          <pre className="mt-1.5 px-2 py-1.5 rounded bg-white dark:bg-gray-900 border border-gray-200 dark:border-gray-700 font-mono text-[11px] text-gray-800 dark:text-gray-200">
            stackunderflow etl backfill
          </pre>
        </div>
      )}

      {/* In-progress banner — surfaces a job started by another tab / curl
          / the CLI as well as one we kicked off ourselves. */}
      {data?.current_job && data.current_job.status === 'running' && (
        <div className="mt-4 px-3 py-2 rounded border border-blue-300 dark:border-blue-800 bg-blue-50 dark:bg-blue-900/20 text-xs text-blue-800 dark:text-blue-300 flex items-center gap-2">
          <IconRefresh size={12} className="animate-spin" />
          <span>
            Backfill <span className="font-mono">{data.current_job.job_id.slice(0, 8)}…</span> is
            running
            {data.current_job.force ? ' (force rebuild)' : ''}. Started{' '}
            {data.current_job.started_at}.
          </span>
        </div>
      )}

      {/* Backfill action */}
      <div className="mt-4 flex items-center justify-between gap-3">
        <div className="text-xs text-gray-500">
          Force a full rebuild of every mart from <span className="font-mono">usage_events</span>.
          Use this if the dashboard looks out of sync.
        </div>
        <button
          type="button"
          onClick={() => setConfirming(true)}
          disabled={isJobRunning}
          className="inline-flex items-center gap-1.5 px-3 py-1.5 text-xs rounded border border-gray-300 dark:border-gray-700 bg-white dark:bg-gray-800 text-gray-700 dark:text-gray-200 hover:border-gray-400 dark:hover:border-gray-600 disabled:opacity-50 shrink-0"
        >
          <IconRefresh size={12} className={isJobRunning ? 'animate-spin' : ''} />
          {isJobRunning ? 'Backfilling…' : 'Backfill now'}
        </button>
      </div>

      {/* Confirmation dialog */}
      {confirming && (
        <div className="mt-3 rounded border border-yellow-300 dark:border-yellow-800 bg-yellow-50 dark:bg-yellow-900/20 p-3 text-xs">
          <div className="flex items-start gap-2">
            <IconAlertTriangle size={14} className="text-yellow-700 dark:text-yellow-400 mt-0.5 shrink-0" />
            <div className="flex-1">
              <div className="font-medium text-yellow-800 dark:text-yellow-300">
                Run a full backfill?
              </div>
              <div className="text-yellow-700 dark:text-yellow-400 mt-1">
                Rebuilds every mart from scratch. On large stores this can take minutes
                while the watcher is paused. Existing dashboard queries continue to work
                throughout.
              </div>
              <div className="mt-2 flex items-center gap-2">
                <button
                  type="button"
                  onClick={handleConfirm}
                  className="px-2.5 py-1 rounded border border-yellow-500 dark:border-yellow-700 bg-yellow-500 dark:bg-yellow-700 text-white text-xs hover:bg-yellow-600 dark:hover:bg-yellow-600"
                >
                  Yes, backfill
                </button>
                <button
                  type="button"
                  onClick={() => setConfirming(false)}
                  className="px-2.5 py-1 rounded border border-gray-300 dark:border-gray-700 bg-white dark:bg-gray-800 text-gray-700 dark:text-gray-200 text-xs hover:border-gray-400 dark:hover:border-gray-600"
                >
                  Cancel
                </button>
              </div>
            </div>
          </div>
        </div>
      )}

      {/* Feedback banner */}
      {feedback && (
        <div
          className={`mt-3 px-3 py-2 rounded border text-xs ${
            feedback.kind === 'ok'
              ? 'border-green-300 dark:border-green-800 bg-green-50 dark:bg-green-900/20 text-green-700 dark:text-green-300'
              : feedback.kind === 'warn'
                ? 'border-yellow-300 dark:border-yellow-800 bg-yellow-50 dark:bg-yellow-900/20 text-yellow-800 dark:text-yellow-300'
                : 'border-red-300 dark:border-red-800 bg-red-50 dark:bg-red-900/20 text-red-700 dark:text-red-400'
          }`}
        >
          {feedback.message}
        </div>
      )}
    </section>
  )
}

interface StatusTileProps {
  label: string
  value: string
  mono?: boolean
}

function StatusTile({ label, value, mono = false }: StatusTileProps) {
  return (
    <div className="rounded border border-gray-200 dark:border-gray-800 px-3 py-2 bg-gray-50 dark:bg-gray-800/40">
      <div className="text-[10px] uppercase tracking-wider text-gray-500">{label}</div>
      <div
        className={`text-sm text-gray-900 dark:text-gray-100 ${mono ? 'font-mono' : ''}`}
      >
        {value}
      </div>
    </div>
  )
}

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
          Customize appearance, currency, model aliases, and which dashboard tabs are visible.
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

      {/* 2. Currency ----------------------------------------------------- */}
      <CurrencySection />

      {/* 3. Model aliases ------------------------------------------------ */}
      <ModelAliasSection />

      {/* 4. Beta features ------------------------------------------------ */}
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

      {/* 5. Tab visibility ----------------------------------------------- */}
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

      {/* 6. Context budget (v0.6.0) ------------------------------------- */}
      <ContextBudgetCard />

      {/* 7. ETL pipeline (Wave 4F) -------------------------------------- */}
      <EtlPipelineSection />

      {/* 8. Danger zone / reset ------------------------------------------ */}
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
