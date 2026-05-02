import { useState, useEffect, useRef, useMemo } from 'react'
import { Link, useNavigate, useLocation } from 'react-router-dom'
import {
  IconStack2,
  IconSearch,
  IconMessageChatbot,
  IconChevronDown,
  IconSettings,
  IconChevronRight,
} from '@tabler/icons-react'
import { getProjects } from '../../services/api'
import { formatProjectName, getNameMode } from '../../services/nameMode'
import { getProviderLabel, getProviderColor } from '../../services/providerStyle'
import { useFilters } from '../../services/filters'
import type { Project } from '../../types/api'
import ThemeToggle from '../common/ThemeToggle'
import Badge from '../common/Badge'

// Persisted UI toggle: group-by-provider mode in the project picker. Stored
// in localStorage so the user's preference survives across sessions.
const PROJECT_GROUP_MODE_KEY = 'stackunderflow.project_group_by_provider'

function readGroupMode(): boolean {
  if (typeof localStorage === 'undefined') return false
  return localStorage.getItem(PROJECT_GROUP_MODE_KEY) === '1'
}

function writeGroupMode(value: boolean): void {
  if (typeof localStorage === 'undefined') return
  if (value) localStorage.setItem(PROJECT_GROUP_MODE_KEY, '1')
  else localStorage.removeItem(PROJECT_GROUP_MODE_KEY)
}

interface HeaderProps {
  onToggleChat: () => void
  chatOpen: boolean
}

export default function Header({ onToggleChat, chatOpen }: HeaderProps) {
  const navigate = useNavigate()
  const location = useLocation()
  const { addProvider } = useFilters()
  const [projects, setProjects] = useState<Project[]>([])
  const [dropdownOpen, setDropdownOpen] = useState(false)
  const [searchQuery, setSearchQuery] = useState('')
  const [projectFilter, setProjectFilter] = useState('')
  const [groupByProvider, setGroupByProvider] = useState<boolean>(() => readGroupMode())
  const [collapsedGroups, setCollapsedGroups] = useState<Set<string>>(new Set())
  const [, setTick] = useState(0) // force re-render when name mode changes
  const dropdownRef = useRef<HTMLDivElement>(null)

  const toggleGroupMode = () => {
    setGroupByProvider((v) => {
      const next = !v
      writeGroupMode(next)
      return next
    })
  }
  const toggleGroupCollapsed = (key: string) => {
    setCollapsedGroups((prev) => {
      const next = new Set(prev)
      if (next.has(key)) next.delete(key)
      else next.add(key)
      return next
    })
  }

  const projectMatch = location.pathname.match(/^\/project\/(.+?)(?:\/|$)/)
  const currentProject = projectMatch ? decodeURIComponent(projectMatch[1]!) : null

  useEffect(() => {
    getProjects(false).then(res => setProjects(res.projects)).catch(() => {})
  }, [])

  useEffect(() => {
    function handleClickOutside(e: MouseEvent) {
      if (dropdownRef.current && !dropdownRef.current.contains(e.target as Node)) {
        setDropdownOpen(false)
      }
    }
    document.addEventListener('mousedown', handleClickOutside)
    return () => document.removeEventListener('mousedown', handleClickOutside)
  }, [])

  // re-render when name mode changes (from Overview toggle)
  useEffect(() => {
    const handler = () => setTick(t => t + 1)
    window.addEventListener('namemode-changed', handler)
    return () => window.removeEventListener('namemode-changed', handler)
  }, [])

  const handleProjectSelect = (dirName: string) => {
    setDropdownOpen(false)
    navigate(`/project/${encodeURIComponent(dirName)}`)
  }

  const handleSearch = (e: React.FormEvent) => {
    e.preventDefault()
    if (searchQuery.trim() && currentProject) {
      navigate(`/project/${encodeURIComponent(currentProject)}?tab=search&q=${encodeURIComponent(searchQuery.trim())}`)
    }
  }

  const mode = getNameMode()
  const displayName = currentProject
    ? formatProjectName(currentProject, undefined, mode)
    : null

  return (
    <header className="h-12 bg-gray-50 dark:bg-gray-900 border-b border-gray-200 dark:border-gray-800 flex items-center px-4 gap-4 shrink-0">
      {/* Logo */}
      <Link to="/" className="flex items-center gap-2 text-indigo-400 hover:text-indigo-300 shrink-0">
        <IconStack2 size={22} />
        <span className="font-semibold text-sm hidden sm:inline">StackUnderflow</span>
      </Link>

      {/* Project Selector */}
      <div className="relative" ref={dropdownRef}>
        <button
          onClick={() => setDropdownOpen(!dropdownOpen)}
          className="flex items-center gap-1.5 text-sm text-gray-700 dark:text-gray-300 hover:text-gray-900 dark:hover:text-gray-100 bg-white dark:bg-gray-800 rounded px-2.5 py-1 max-w-[240px]"
        >
          <span className="truncate">{displayName ?? 'Select project'}</span>
          <IconChevronDown size={14} className="shrink-0" />
        </button>
        {dropdownOpen && (
          <ProjectPickerDropdown
            projects={projects}
            currentProject={currentProject}
            mode={mode}
            projectFilter={projectFilter}
            setProjectFilter={setProjectFilter}
            groupByProvider={groupByProvider}
            toggleGroupMode={toggleGroupMode}
            collapsedGroups={collapsedGroups}
            toggleGroupCollapsed={toggleGroupCollapsed}
            onSelectProject={(dir) => { handleProjectSelect(dir); setProjectFilter('') }}
            onProviderClick={(prov) => {
              addProvider(prov)
            }}
          />
        )}
      </div>

      {/* Nav Links */}
      <nav className="hidden md:flex items-center gap-1 ml-2">
        <Link
          to="/"
          className={`px-2.5 py-1 rounded text-xs font-medium ${
            location.pathname === '/'
              ? 'bg-white dark:bg-gray-800 text-indigo-400'
              : 'text-gray-600 dark:text-gray-400 hover:text-gray-800 dark:hover:text-gray-200 hover:bg-gray-100/70 dark:hover:bg-gray-800/50'
          }`}
        >
          Overview
        </Link>
        {currentProject && (
          <Link
            to={`/project/${encodeURIComponent(currentProject)}`}
            className={`px-2.5 py-1 rounded text-xs font-medium ${
              location.pathname.startsWith('/project/')
                ? 'bg-white dark:bg-gray-800 text-indigo-400'
                : 'text-gray-600 dark:text-gray-400 hover:text-gray-800 dark:hover:text-gray-200 hover:bg-gray-100/70 dark:hover:bg-gray-800/50'
            }`}
          >
            Dashboard
          </Link>
        )}
      </nav>

      {/* Spacer */}
      <div className="flex-1" />

      {/* Search */}
      <form onSubmit={handleSearch} className="hidden sm:flex items-center">
        <div className="relative">
          <IconSearch size={14} className="absolute left-2 top-1/2 -translate-y-1/2 text-gray-500" />
          <input
            type="text"
            value={searchQuery}
            onChange={e => setSearchQuery(e.target.value)}
            placeholder="Search messages..."
            className="bg-white dark:bg-gray-800 border border-gray-300 dark:border-gray-700 rounded pl-7 pr-3 py-1 text-xs text-gray-700 dark:text-gray-300 placeholder-gray-500 focus:outline-none focus:border-indigo-500 w-48"
          />
        </div>
      </form>

      {/* Settings */}
      <Link
        to="/settings"
        className="p-1.5 rounded text-gray-600 dark:text-gray-400 hover:text-gray-800 dark:hover:text-gray-200 hover:bg-gray-200 dark:hover:bg-gray-800"
        title="Settings"
        aria-label="Open settings"
      >
        <IconSettings size={18} />
      </Link>

      {/* Theme Toggle */}
      <ThemeToggle />

      {/* Chat Toggle */}
      <button
        onClick={onToggleChat}
        className={`p-1.5 rounded ${
          chatOpen
            ? 'bg-indigo-600 text-white'
            : 'text-gray-600 dark:text-gray-400 hover:text-gray-800 dark:hover:text-gray-200 hover:bg-gray-200 dark:hover:bg-gray-800'
        }`}
        title="Toggle Ollama Chat"
      >
        <IconMessageChatbot size={18} />
      </button>
    </header>
  )
}

// ---------------------------------------------------------------------------
// ProjectPickerDropdown — extracted from inline render so the grouped /
// flat branches stay readable. State (search query, group toggle, collapsed
// groups) lives in the parent so closing/reopening the dropdown doesn't
// reset the user's preferences.
// ---------------------------------------------------------------------------

interface ProjectPickerDropdownProps {
  projects: Project[]
  currentProject: string | null
  mode: ReturnType<typeof getNameMode>
  projectFilter: string
  setProjectFilter: (v: string) => void
  groupByProvider: boolean
  toggleGroupMode: () => void
  collapsedGroups: Set<string>
  toggleGroupCollapsed: (key: string) => void
  onSelectProject: (dirName: string) => void
  onProviderClick: (provider: string) => void
}

function ProjectPickerDropdown({
  projects,
  currentProject,
  mode,
  projectFilter,
  setProjectFilter,
  groupByProvider,
  toggleGroupMode,
  collapsedGroups,
  toggleGroupCollapsed,
  onSelectProject,
  onProviderClick,
}: ProjectPickerDropdownProps) {
  const q = projectFilter.toLowerCase()
  const filtered = q
    ? projects.filter((p) =>
        p.dir_name.toLowerCase().includes(q) ||
        (p.display_name || '').toLowerCase().includes(q) ||
        formatProjectName(p.dir_name, 0, mode).toLowerCase().includes(q),
      )
    : projects

  const grouped = useMemo(() => {
    const m = new Map<string, Project[]>()
    for (const p of filtered) {
      const provs = (p.provider ?? '').split(',').map((s) => s.trim()).filter(Boolean)
      if (provs.length === 0) {
        const key = 'unknown'
        if (!m.has(key)) m.set(key, [])
        m.get(key)!.push(p)
      } else {
        // Multi-provider rows show up in every relevant bucket so the
        // grouping mirrors the projects-merge logic on /api/projects.
        for (const prov of provs) {
          const key = prov.toLowerCase()
          if (!m.has(key)) m.set(key, [])
          m.get(key)!.push(p)
        }
      }
    }
    // Sort groups by count desc; same project may appear in multiple groups
    // when it spans providers — that's expected.
    return Array.from(m.entries()).sort((a, b) => b[1].length - a[1].length)
  }, [filtered])

  return (
    <div className="absolute top-full left-0 mt-1 w-80 bg-white dark:bg-gray-800 border border-gray-300 dark:border-gray-700 rounded-lg shadow-xl z-50 flex flex-col max-h-96">
      <div className="p-2 border-b border-gray-300 dark:border-gray-700 space-y-2">
        <input
          type="text"
          value={projectFilter}
          onChange={(e) => setProjectFilter(e.target.value)}
          placeholder="Search projects..."
          autoFocus
          className="w-full bg-gray-50 dark:bg-gray-900 border border-gray-400 dark:border-gray-600 rounded px-2.5 py-1.5 text-xs text-gray-700 dark:text-gray-300 placeholder-gray-500 focus:outline-none focus:border-indigo-500"
        />
        <div className="flex items-center justify-between text-[11px]">
          <span className="text-gray-500">
            {filtered.length} project{filtered.length === 1 ? '' : 's'}
          </span>
          <button
            type="button"
            onClick={toggleGroupMode}
            className={`px-1.5 py-0.5 rounded border transition-colors ${
              groupByProvider
                ? 'border-indigo-500 bg-indigo-500/15 text-indigo-700 dark:text-indigo-300'
                : 'border-gray-300 dark:border-gray-600 bg-white dark:bg-gray-900 text-gray-600 dark:text-gray-400 hover:text-gray-800 dark:hover:text-gray-200'
            }`}
            title="Toggle group-by-provider (persists across sessions)"
          >
            {groupByProvider ? '∴ Grouped' : '· Flat'}
          </button>
        </div>
      </div>
      <div className="overflow-y-auto flex-1">
        {filtered.length === 0 ? (
          <div className="px-3 py-3 text-xs text-gray-500 text-center">No matches</div>
        ) : groupByProvider ? (
          grouped.map(([providerKey, items]) => {
            const collapsed = collapsedGroups.has(providerKey)
            return (
              <div key={providerKey}>
                <div className="flex items-center justify-between px-2 py-1.5 bg-gray-100/70 dark:bg-gray-900/70 sticky top-0 z-10 border-b border-gray-200 dark:border-gray-800">
                  <button
                    type="button"
                    onClick={() => toggleGroupCollapsed(providerKey)}
                    className="flex items-center gap-1.5 text-xs text-gray-700 dark:text-gray-300 hover:text-gray-900 dark:hover:text-gray-100"
                  >
                    {collapsed ? <IconChevronRight size={12} /> : <IconChevronDown size={12} />}
                    <Badge color={getProviderColor(providerKey)} size="sm">
                      {getProviderLabel(providerKey)}
                    </Badge>
                    <span className="text-gray-500 tabular-nums">({items.length})</span>
                  </button>
                  <button
                    type="button"
                    onClick={(e) => {
                      e.stopPropagation()
                      onProviderClick(providerKey)
                    }}
                    className="text-[10px] text-indigo-600 dark:text-indigo-400 hover:underline"
                    title={`Filter dashboard to ${getProviderLabel(providerKey)} only`}
                  >
                    filter
                  </button>
                </div>
                {!collapsed && items.map((p, i) => (
                  <button
                    key={`${providerKey}-${p.dir_name}`}
                    onClick={() => onSelectProject(p.dir_name)}
                    className={`w-full text-left px-3 py-2 text-sm hover:bg-gray-300 dark:hover:bg-gray-700 ${
                      p.dir_name === currentProject ? 'text-indigo-400 bg-gray-200/50 dark:bg-gray-700/50' : 'text-gray-700 dark:text-gray-300'
                    }`}
                  >
                    <div className="truncate">{formatProjectName(p.dir_name, i, mode)}</div>
                    <div className="text-xs text-gray-500">{p.file_count} files</div>
                  </button>
                ))}
              </div>
            )
          })
        ) : (
          filtered.map((p, i) => (
            <button
              key={p.dir_name}
              onClick={() => onSelectProject(p.dir_name)}
              className={`w-full text-left px-3 py-2 text-sm hover:bg-gray-300 dark:hover:bg-gray-700 ${
                p.dir_name === currentProject ? 'text-indigo-400 bg-gray-200/50 dark:bg-gray-700/50' : 'text-gray-700 dark:text-gray-300'
              }`}
            >
              <div className="truncate">{formatProjectName(p.dir_name, i, mode)}</div>
              <div className="text-xs text-gray-500">{p.file_count} files</div>
            </button>
          ))
        )}
      </div>
    </div>
  )
}
