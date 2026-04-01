import { useState, useEffect, useMemo } from 'react'
import { useNavigate } from 'react-router-dom'
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import { IconRefresh, IconArrowUp, IconArrowDown, IconSearch } from '@tabler/icons-react'
import { getProjects, refreshData, getGlobalStats } from '../services/api'
import { formatProjectName, getNameMode, setNameMode as persistNameMode } from '../services/nameMode'
import type { NameMode } from '../services/nameMode'
import type { Project } from '../types/api'
import LoadingSpinner from '../components/common/LoadingSpinner'
import EmptyState from '../components/common/EmptyState'
import {
  AreaChart, Area, BarChart, Bar, XAxis, YAxis, CartesianGrid, Tooltip, ResponsiveContainer,
} from 'recharts'

type SortField = 'display_name' | 'last_modified' | 'total_cost' | 'total_commands' | 'total_size_mb'
type SortDir = 'asc' | 'desc'
type DateRange = '7d' | '30d' | '90d' | 'all'
const NAME_MODES: { key: NameMode; label: string }[] = [
  { key: 'name', label: 'Name' },
  { key: 'path', label: 'Path' },
  { key: 'anon', label: 'Anon' },
]

const DATE_PRESETS: { key: DateRange; label: string }[] = [
  { key: '7d', label: '7 days' },
  { key: '30d', label: '30 days' },
  { key: '90d', label: '90 days' },
  { key: 'all', label: 'All time' },
]

function daysAgo(n: number): Date {
  const d = new Date()
  d.setDate(d.getDate() - n)
  d.setHours(0, 0, 0, 0)
  return d
}

function rangeCutoff(range: DateRange): Date | null {
  if (range === '7d') return daysAgo(7)
  if (range === '30d') return daysAgo(30)
  if (range === '90d') return daysAgo(90)
  return null
}

function formatDate(ts: number): string {
  return new Date(ts * 1000).toLocaleDateString(undefined, { month: 'short', day: 'numeric', year: 'numeric' })
}

function formatCost(cost: number): string {
  if (cost === 0) return '$0'
  if (cost < 0.01) return `$${cost.toFixed(4)}`
  if (cost >= 1000) return `$${cost.toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`
  return `$${cost.toFixed(2)}`
}

function formatDuration(firstDate: string | undefined, lastDate: string | undefined): string {
  if (!firstDate || !lastDate) return '-'
  const ms = new Date(lastDate).getTime() - new Date(firstDate).getTime()
  const days = Math.floor(ms / (1000 * 60 * 60 * 24))
  if (days < 1) return '<1d'
  if (days < 30) return `${days}d`
  const months = Math.floor(days / 30)
  const remDays = days % 30
  if (months < 12) return remDays > 0 ? `${months}mo ${remDays}d` : `${months}mo`
  return `${Math.floor(months / 12)}y ${months % 12}mo`
}

function formatTokens(n: number): string {
  if (n >= 1_000_000_000) return `${(n / 1_000_000_000).toFixed(1)}B`
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`
  return n.toLocaleString()
}

function rangeLabel(range: DateRange, firstUse: string | undefined): string {
  if (range === 'all' && firstUse) {
    const d = new Date(firstUse)
    return `Since ${d.toLocaleDateString(undefined, { month: 'short', day: 'numeric', year: 'numeric' })}`
  }
  const preset = DATE_PRESETS.find(p => p.key === range)
  return preset ? `Last ${preset.label}` : ''
}

export default function Overview() {
  const navigate = useNavigate()
  const queryClient = useQueryClient()
  const [search, setSearch] = useState('')
  const [sortField, setSortField] = useState<SortField>('last_modified')
  const [sortDir, setSortDir] = useState<SortDir>('desc')
  const [page, setPage] = useState(1)
  const [perPage, setPerPage] = useState(20)
  const [dateRange, setDateRange] = useState<DateRange>('30d')
  const [selectedModel, setSelectedModel] = useState<string>('all')
  const [nameMode, setNameModeLocal] = useState<NameMode>(getNameMode())
  const setNameModeAndPersist = (m: NameMode) => { setNameModeLocal(m); persistNameMode(m) }

  const { data: projectsData, isLoading } = useQuery({
    queryKey: ['projects', true],
    queryFn: () => getProjects(true),
  })

  const { data: globalStats } = useQuery({
    queryKey: ['globalStats'],
    queryFn: getGlobalStats,
  })

  const refreshMutation = useMutation({
    mutationFn: () => refreshData(new Date().getTimezoneOffset()),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['projects'] })
      queryClient.invalidateQueries({ queryKey: ['globalStats'] })
    },
  })

  const projects = projectsData?.projects ?? []

  // Global stats
  const gStats = globalStats as Record<string, unknown> | undefined
  const firstUseDate = gStats?.first_use_date as string | undefined
  const lastUseDate = gStats?.last_use_date as string | undefined

  // Daily data from API
  const allDailyTokens = (gStats?.daily_token_usage as Array<{ date: string; input: number; output: number }>) ?? []
  const allDailyCosts = (gStats?.daily_costs as Array<{ date: string; cost: number; by_model?: Record<string, number> }>) ?? []

  // Available models from API
  const apiModels = (gStats?.models ?? {}) as Record<string, { count: number; cost: number }>
  const availableModels = useMemo(() => {
    const models = Object.keys(apiModels).filter(m => m !== '<synthetic>')
    // sort by cost descending
    models.sort((a, b) => (apiModels[b]?.cost ?? 0) - (apiModels[a]?.cost ?? 0))
    return models
  }, [apiModels])

  // Short display name for models
  const shortModelName = (m: string): string => {
    return m
      .replace('claude-', '')
      .replace(/-\d{8,}$/, '')  // strip date suffix
      .replace('-20251101', '')
      .replace('-20250929', '')
      .replace('-20251001', '')
  }

  const projectDisplayName = (p: Project, index: number): string =>
    formatProjectName(p.dir_name, index, nameMode)

  // Filter daily data by date range
  const cutoff = rangeCutoff(dateRange)
  const dailyTokens = useMemo(() => {
    if (!cutoff) return allDailyTokens
    return allDailyTokens.filter(d => new Date(d.date) >= cutoff)
  }, [allDailyTokens, cutoff])

  // Filter daily costs by date range AND model
  const dailyCosts = useMemo(() => {
    let data = allDailyCosts
    if (cutoff) {
      data = data.filter(d => new Date(d.date) >= cutoff)
    }
    if (selectedModel !== 'all') {
      // recalculate cost from by_model data
      return data.map(d => ({
        ...d,
        cost: d.by_model?.[selectedModel] ?? 0,
      }))
    }
    return data
  }, [allDailyCosts, cutoff, selectedModel])

  // Filter projects by date range
  const dateFilteredProjects = useMemo(() => {
    if (!cutoff) return projects
    const cutoffTs = cutoff.getTime() / 1000
    return projects.filter(p => p.last_modified >= cutoffTs)
  }, [projects, cutoff])

  // Cache token totals from the global stats (not available per-day)
  const totalCacheRead = (gStats?.total_cache_read_tokens as number) ?? 0
  const totalCacheWrite = (gStats?.total_cache_write_tokens as number) ?? 0

  // Compute stats from filtered daily data — always sum from the visible range
  const filteredStats = useMemo(() => {
    const inputTok = dailyTokens.reduce((s, d) => s + d.input, 0)
    const outputTok = dailyTokens.reduce((s, d) => s + d.output, 0)
    const cost = dailyCosts.reduce((s, d) => s + (d.cost ?? 0), 0)
    const cmds = dateFilteredProjects.reduce((s, p) => s + (p.stats?.total_commands ?? 0), 0)
    // For "all time", include cache tokens; for filtered ranges, just show input+output
    const cacheTokens = dateRange === 'all' ? totalCacheRead + totalCacheWrite : 0
    return {
      totalTokens: inputTok + outputTok + cacheTokens,
      inputTokens: inputTok,
      outputTokens: outputTok,
      cacheTokens,
      totalCost: cost,
      totalCommands: cmds,
      projectCount: dateFilteredProjects.length,
    }
  }, [dailyTokens, dailyCosts, dateFilteredProjects, dateRange, totalCacheRead, totalCacheWrite])

  // Search & sort on date-filtered projects
  const filtered = useMemo(() => {
    let result = dateFilteredProjects
    if (search) {
      const q = search.toLowerCase()
      result = result.filter(p => p.display_name.toLowerCase().includes(q) || p.dir_name.toLowerCase().includes(q))
    }
    result = [...result].sort((a, b) => {
      let av: number | string
      let bv: number | string
      switch (sortField) {
        case 'display_name': av = a.display_name.toLowerCase(); bv = b.display_name.toLowerCase(); break
        case 'last_modified': av = a.last_modified; bv = b.last_modified; break
        case 'total_cost': av = a.stats?.total_cost ?? 0; bv = b.stats?.total_cost ?? 0; break
        case 'total_commands': av = a.stats?.total_commands ?? 0; bv = b.stats?.total_commands ?? 0; break
        case 'total_size_mb': av = a.total_size_mb; bv = b.total_size_mb; break
        default: av = 0; bv = 0
      }
      if (av < bv) return sortDir === 'asc' ? -1 : 1
      if (av > bv) return sortDir === 'asc' ? 1 : -1
      return 0
    })
    return result
  }, [dateFilteredProjects, search, sortField, sortDir])

  // Pagination
  const totalPages = Math.ceil(filtered.length / perPage)
  const paged = filtered.slice((page - 1) * perPage, page * perPage)

  useEffect(() => { setPage(1) }, [search, sortField, sortDir, perPage, dateRange])

  const toggleSort = (field: SortField) => {
    if (sortField === field) {
      setSortDir(d => d === 'asc' ? 'desc' : 'asc')
    } else {
      setSortField(field)
      setSortDir('desc')
    }
  }

  const SortIcon = ({ field }: { field: SortField }) => {
    if (sortField !== field) return null
    return sortDir === 'asc'
      ? <IconArrowUp size={12} className="inline ml-0.5" />
      : <IconArrowDown size={12} className="inline ml-0.5" />
  }

  if (isLoading) return <LoadingSpinner message="Loading projects..." />
  if (projects.length === 0) return <EmptyState title="No projects found" description="Make sure you have Claude Code sessions in ~/.claude/projects/" />

  return (
    <div className="max-w-7xl mx-auto p-6 space-y-6">
      {/* Header */}
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-xl font-bold text-gray-100">Projects Overview</h1>
          <p className="text-sm text-gray-500 mt-0.5">
            {filteredStats.projectCount} projects {dateRange !== 'all' ? `active in last ${dateRange.replace('d', ' days')}` : 'analyzed'}
            {dateRange === 'all' && firstUseDate && (
              <> &middot; since {new Date(firstUseDate).toLocaleDateString(undefined, { month: 'short', year: 'numeric' })}</>
            )}
          </p>
        </div>
        <div className="flex items-center gap-3 flex-wrap">
          {/* Date range presets */}
          <div className="flex items-center bg-gray-800 rounded border border-gray-700 overflow-hidden">
            {DATE_PRESETS.map(({ key, label }) => (
              <button
                key={key}
                onClick={() => setDateRange(key)}
                className={`px-3 py-1.5 text-xs font-medium transition-colors ${
                  dateRange === key
                    ? 'bg-indigo-600 text-white'
                    : 'text-gray-400 hover:text-gray-200 hover:bg-gray-700'
                }`}
              >
                {label}
              </button>
            ))}
          </div>
          {/* Model filter */}
          {availableModels.length > 1 && (
            <div className="flex items-center bg-gray-800 rounded border border-gray-700 overflow-hidden">
              <button
                onClick={() => setSelectedModel('all')}
                className={`px-3 py-1.5 text-xs font-medium transition-colors ${
                  selectedModel === 'all'
                    ? 'bg-emerald-600 text-white'
                    : 'text-gray-400 hover:text-gray-200 hover:bg-gray-700'
                }`}
              >
                All models
              </button>
              {availableModels.map(m => (
                <button
                  key={m}
                  onClick={() => setSelectedModel(m)}
                  className={`px-3 py-1.5 text-xs font-medium transition-colors ${
                    selectedModel === m
                      ? 'bg-emerald-600 text-white'
                      : 'text-gray-400 hover:text-gray-200 hover:bg-gray-700'
                  }`}
                >
                  {shortModelName(m)}
                </button>
              ))}
            </div>
          )}
          <button
            onClick={() => refreshMutation.mutate()}
            disabled={refreshMutation.isPending}
            className="flex items-center gap-1.5 px-3 py-1.5 bg-gray-800 text-gray-300 hover:text-white rounded text-sm border border-gray-700 hover:border-gray-600 disabled:opacity-50"
          >
            <IconRefresh size={14} className={refreshMutation.isPending ? 'animate-spin' : ''} />
            Refresh
          </button>
        </div>
      </div>

      {/* Global Stats Cards */}
      <div className="grid grid-cols-2 md:grid-cols-5 gap-4">
        <div className="bg-gray-800/50 rounded-lg p-4 border border-gray-800">
          <div className="text-xs text-gray-500 uppercase tracking-wider">Projects</div>
          <div className="text-2xl font-bold text-gray-100 mt-1">{filteredStats.projectCount}</div>
        </div>
        <div className="bg-gray-800/50 rounded-lg p-4 border border-gray-800">
          <div className="text-xs text-gray-500 uppercase tracking-wider">Total Tokens</div>
          <div className="text-2xl font-bold text-gray-100 mt-1">{formatTokens(filteredStats.totalTokens)}</div>
          <div className="text-[10px] text-gray-500 mt-0.5">
            In: {formatTokens(filteredStats.inputTokens)} / Out: {formatTokens(filteredStats.outputTokens)}
            {filteredStats.cacheTokens > 0 && <> / Cache: {formatTokens(filteredStats.cacheTokens)}</>}
          </div>
        </div>
        <div className="bg-gray-800/50 rounded-lg p-4 border border-gray-800">
          <div className="text-xs text-gray-500 uppercase tracking-wider">Est. API Cost</div>
          <div className="text-2xl font-bold text-gray-100 mt-1">{formatCost(filteredStats.totalCost)}</div>
          <div className="text-[10px] text-gray-500 mt-0.5">{rangeLabel(dateRange, firstUseDate)}</div>
          <div className="text-[9px] text-gray-600 mt-0.5">pay-per-token equivalent</div>
        </div>
        <div className="bg-gray-800/50 rounded-lg p-4 border border-gray-800">
          <div className="text-xs text-gray-500 uppercase tracking-wider">Commands</div>
          <div className="text-2xl font-bold text-gray-100 mt-1">{filteredStats.totalCommands.toLocaleString()}</div>
        </div>
        <div className="bg-gray-800/50 rounded-lg p-4 border border-gray-800">
          <div className="text-xs text-gray-500 uppercase tracking-wider">Cached</div>
          <div className="text-2xl font-bold text-gray-100 mt-1">
            {projectsData?.cache_status?.cached_count ?? 0}/{projects.length}
          </div>
        </div>
      </div>

      {/* Token Usage Chart — full width */}
      {dailyTokens.length > 0 && (
        <div className="bg-gray-800/50 rounded-lg p-4 border border-gray-800">
          <h3 className="text-sm font-medium text-gray-300 mb-3">Token Usage Over Time</h3>
          <ResponsiveContainer width="100%" height={280}>
            <AreaChart data={dailyTokens}>
              <CartesianGrid strokeDasharray="3 3" stroke="#374151" />
              <XAxis dataKey="date" tick={{ fontSize: 10, fill: '#9CA3AF' }} />
              <YAxis tick={{ fontSize: 10, fill: '#9CA3AF' }} tickFormatter={formatTokens} />
              <Tooltip
                contentStyle={{ backgroundColor: '#1F2937', border: '1px solid #374151', borderRadius: '6px', fontSize: '12px' }}
                formatter={(value: number) => [value.toLocaleString(), undefined]}
              />
              <Area type="monotone" dataKey="input" stackId="1" stroke="#818CF8" fill="#818CF8" fillOpacity={0.4} name="Input" />
              <Area type="monotone" dataKey="output" stackId="1" stroke="#34D399" fill="#34D399" fillOpacity={0.4} name="Output" />
            </AreaChart>
          </ResponsiveContainer>
        </div>
      )}

      {/* Daily Cost Chart — full width */}
      {dailyCosts.length > 0 && (
        <div className="bg-gray-800/50 rounded-lg p-4 border border-gray-800">
          <h3 className="text-sm font-medium text-gray-300 mb-3">Daily Cost</h3>
          <ResponsiveContainer width="100%" height={200}>
            <BarChart data={dailyCosts}>
              <CartesianGrid strokeDasharray="3 3" stroke="#374151" />
              <XAxis dataKey="date" tick={{ fontSize: 10, fill: '#9CA3AF' }} />
              <YAxis tick={{ fontSize: 10, fill: '#9CA3AF' }} tickFormatter={v => `$${v}`} />
              <Tooltip contentStyle={{ backgroundColor: '#1F2937', border: '1px solid #374151', borderRadius: '6px', fontSize: '12px' }} formatter={(v: number) => [`$${v.toFixed(4)}`, 'Cost']} />
              <Bar dataKey="cost" fill="#818CF8" radius={[2, 2, 0, 0]} />
            </BarChart>
          </ResponsiveContainer>
        </div>
      )}

      {/* Search + Per Page */}
      <div className="flex items-center gap-3">
        <div className="relative flex-1 max-w-xs">
          <IconSearch size={14} className="absolute left-2.5 top-1/2 -translate-y-1/2 text-gray-500" />
          <input
            type="text"
            value={search}
            onChange={e => setSearch(e.target.value)}
            placeholder="Filter projects..."
            className="w-full bg-gray-800 border border-gray-700 rounded pl-8 pr-3 py-1.5 text-sm text-gray-300 placeholder-gray-500 focus:outline-none focus:border-indigo-500"
          />
        </div>
        {/* Name display mode */}
        <div className="flex items-center bg-gray-800 rounded border border-gray-700 overflow-hidden">
          {NAME_MODES.map(({ key, label }) => (
            <button
              key={key}
              onClick={() => setNameModeAndPersist(key)}
              className={`px-2.5 py-1.5 text-xs font-medium transition-colors ${
                nameMode === key
                  ? 'bg-violet-600 text-white'
                  : 'text-gray-400 hover:text-gray-200 hover:bg-gray-700'
              }`}
            >
              {label}
            </button>
          ))}
        </div>
        <select
          value={perPage}
          onChange={e => setPerPage(Number(e.target.value))}
          className="bg-gray-800 border border-gray-700 rounded px-2 py-1.5 text-sm text-gray-300"
        >
          <option value={10}>10</option>
          <option value={20}>20</option>
          <option value={50}>50</option>
          <option value={100}>100</option>
        </select>
      </div>

      {/* Projects Table */}
      <div className="bg-gray-800/30 rounded-lg border border-gray-800 overflow-hidden">
        <div className="overflow-x-auto">
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-gray-800 text-gray-400 text-xs uppercase tracking-wider">
                <th className="text-left px-4 py-3 cursor-pointer hover:text-gray-200" onClick={() => toggleSort('display_name')}>
                  Project <SortIcon field="display_name" />
                </th>
                <th className="text-left px-4 py-3 cursor-pointer hover:text-gray-200" onClick={() => toggleSort('last_modified')}>
                  Last Active <SortIcon field="last_modified" />
                </th>
                <th className="text-right px-4 py-3">Span</th>
                <th className="text-right px-4 py-3 cursor-pointer hover:text-gray-200" onClick={() => toggleSort('total_commands')}>
                  Commands <SortIcon field="total_commands" />
                </th>
                <th className="text-right px-4 py-3">Tokens</th>
                <th className="text-right px-4 py-3">Steps/Cmd</th>
                <th className="text-right px-4 py-3 cursor-pointer hover:text-gray-200" onClick={() => toggleSort('total_cost')}>
                  Est. Cost <SortIcon field="total_cost" />
                </th>
                <th className="text-right px-4 py-3 cursor-pointer hover:text-gray-200" onClick={() => toggleSort('total_size_mb')}>
                  Size <SortIcon field="total_size_mb" />
                </th>
              </tr>
            </thead>
            <tbody>
              {paged.map((p: Project, idx: number) => {
                const totalTok = (p.stats?.total_input_tokens ?? 0) + (p.stats?.total_output_tokens ?? 0)
                return (
                  <tr
                    key={p.dir_name}
                    onClick={() => navigate(`/project/${encodeURIComponent(p.dir_name)}`)}
                    className="border-b border-gray-800/50 hover:bg-gray-800/50 cursor-pointer"
                  >
                    <td className="px-4 py-3">
                      <div className="text-gray-200 font-medium">{projectDisplayName(p, (page - 1) * perPage + idx)}</div>
                      <div className="text-xs text-gray-500">{p.file_count} sessions</div>
                    </td>
                    <td className="px-4 py-3 text-gray-400 text-xs">
                      {formatDate(p.last_modified)}
                    </td>
                    <td className="px-4 py-3 text-right text-gray-500 text-xs">
                      {formatDuration(p.stats?.first_message_date ?? undefined, p.stats?.last_message_date ?? undefined)}
                    </td>
                    <td className="px-4 py-3 text-right text-gray-300">{p.stats?.total_commands?.toLocaleString() ?? '-'}</td>
                    <td className="px-4 py-3 text-right text-gray-400">
                      <div>{totalTok > 0 ? formatTokens(totalTok) : '-'}</div>
                      {totalTok > 0 && (
                        <div className="text-[10px] text-gray-600">
                          {formatTokens(p.stats?.total_input_tokens ?? 0)} in / {formatTokens(p.stats?.total_output_tokens ?? 0)} out
                        </div>
                      )}
                    </td>
                    <td className="px-4 py-3 text-right text-gray-400">
                      {p.stats?.avg_steps_per_command ? p.stats.avg_steps_per_command.toFixed(1) : '-'}
                    </td>
                    <td className="px-4 py-3 text-right text-gray-300">
                      {p.stats?.total_cost != null ? formatCost(p.stats.total_cost) : '-'}
                    </td>
                    <td className="px-4 py-3 text-right text-gray-400">
                      {p.total_size_mb.toFixed(1)} MB
                    </td>
                  </tr>
                )
              })}
            </tbody>
          </table>
        </div>
      </div>

      {/* Pagination */}
      {totalPages > 1 && (
        <div className="flex items-center justify-between text-sm text-gray-400">
          <span>
            Showing {(page - 1) * perPage + 1}-{Math.min(page * perPage, filtered.length)} of {filtered.length}
          </span>
          <div className="flex items-center gap-2">
            <button
              onClick={() => setPage(p => Math.max(1, p - 1))}
              disabled={page <= 1}
              className="px-3 py-1 bg-gray-800 rounded border border-gray-700 hover:border-gray-600 disabled:opacity-50"
            >
              Prev
            </button>
            <span>Page {page} of {totalPages}</span>
            <button
              onClick={() => setPage(p => Math.min(totalPages, p + 1))}
              disabled={page >= totalPages}
              className="px-3 py-1 bg-gray-800 rounded border border-gray-700 hover:border-gray-600 disabled:opacity-50"
            >
              Next
            </button>
          </div>
        </div>
      )}
    </div>
  )
}
