import { useCallback, useEffect, useMemo, useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { useParams } from 'react-router-dom'
import {
  IconAlertTriangle,
  IconCalendar,
  IconFilter,
  IconRefresh,
  IconTool,
  IconX,
} from '@tabler/icons-react'
import type {
  CommandCost,
  ErrorCost,
  Outliers,
  RetrySignal,
  SessionCost,
  TokenComposition,
  ToolCost,
  Trends,
} from '../../types/api'
import { getParam, openInteraction, openSession } from '../../services/navigation'
import { useFilters } from '../../services/filters'
import { formatModelName } from '../../services/format'
import TrendDeltaStrip from '../cost/TrendDeltaStrip'
import CacheRoiCard from '../cost/CacheRoiCard'
import CostByProviderCard from '../cost/CostByProviderCard'
import ErrorCostCard from '../cost/ErrorCostCard'
import SessionCostBarChart from '../cost/SessionCostBarChart'
import CommandCostList from '../cost/CommandCostList'
import ToolCostBarChart from '../cost/ToolCostBarChart'
import TokenCompositionDonut from '../cost/TokenCompositionDonut'
import TokenCompositionStack from '../cost/TokenCompositionStack'
import ByModelCostChart, { type ByModelPeriod } from '../cost/ByModelCostChart'
import OutlierCommandsTable from '../cost/OutlierCommandsTable'
import RetryAlertsPanel from '../cost/RetryAlertsPanel'

// ---------------------------------------------------------------------------
// CostTab — analytics-polish spec §C21
//
// Lazy-loads the heavy cost analytics payload from the split endpoint
// `/api/cost-data` on mount, instead of consuming `stats` from the parent.
// Hosts an inline FilterBar (range + session + tool) that listens for the
// `stackunderflow:filter-window` and `stackunderflow:filter-tool` custom
// events emitted by Wave-B components and applies the filters client-side
// before passing sliced data down to each chart/panel.
//
// Callbacks from Wave-B components:
//   - CommandCostList.onOpen      → openInteraction()
//   - SessionCostBarChart.onSelect → openSession()
//   - OutlierCommandsTable.onOpen → openInteraction()
// All three go through services/navigation so they push URL state and fire
// `stackunderflow:nav` for cross-tab coordination.
// ---------------------------------------------------------------------------

// Accept `stats?` for backwards-compat with the old prop-based wiring. When
// omitted we fetch our own data. Kept optional so ProjectDashboard can keep
// passing it until the C25 router work lands.
interface CostTabProps {
  stats?: { cache?: unknown; overview?: { total_tokens?: Record<string, number> } }
}

type RangeKey = '7d' | '30d' | 'all'

interface FilterState {
  range: RangeKey
  sessionFilter: string | null
  // Not in the minimal spec state, but required for the filter-tool event
  // listener — see the C21 task description. Kept nullable so the chart's
  // empty-data branch fires when cleared.
  toolFilter: string | null
}

interface CostData {
  session_costs: SessionCost[]
  command_costs: CommandCost[]
  tool_costs: Record<string, ToolCost>
  token_composition: TokenComposition | Record<string, never>
  outliers: Outliers | Record<string, never>
  retry_signals: RetrySignal[]
  session_efficiency: unknown[]
  error_cost: ErrorCost | Record<string, never>
  trends: Trends | Record<string, never>
  // Cache may not be on /api/cost-data (it lives on dashboard-data), so we
  // accept it from the optional `stats` prop as a fallback.
  cache?: unknown
}

// ── helpers ─────────────────────────────────────────────────────────────────

/**
 * #10: the heavy `/api/cost-data` payload is now fetched through React Query
 * (see the `useQuery` call below) so re-visiting the Cost tab within the
 * 30s staleTime serves the cached payload instead of re-downloading it.
 * The server resolves `log_path` from `deps.current_log_path`, so the project
 * identity is folded into the query key.
 *
 * #57: the active model filter is forwarded as repeated `?model=` params so the
 * server narrows the `token_composition` blocks (donut/stack) to the selected
 * model(s) — broadening the filter past the by-model cost chart. `models` is
 * empty for "all models" (the existing behaviour).
 */
async function fetchCostData(models: string[]): Promise<CostData> {
  const params = new URLSearchParams()
  for (const m of models) {
    if (m && m.trim()) params.append('model', m.toLowerCase().trim())
  }
  const qs = params.toString()
  const res = await fetch(`/api/cost-data${qs ? `?${qs}` : ''}`)
  if (!res.ok) {
    const text = await res.text().catch(() => '')
    throw new Error(`${res.status} ${res.statusText}${text ? `: ${text}` : ''}`)
  }
  return res.json() as Promise<CostData>
}

function parseTs(iso: string | undefined | null): number | null {
  if (!iso) return null
  const t = Date.parse(iso)
  return Number.isNaN(t) ? null : t
}

/** §D4: coerce an arbitrary URL string to a valid RangeKey, default 'all'. */
function parseRange(raw: string | null): RangeKey {
  return raw === '7d' || raw === '30d' || raw === 'all' ? raw : 'all'
}

/** §D4: sync filter state to the URL via `history.replaceState` — never
 * `pushState`, so filter toggles don't create back-button traps. Only
 * non-default params are emitted (`range=all` is the default → omitted).
 * Leaves any unrelated params (`tab`, `interaction`, …) untouched. */
function replaceFilterParams(range: RangeKey, sessionFilter: string | null, toolFilter: string | null): void {
  if (typeof window === 'undefined' || typeof window.history === 'undefined') return
  const url = new URL(window.location.href)
  if (range === 'all') {
    url.searchParams.delete('range')
  } else {
    url.searchParams.set('range', range)
  }
  if (sessionFilter) {
    url.searchParams.set('session', sessionFilter)
  } else {
    url.searchParams.delete('session')
  }
  if (toolFilter) {
    url.searchParams.set('tool', toolFilter)
  } else {
    url.searchParams.delete('tool')
  }
  const next = `${url.pathname}${url.search}${url.hash}`
  const current = `${window.location.pathname}${window.location.search}${window.location.hash}`
  if (next !== current) {
    window.history.replaceState({}, '', next)
  }
}

/** Derive the filter-window cutoff epoch-ms for a given range. Uses the
 * most-recent timestamp in `sessions`/`commands` as the anchor so inactive
 * projects don't vanish; falls back to `Date.now()` when empty. */
function computeCutoff(range: RangeKey, sessions: SessionCost[], commands: CommandCost[]): number | null {
  if (range === 'all') return null
  const stamps: number[] = []
  for (const s of sessions) {
    const e = parseTs(s.ended_at) ?? parseTs(s.started_at)
    if (e !== null) stamps.push(e)
  }
  for (const c of commands) {
    const t = parseTs(c.timestamp)
    if (t !== null) stamps.push(t)
  }
  const anchor = stamps.length ? Math.max(...stamps) : Date.now()
  const days = range === '7d' ? 7 : 30
  return anchor - days * 24 * 60 * 60 * 1000
}

// ── inline FilterBar ────────────────────────────────────────────────────────

interface FilterBarProps {
  filter: FilterState
  onRangeChange: (r: RangeKey) => void
  onClearSession: () => void
  onClearTool: () => void
}

function FilterBar({ filter, onRangeChange, onClearSession, onClearTool }: FilterBarProps) {
  const ranges: { key: RangeKey; label: string }[] = [
    { key: '7d', label: 'Last 7 days' },
    { key: '30d', label: 'Last 30 days' },
    { key: 'all', label: 'All time' },
  ]
  return (
    <div className="flex flex-wrap items-center gap-3 bg-gray-100/40 dark:bg-gray-800/40 border border-gray-200 dark:border-gray-800 rounded-lg px-3 py-2">
      <div className="flex items-center gap-1.5 text-gray-500">
        <IconFilter size={13} />
        <span className="text-[11px] uppercase tracking-wider">Filter</span>
      </div>
      <div className="flex items-center gap-1 text-xs">
        <IconCalendar size={12} className="text-gray-500" />
        {ranges.map((r) => (
          <button
            key={r.key}
            onClick={() => onRangeChange(r.key)}
            className={`px-2 py-0.5 rounded text-[11px] transition-colors ${
              filter.range === r.key
                ? 'bg-indigo-500/20 text-indigo-300 border border-indigo-500/40'
                : 'text-gray-600 dark:text-gray-400 hover:text-gray-800 dark:hover:text-gray-200 border border-transparent'
            }`}
          >
            {r.label}
          </button>
        ))}
      </div>
      {filter.sessionFilter && (
        <button
          onClick={onClearSession}
          className="flex items-center gap-1 text-[11px] px-2 py-0.5 rounded bg-amber-500/15 text-amber-300 border border-amber-500/30 hover:bg-amber-500/25"
          title="Clear session filter"
        >
          Session: <span className="font-mono">{filter.sessionFilter.slice(0, 8)}</span>
          <IconX size={11} />
        </button>
      )}
      {filter.toolFilter && (
        <button
          onClick={onClearTool}
          className="flex items-center gap-1 text-[11px] px-2 py-0.5 rounded bg-purple-500/15 text-purple-300 border border-purple-500/30 hover:bg-purple-500/25"
          title="Clear tool filter"
        >
          <IconTool size={11} /> {filter.toolFilter}
          <IconX size={11} />
        </button>
      )}
    </div>
  )
}

// ── skeleton ────────────────────────────────────────────────────────────────

function Skeleton() {
  const row = 'bg-gray-100/40 dark:bg-gray-800/40 rounded-lg border border-gray-200 dark:border-gray-800 animate-pulse'
  return (
    <div className="space-y-6" aria-busy="true" aria-label="Loading cost analytics">
      <div className={`${row} h-20`} />
      <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
        <div className={`${row} h-40`} />
        <div className={`${row} h-40`} />
      </div>
      <div className={`${row} h-64`} />
      <div className={`${row} h-80`} />
      <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
        <div className={`${row} h-64`} />
        <div className={`${row} h-64`} />
      </div>
      <div className={`${row} h-48`} />
      <div className={`${row} h-40`} />
    </div>
  )
}

// ── error state ─────────────────────────────────────────────────────────────

interface ErrorBannerProps {
  message: string
  onRetry: () => void
}
function ErrorBanner({ message, onRetry }: ErrorBannerProps) {
  return (
    <div className="bg-red-50 dark:bg-red-900/20 border border-red-300 dark:border-red-800 rounded-lg p-4 flex items-center justify-between gap-3">
      <div className="flex items-start gap-2 min-w-0">
        <IconAlertTriangle size={16} className="text-red-600 dark:text-red-400 mt-0.5 shrink-0" />
        <div className="min-w-0">
          <div className="text-sm text-red-700 dark:text-red-300 font-medium">Failed to load cost analytics</div>
          <div className="text-xs text-red-700/80 dark:text-red-400/80 truncate">{message}</div>
        </div>
      </div>
      <button
        onClick={onRetry}
        className="flex items-center gap-1.5 px-3 py-1.5 bg-white dark:bg-gray-800 border border-gray-200 dark:border-gray-700 rounded text-xs text-gray-800 dark:text-gray-200 hover:bg-gray-200 dark:hover:bg-gray-700 shrink-0"
      >
        <IconRefresh size={12} /> Retry
      </button>
    </div>
  )
}

// ── main component ──────────────────────────────────────────────────────────

export default function CostTab({ stats }: CostTabProps) {
  // #10: route the heavy payload through React Query. `name` (the project
  // slug from the router) scopes the cache key so switching projects doesn't
  // serve the previous project's cost data from cache. staleTime mirrors the
  // app-wide default (main.tsx) — re-visiting within 30s is a cache hit.
  const { name } = useParams<{ name: string }>()
  // #57: the dashboard-wide model filter must scope this tab's
  // token_composition (not just the by-model cost chart). Thread the active
  // model set into the fetch + query key so a chip toggle refetches the
  // model-narrowed payload.
  const { filters } = useFilters()
  const modelFilter = filters.models
  const {
    data,
    isLoading: loading,
    error: queryError,
    refetch,
  } = useQuery({
    queryKey: ['costData', name ?? null, modelFilter],
    queryFn: () => fetchCostData(modelFilter),
    staleTime: 30_000,
  })
  const error = queryError instanceof Error ? queryError.message : queryError ? String(queryError) : null
  const reload = useCallback(() => {
    void refetch()
  }, [refetch])
  // §D4: seed from URL so a reload / shared link restores filter state.
  // Params: ?range=7d|30d|all · ?session=<id> · ?tool=<name>
  const [filter, setFilter] = useState<FilterState>(() => ({
    range: parseRange(getParam('range')),
    sessionFilter: getParam('session'),
    toolFilter: getParam('tool'),
  }))

  // §D4: mirror filter state into the URL via `replaceState` (never push),
  // so filter toggles don't balloon the history stack or trap back-nav.
  useEffect(() => {
    replaceFilterParams(filter.range, filter.sessionFilter, filter.toolFilter)
  }, [filter.range, filter.sessionFilter, filter.toolFilter])

  // Listen for window/tool filter events from Wave-B components
  useEffect(() => {
    const onFilterWindow = (ev: Event) => {
      const detail = (ev as CustomEvent<{ window?: string; metric?: string }>).detail
      // TrendDeltaStrip currently emits {window: 'current-week'} — map to '7d'.
      if (detail?.window === 'current-week') {
        setFilter((f) => ({ ...f, range: '7d' }))
      } else if (detail?.window === 'prior-week') {
        // There's no "prior 7d" range — fall back to 30d which includes it.
        setFilter((f) => ({ ...f, range: '30d' }))
      }
    }
    const onFilterTool = (ev: Event) => {
      const detail = (ev as CustomEvent<{ tool?: string }>).detail
      if (detail?.tool) {
        setFilter((f) => ({ ...f, toolFilter: detail.tool as string }))
      }
    }
    window.addEventListener('stackunderflow:filter-window', onFilterWindow)
    window.addEventListener('stackunderflow:filter-tool', onFilterTool)
    return () => {
      window.removeEventListener('stackunderflow:filter-window', onFilterWindow)
      window.removeEventListener('stackunderflow:filter-tool', onFilterTool)
    }
  }, [])

  // ── derived / filtered data ───────────────────────────────────────────────

  const filtered = useMemo(() => {
    if (!data) return null
    const sessions = data.session_costs ?? []
    const commands = data.command_costs ?? []
    const retries = data.retry_signals ?? []
    const cutoff = computeCutoff(filter.range, sessions, commands)

    const inWindow = (ts: string | undefined | null): boolean => {
      if (cutoff === null) return true
      const t = parseTs(ts)
      return t === null ? true : t >= cutoff
    }

    const sessionMatch = (sid: string | undefined): boolean =>
      !filter.sessionFilter || sid === filter.sessionFilter

    // tool filter narrows commands to those that actually used the tool
    // (CommandCost doesn't carry tool names; retry_signals do). For commands
    // we fall back to showing all when no tool-name data is available — the
    // filter pill stays visible so users understand why the chart hasn't
    // collapsed to zero rows.
    const filteredSessions = sessions.filter(
      (s) => inWindow(s.ended_at ?? s.started_at) && sessionMatch(s.session_id),
    )
    const filteredCommands = commands.filter(
      (c) => inWindow(c.timestamp) && sessionMatch(c.session_id),
    )
    const filteredRetries = retries.filter(
      (r) =>
        inWindow(r.timestamp) &&
        sessionMatch(r.session_id) &&
        (!filter.toolFilter || r.tool === filter.toolFilter),
    )

    // Tool cost — when a tool filter is active, narrow to just that tool.
    const toolCosts: Record<string, ToolCost> = filter.toolFilter
      ? data.tool_costs && filter.toolFilter in data.tool_costs
        ? { [filter.toolFilter]: data.tool_costs[filter.toolFilter]! }
        : {}
      : data.tool_costs ?? {}

    // #32: the Token Composition donut must reflect the active filter rather
    // than always showing all-time totals. When a range/session filter is on
    // and the payload carries `per_session` token maps, sum them over the
    // sessions left after windowing. Otherwise fall back to the all-time
    // `totals` (or legacy overview.total_tokens when the section is absent).
    const tokenComp = data.token_composition as TokenComposition | undefined
    const overviewTokens = stats?.overview?.total_tokens
    const allTimeTotals: Record<string, number> =
      tokenComp?.totals ??
      (overviewTokens
        ? {
            input: overviewTokens.input ?? 0,
            output: overviewTokens.output ?? 0,
            cache_read: overviewTokens.cache_read ?? 0,
            cache_creation: overviewTokens.cache_creation ?? 0,
          }
        : {})
    const isScoped = filter.range !== 'all' || !!filter.sessionFilter
    let tokenTotals = allTimeTotals
    if (isScoped && tokenComp?.per_session) {
      const summed: Record<string, number> = {}
      for (const s of filteredSessions) {
        const m = tokenComp.per_session[s.session_id]
        if (!m) continue
        for (const [k, v] of Object.entries(m)) summed[k] = (summed[k] ?? 0) + (v ?? 0)
      }
      tokenTotals = summed
    }

    return {
      sessions: filteredSessions,
      commands: filteredCommands,
      retries: filteredRetries,
      tools: toolCosts,
      tokenTotals,
    }
  }, [data, filter, stats])

  // #48/#63: CacheRoiCard renders a ROI sparkline from per-day cache token
  // ratios, but CostData has no `daily_stats` field — the per-day token maps
  // live under `token_composition.daily`. Re-shape them into the
  // `{date: {tokens: {...}}}` form the card expects. Memoised for a stable
  // prop ref (and kept all-time — the card's headline cache figures are too).
  const cacheDailyStats = useMemo(() => {
    const daily = (data?.token_composition as TokenComposition | undefined)?.daily
    if (!daily) return undefined
    const out: Record<string, { tokens: Record<string, number> }> = {}
    for (const [date, toks] of Object.entries(daily)) out[date] = { tokens: toks }
    return out
  }, [data])

  // ── callbacks wired into Wave-B components ────────────────────────────────

  const handleOpenInteraction = useCallback((id: string) => {
    openInteraction(id)
  }, [])
  const handleOpenSession = useCallback((id: string) => {
    openSession(id)
  }, [])

  const handleRangeChange = (r: RangeKey) => setFilter((f) => ({ ...f, range: r }))
  const handleClearSession = () => setFilter((f) => ({ ...f, sessionFilter: null }))
  const handleClearTool = () => setFilter((f) => ({ ...f, toolFilter: null }))

  // ── render ────────────────────────────────────────────────────────────────

  if (loading) {
    return (
      <div className="space-y-6">
        <FilterBar
          filter={filter}
          onRangeChange={handleRangeChange}
          onClearSession={handleClearSession}
          onClearTool={handleClearTool}
        />
        <Skeleton />
      </div>
    )
  }

  if (error || !data) {
    return (
      <div className="space-y-6">
        <FilterBar
          filter={filter}
          onRangeChange={handleRangeChange}
          onClearSession={handleClearSession}
          onClearTool={handleClearTool}
        />
        <ErrorBanner message={error ?? 'No data returned from /api/cost-data'} onRetry={reload} />
      </div>
    )
  }

  const f = filtered!
  // #32: filter-aware token totals (computed in the `filtered` memo — sums
  // per-session token maps over the windowed sessions, else all-time).
  const tokenComp = data.token_composition as TokenComposition | undefined
  const tokenTotals = f.tokenTotals

  const trends = (data.trends ?? undefined) as Trends | undefined
  const errorCost = (data.error_cost ?? undefined) as ErrorCost | undefined
  const outliers = (data.outliers ?? undefined) as Outliers | undefined

  // Map the tab's range filter onto the by-model endpoint's period grain
  // (same mapping CostByProviderCard uses for its initial period).
  const byModelPeriod: ByModelPeriod =
    filter.range === '7d' ? 'week' : filter.range === '30d' ? 'month' : 'all'

  return (
    <div className="space-y-6">
      <FilterBar
        filter={filter}
        onRangeChange={handleRangeChange}
        onClearSession={handleClearSession}
        onClearTool={handleClearTool}
      />

      {/* 0. Spend-by-agent card — v0.6.1 multi-provider polish. Surfaces
          the per-provider total cost split for the active period above
          everything else so users with multi-agent setups see the share
          first. The card reads its own period independently from the
          tab-level range filter — both are useful: range filters the
          per-session/per-tool charts below, the card's period drives a
          purpose-built backend rollup. */}
      <CostByProviderCard
        initialPeriod={
          filter.range === '7d' ? 'week' : filter.range === '30d' ? 'month' : 'all'
        }
      />

      {/* 1. Trend strip full-width */}
      <TrendDeltaStrip trends={trends} />

      {/* 2. Cache ROI · Error cost two-column.
          #48/#63: feed the card `session_costs` (drives the "Top cache savers"
          breakdown) and per-day token data (drives the ROI sparkline) so both
          previously-dead surfaces render. */}
      <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
        <CacheRoiCard
          cache={(data.cache ?? stats?.cache ?? undefined) as never}
          sessionCosts={data.session_costs}
          dailyStats={cacheDailyStats}
        />
        <ErrorCostCard errorCost={errorCost} />
      </div>

      {/* 3. Session cost bar chart — clicking a bar deep-links to the session */}
      <SessionCostBarChart
        data={f.sessions}
        onSelect={(sid) => {
          setFilter((prev) => ({ ...prev, sessionFilter: sid }))
          handleOpenSession(sid)
        }}
      />

      {/* 4. Command cost list — clicking a row deep-links to the interaction */}
      <CommandCostList data={f.commands} onOpen={handleOpenInteraction} />

      {/* 5. Tool cost · Token donut two-column.
          #24: per-tool costs come from an all-time mart with no per-row
          timestamps, so the date-range filter can't be applied client-side.
          Surface an "all time" badge when a range is active rather than
          silently mislabelling all-time totals as "Last 7/30 days".
          #57: tool_mart has no model dimension either, so the model filter
          can't narrow tool costs — badge it all-models when a model filter is
          active so the bars don't read as model-scoped. */}
      <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
        <div>
          {(filter.range !== 'all' || modelFilter.length > 0) && (
            <div className="mb-1.5 flex items-center gap-1.5 text-[10px] text-amber-700 dark:text-amber-400">
              <IconAlertTriangle size={11} />
              <span>
                Tool costs are{' '}
                <span className="font-semibold">
                  {[filter.range !== 'all' ? 'all-time' : null, modelFilter.length > 0 ? 'all-models' : null]
                    .filter(Boolean)
                    .join(' · ')}
                </span>{' '}
                — the {filter.range !== 'all' && modelFilter.length > 0 ? 'date-range and model filters aren’t' : filter.range !== 'all' ? 'date-range filter isn’t' : 'model filter isn’t'} applied here yet.
              </span>
            </div>
          )}
          <ToolCostBarChart data={f.tools} />
        </div>
        <div>
          {modelFilter.length > 0 && (
            <div className="mb-1.5 text-[10px] text-gray-500">
              Token composition scoped to{' '}
              <span className="font-medium text-gray-700 dark:text-gray-300">
                {modelFilter.map(formatModelName).join(', ')}
              </span>
            </div>
          )}
          <TokenCompositionDonut totals={tokenTotals} />
        </div>
      </div>

      {/* 6. Token composition stack full-width */}
      <TokenCompositionStack daily={tokenComp?.daily ?? {}} />

      {/* 6b. Daily cost split by model (audit #6) — the spend sibling of the
          token-type stack above. Consumes /api/cost-data/by-model and also
          surfaces an unpriced-models banner. Scoped to the tab range via the
          by-model endpoint's period grain. */}
      <ByModelCostChart period={byModelPeriod} />

      {/* 7. Outlier commands table — onOpen deep-links to interaction */}
      <OutlierCommandsTable outliers={outliers} onOpen={handleOpenInteraction} />

      {/* 8. Retry alerts panel full-width */}
      <RetryAlertsPanel signals={f.retries} />
    </div>
  )
}
