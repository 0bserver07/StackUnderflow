import { useState, useEffect } from 'react'
import { useParams, useSearchParams } from 'react-router-dom'
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
import LoadingSpinner from '../components/common/LoadingSpinner'
import EmptyState from '../components/common/EmptyState'
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

export default function ProjectDashboard() {
  const { name } = useParams<{ name: string }>()
  const [searchParams, setSearchParams] = useSearchParams()
  const queryClient = useQueryClient()

  const tabParam = searchParams.get('tab') as TabId | null
  const [activeTab, setActiveTab] = useState<TabId>(tabParam && TABS.some(t => t.id === tabParam) ? tabParam : 'overview')

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

  const handleTabChange = (tab: TabId) => {
    setActiveTab(tab)
    const params = new URLSearchParams(searchParams)
    if (tab === 'overview') {
      params.delete('tab')
    } else {
      params.set('tab', tab)
    }
    setSearchParams(params, { replace: true })
  }

  // Sync tab from URL on mount
  useEffect(() => {
    if (tabParam && TABS.some(t => t.id === tabParam)) {
      setActiveTab(tabParam)
    }
  }, [tabParam])

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
        <nav className="flex gap-0 -mb-px overflow-x-auto">
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

      {/* Tab Content */}
      <div>
        {activeTab === 'overview' && <OverviewTab stats={stats} />}
        {activeTab === 'cost' && <CostTab stats={stats} />}
        {activeTab === 'commands' && <CommandsTab data={dashboardData} />}
        {activeTab === 'messages' && <MessagesTab data={dashboardData} projectName={name!} />}
        {activeTab === 'search' && <SearchTab projectName={name!} initialQuery={searchParams.get('q') ?? ''} />}
        {activeTab === 'qa' && <QATab projectName={name!} />}
        {activeTab === 'bookmarks' && <BookmarksTab />}
        {activeTab === 'tags' && <TagsTab />}
        {activeTab === 'sessions' && <SessionsTab projectName={name!} sessionEfficiency={stats.session_efficiency} />}
      </div>
    </div>
  )
}
