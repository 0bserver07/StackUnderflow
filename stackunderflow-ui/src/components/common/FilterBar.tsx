import { useMemo } from 'react'
import { useQuery } from '@tanstack/react-query'
import { IconFilter, IconX } from '@tabler/icons-react'
import { useFilters } from '../../services/filters'
import { getProviders } from '../../services/api'
import {
  getProviderColor,
  getProviderLabel,
} from '../../services/providerStyle'
import { formatModelName } from '../../services/format'
import Badge from './Badge'

// ---------------------------------------------------------------------------
// FilterBar — global provider/model filter strip rendered between the tab
// strip and the tab content. State lives in <FiltersProvider> (URL-synced),
// so toggles here scope every tab simultaneously and survive a refresh.
// Only renders provider chips when ≥2 providers exist in the store —
// single-provider installs see no extra chrome. Click-to-filter affordances
// elsewhere (Compare row, CostByProviderCard slice) push state through the
// same hook, so the bar always reflects the active set.
// ---------------------------------------------------------------------------

interface FilterBarProps {
  /** Optional model list to expose as toggle chips (top N by frequency). */
  modelOptions?: string[]
  /** Class for the outer wrapper, e.g. for top-margin tweaks. */
  className?: string
}

interface ProviderChipButtonProps {
  provider: string
  active: boolean
  count?: number
  onToggle: () => void
}

function ProviderChipButton({
  provider,
  active,
  count,
  onToggle,
}: ProviderChipButtonProps) {
  const color = getProviderColor(provider)
  const label = getProviderLabel(provider)
  return (
    <button
      type="button"
      onClick={onToggle}
      aria-pressed={active}
      data-testid={`filter-provider-chip-${provider}`}
      className={`inline-flex items-center gap-1 px-2 py-0.5 rounded-full text-[11px] font-medium border transition-colors ${
        active
          ? 'border-indigo-500 bg-indigo-500/15 text-indigo-700 dark:text-indigo-300 ring-1 ring-indigo-500/40'
          : 'border-gray-200 dark:border-gray-700 bg-white dark:bg-gray-800 text-gray-700 dark:text-gray-300 hover:border-gray-300 dark:hover:border-gray-500'
      }`}
      title={
        active
          ? `Active filter — ${label}. Click to remove.`
          : `Filter to ${label} only`
      }
    >
      <Badge color={color} size="sm">
        {label}
      </Badge>
      {typeof count === 'number' && count > 0 && (
        <span className="text-[10px] text-gray-500 tabular-nums">{count}</span>
      )}
    </button>
  )
}

interface ActivePillProps {
  label: string
  onRemove: () => void
  testId?: string
}

function ActivePill({ label, onRemove, testId }: ActivePillProps) {
  return (
    <span
      className="inline-flex items-center gap-1 px-2 py-0.5 rounded-full text-[11px] bg-indigo-100 dark:bg-indigo-900/40 text-indigo-700 dark:text-indigo-300 border border-indigo-300 dark:border-indigo-800"
      data-testid={testId}
    >
      <span className="font-mono">{label}</span>
      <button
        type="button"
        onClick={onRemove}
        aria-label={`Remove filter ${label}`}
        className="hover:text-indigo-900 dark:hover:text-indigo-100"
      >
        <IconX size={11} />
      </button>
    </span>
  )
}

export default function FilterBar({
  modelOptions = [],
  className = '',
}: FilterBarProps) {
  const {
    filters,
    addProvider,
    removeProvider,
    addModel,
    removeModel,
    setProviders,
    setModels,
    clearFilters,
    isFiltered,
  } = useFilters()

  // Derive the provider list from the projects index. Keeps us out of the
  // business of inventing a new endpoint — the project list already knows
  // which providers the store has touched. Cached at the react-query layer
  // so flipping between tabs doesn't refetch.
  const projectsQuery = useQuery({
    queryKey: ['providers-derive'],
    queryFn: () => getProviders(),
    staleTime: 5 * 60_000,
  })

  const providerCounts = projectsQuery.data?.providers ?? []

  const toggleProvider = (provider: string) => {
    if (filters.providers.includes(provider)) {
      removeProvider(provider)
    } else {
      addProvider(provider)
    }
  }

  const toggleModel = (model: string) => {
    if (filters.models.includes(model)) {
      removeModel(model)
    } else {
      addModel(model)
    }
  }

  // Hide the bar entirely when there's < 2 providers and nothing to filter
  // (single-provider installs don't need the chrome). Once any filter is
  // active, render the bar so the user can see + clear what's scoped.
  const showBar = providerCounts.length >= 2 || isFiltered

  // Top-N model chips: include any active models even if they're not in the
  // provided modelOptions list, so removing the last one doesn't make the
  // chip silently disappear before the user can click the X.
  const visibleModels = useMemo(() => {
    const seen = new Set<string>()
    const out: string[] = []
    for (const m of modelOptions) {
      const v = m.toLowerCase()
      if (!seen.has(v)) {
        seen.add(v)
        out.push(m)
      }
    }
    for (const m of filters.models) {
      if (!seen.has(m)) {
        seen.add(m)
        out.push(m)
      }
    }
    return out
  }, [modelOptions, filters.models])

  if (!showBar) return null

  return (
    <div
      className={`flex flex-wrap items-center gap-2 bg-gray-50/60 dark:bg-gray-900/50 border border-gray-200 dark:border-gray-800 rounded-lg px-3 py-2 ${className}`}
      data-testid="filter-bar"
    >
      <div className="flex items-center gap-1.5 text-gray-500 shrink-0">
        <IconFilter size={13} />
        <span className="text-[10px] uppercase tracking-wider">Filter</span>
      </div>

      {providerCounts.length >= 2 && (
        <div className="flex flex-wrap items-center gap-1.5">
          <button
            type="button"
            onClick={() => setProviders([])}
            aria-pressed={filters.providers.length === 0}
            className={`px-2 py-0.5 rounded-full text-[11px] font-medium border transition-colors ${
              filters.providers.length === 0
                ? 'border-gray-300 dark:border-gray-600 bg-gray-200 dark:bg-gray-700 text-gray-800 dark:text-gray-100'
                : 'border-gray-200 dark:border-gray-700 bg-white dark:bg-gray-800 text-gray-600 dark:text-gray-400 hover:text-gray-800 dark:hover:text-gray-200'
            }`}
            title="Show all providers"
          >
            All
          </button>
          {providerCounts.map((p) => (
            <ProviderChipButton
              key={p.provider}
              provider={p.provider}
              count={p.project_count}
              active={filters.providers.includes(p.provider)}
              onToggle={() => toggleProvider(p.provider)}
            />
          ))}
        </div>
      )}

      {visibleModels.length > 0 && (
        <div className="flex flex-wrap items-center gap-1.5 ml-2 pl-2 border-l border-gray-200 dark:border-gray-700">
          <span className="text-[10px] uppercase tracking-wider text-gray-500">
            Model
          </span>
          {filters.models.length > 0 && (
            <button
              type="button"
              onClick={() => setModels([])}
              className="px-2 py-0.5 rounded-full text-[10px] font-medium border border-gray-200 dark:border-gray-700 bg-white dark:bg-gray-800 text-gray-600 dark:text-gray-400 hover:text-gray-800 dark:hover:text-gray-200"
            >
              All
            </button>
          )}
          {visibleModels.map((m) => {
            const active = filters.models.includes(m.toLowerCase())
            return (
              <button
                key={m}
                type="button"
                onClick={() => toggleModel(m)}
                aria-pressed={active}
                className={`px-2 py-0.5 rounded-full text-[11px] font-mono border transition-colors ${
                  active
                    ? 'border-indigo-500 bg-indigo-500/15 text-indigo-700 dark:text-indigo-300'
                    : 'border-gray-200 dark:border-gray-700 bg-white dark:bg-gray-800 text-gray-600 dark:text-gray-400 hover:text-gray-800 dark:hover:text-gray-200'
                }`}
                title={m}
              >
                {formatModelName(m)}
              </button>
            )
          })}
        </div>
      )}

      {isFiltered && (
        <div className="flex flex-wrap items-center gap-1.5 ml-auto">
          {filters.providers.map((p) => (
            <ActivePill
              key={`p-${p}`}
              label={getProviderLabel(p)}
              onRemove={() => removeProvider(p)}
              testId={`filter-active-provider-${p}`}
            />
          ))}
          {filters.models.map((m) => (
            <ActivePill
              key={`m-${m}`}
              label={formatModelName(m)}
              onRemove={() => removeModel(m)}
              testId={`filter-active-model-${m}`}
            />
          ))}
          <button
            type="button"
            onClick={clearFilters}
            className="text-[11px] text-gray-600 dark:text-gray-400 hover:text-gray-900 dark:hover:text-gray-100 underline underline-offset-2"
            data-testid="filter-clear-all"
          >
            Clear
          </button>
        </div>
      )}
    </div>
  )
}
