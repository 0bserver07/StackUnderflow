import { useCallback, useEffect, useState } from 'react'
import { useParams } from 'react-router-dom'
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import {
  IconRefresh,
  IconLayoutDashboard,
  IconFolders,
  IconCurrencyDollar,
  IconTerminal2,
  IconMessageCircle,
  IconSearch,
  IconHelpCircle,
  IconBookmark,
  IconTag,
} from '@tabler/icons-react'
import { setProjectByDir, getDashboardData, refreshData } from '../services/api'
import { formatProjectName, getNameMode } from '../services/nameMode'
import {
  NAV_EVENT,
  getTabFromURL,
  getParam,
  clearParam,
  setTab as navSetTab,
} from '../services/navigation'
import LoadingSpinner from '../components/common/LoadingSpinner'
import EmptyState from '../components/common/EmptyState'
import { Breadcrumb, BackButton } from '../components/common/Breadcrumb'
import OverviewTab from '../components/dashboard/OverviewTab'
import CommandsTab from '../components/dashboard/CommandsTab'
import MessagesTab from '../components/dashboard/MessagesTab'
import SearchTab from '../components/dashboard/SearchTab'
import QATab from '../components/dashboard/QATab'
import BookmarksTab from '../components/dashboard/BookmarksTab'
import TagsTab from '../components/dashboard/TagsTab'
import SessionsTab from '../components/dashboard/SessionsTab'
import CostTab from '../components/dashboard/CostTab'

const TABS = [
  { id: 'overview', label: 'Overview', icon: IconLayoutDashboard },
  { id: 'sessions', label: 'Sessions', icon: IconFolders },
  { id: 'cost', label: 'Cost', icon: IconCurrencyDollar },
  { id: 'commands', label: 'Commands', icon: IconTerminal2 },
  { id: 'messages', label: 'Messages', icon: IconMessageCircle },
  { id: 'search', label: 'Search', icon: IconSearch },
  { id: 'qa', label: 'Q&A', icon: IconHelpCircle },
  { id: 'bookmarks', label: 'Bookmarks', icon: IconBookmark },
  { id: 'tags', label: 'Tags', icon: IconTag },
] as const

type TabId = typeof TABS[number]['id']

const TAB_IDS = TABS.map(t => t.id) as readonly string[]

function isValidTab(value: string | null): value is TabId {
  return !!value && TAB_IDS.includes(value)
}

function resolveInitialTab(): TabId {
  const fromUrl = getTabFromURL()
  return isValidTab(fromUrl) ? fromUrl : 'overview'
}

export default function ProjectDashboard() {
  const { name } = useParams<{ name: string }>()
  const queryClient = useQueryClient()

  // Initial tab comes from `?tab=` if present and valid.
  const [activeTab, setActiveTab] = useState<TabId>(resolveInitialTab)
  // `urlTick` bumps whenever the URL changes via NAV_EVENT or popstate so
  // children that read URL params (Search query, etc.) re-render in step.
  const [urlTick, setUrlTick] = useState(0)

  // Set project on backend
  const { isLoading: settingProject, error: setError } = useQuery({
    queryKey: ['setProject', name],
    queryFn: () => setProjectByDir(name!),
    enabled: !!name,
    staleTime: 60_000,
  })

  // Load dashboard data
  const { data: dashboardData, isLoading: loadingData, error: dataError } = useQuery({
    queryKey: ['dashboardData', name],
    queryFn: () => getDashboardData(new Date().getTimezoneOffset()),
    enabled: !!name && !settingProject,
  })

  const refreshMutation = useMutation({
    mutationFn: () => refreshData(new Date().getTimezoneOffset()),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['dashboardData', name] })
    },
  })

  // User clicked a tab in the bar: swap state + rewrite URL via
  // `history.replaceState` so other params (session=, interaction=, q=)
  // are preserved and we don't pollute the back/forward stack.
  const handleTabChange = useCallback((tab: TabId) => {
    setActiveTab(tab)
    if (typeof window === 'undefined') return
    const url = new URL(window.location.href)
    url.searchParams.set('tab', tab)
    const next = `${url.pathname}${url.search}${url.hash}`
    const current = `${window.location.pathname}${window.location.search}${window.location.hash}`
    if (next !== current) {
      window.history.replaceState({}, '', next)
    }
  }, [])

  // Programmatic API for child components / external callers that prefer
  // the navigation service. Keeps `activeTab` in sync if the listeners
  // somehow miss the round-trip event.
  const switchTabProgrammatically = useCallback(
    (tab: TabId, extra?: Record<string, string>) => {
      setActiveTab(tab)
      navSetTab(tab, extra)
    },
    [],
  )
  // Surface on window for ad-hoc cross-tab triggers (debug + e2e hooks).
  useEffect(() => {
    if (typeof window === 'undefined') return
    ;(window as unknown as { __suSwitchTab?: typeof switchTabProgrammatically }).__suSwitchTab =
      switchTabProgrammatically
    return () => {
      delete (window as unknown as { __suSwitchTab?: typeof switchTabProgrammatically }).__suSwitchTab
    }
  }, [switchTabProgrammatically])

  // Listen to NAV_EVENT (fired by openInteraction / openSession / setTab in
  // ../services/navigation) and to the browser's popstate so back/forward
  // and deep-link nav both update the active tab without a full reload.
  useEffect(() => {
    if (typeof window === 'undefined') return
    const sync = () => {
      const next = getTabFromURL()
      if (isValidTab(next)) {
        setActiveTab(prev => (prev === next ? prev : next))
      }
      setUrlTick(t => t + 1)
    }
    window.addEventListener(NAV_EVENT, sync as EventListener)
    window.addEventListener('popstate', sync)
    return () => {
      window.removeEventListener(NAV_EVENT, sync as EventListener)
      window.removeEventListener('popstate', sync)
    }
  }, [])

  const displayName = name ? formatProjectName(name, undefined, getNameMode()) : ''

  if (settingProject || loadingData) return <LoadingSpinner message={`Loading ${displayName}...`} />
  if (setError || dataError) {
    const err = setError || dataError
    return (
      <div className="p-6">
        <div className="bg-red-900/20 border border-red-800 rounded-lg p-4 text-red-400 text-sm">
          Failed to load project: {err instanceof Error ? err.message : 'Unknown error'}
        </div>
      </div>
    )
  }
  if (!dashboardData) return <EmptyState title="No data" description="No dashboard data available" />

  const stats = dashboardData.statistics
  // Re-read on every render so URL changes captured via `urlTick` propagate.
  void urlTick
  const initialSearchQuery = getParam('q') ?? ''

  // Deep-link awareness: when ?session= or ?interaction= is active, surface a
  // breadcrumb (with BackButton) between the tab bar and the tab content.
  // Clicking the tab segment clears the deep-link params and returns the user
  // to the bare tab; keeps the tab bar as the primary nav, avoids clutter
  // when no detail view is active.
  const activeSessionParam = getParam('session')
  const activeInteractionParam = getParam('interaction')
  const activeTabLabel = TABS.find(t => t.id === activeTab)?.label ?? activeTab
  let breadcrumbTrail: Array<{ label: string; onClick?: () => void }> | null = null
  if (activeSessionParam) {
    breadcrumbTrail = [
      { label: activeTabLabel, onClick: () => clearParam('session') },
      { label: `Session · ${activeSessionParam}` },
    ]
  } else if (activeInteractionParam) {
    breadcrumbTrail = [
      { label: activeTabLabel, onClick: () => clearParam('interaction') },
      { label: `Interaction · ${activeInteractionParam}` },
    ]
  }

  return (
    <div className="max-w-7xl mx-auto px-6 py-4 space-y-4">
      {/* Dashboard Header */}
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-lg font-bold text-gray-100">{displayName}</h1>
          {stats?.overview?.date_range && (
            <p className="text-xs text-gray-500">
              {stats.overview.date_range.start} — {stats.overview.date_range.end}
            </p>
          )}
        </div>
        <div className="flex items-center gap-2">
          {dashboardData.is_reindexing && (
            <span className="text-xs text-yellow-400 bg-yellow-900/20 px-2 py-1 rounded">Reindexing...</span>
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

      {/* Tab Bar */}
      <div className="border-b border-gray-800">
        <nav className="flex gap-0 -mb-px overflow-x-auto" data-testid="dashboard-tabs">
          {TABS.map(tab => {
            const Icon = tab.icon
            return (
              <button
                key={tab.id}
                onClick={() => handleTabChange(tab.id)}
                data-tab={tab.id}
                className={`flex items-center gap-1.5 px-4 py-2 text-sm font-medium whitespace-nowrap border-b-2 ${
                  activeTab === tab.id
                    ? 'text-indigo-400 border-indigo-400'
                    : 'text-gray-400 border-transparent hover:text-gray-200 hover:border-gray-600'
                }`}
              >
                <Icon size={14} />
                {tab.label}
              </button>
            )
          })}
        </nav>
      </div>

      {/* Breadcrumb strip — shown only when a deep-link param is active, to
          avoid clutter on the main tab views. */}
      {breadcrumbTrail && (
        <div className="flex items-center gap-3">
          <BackButton />
          <Breadcrumb trail={breadcrumbTrail} />
        </div>
      )}

      {/* Tab Content */}
      <div>
        {activeTab === 'overview' && <OverviewTab stats={stats} />}
        {activeTab === 'cost' && <CostTab stats={stats} />}
        {activeTab === 'commands' && <CommandsTab data={dashboardData} />}
        {activeTab === 'messages' && <MessagesTab data={dashboardData} projectName={name!} />}
        {activeTab === 'search' && <SearchTab projectName={name!} initialQuery={initialSearchQuery} />}
        {activeTab === 'qa' && <QATab projectName={name!} />}
        {activeTab === 'bookmarks' && <BookmarksTab />}
        {activeTab === 'tags' && <TagsTab />}
        {activeTab === 'sessions' && <SessionsTab projectName={name!} sessionEfficiency={stats.session_efficiency} />}
      </div>
    </div>
  )
}
