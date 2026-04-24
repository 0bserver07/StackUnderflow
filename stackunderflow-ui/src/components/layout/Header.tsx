import { useState, useEffect, useRef } from 'react'
import { Link, useNavigate, useLocation } from 'react-router-dom'
import { IconStack2, IconSearch, IconMessageChatbot, IconChevronDown } from '@tabler/icons-react'
import { getProjects } from '../../services/api'
import { formatProjectName, getNameMode } from '../../services/nameMode'
import type { Project } from '../../types/api'
import ThemeToggle from '../common/ThemeToggle'

interface HeaderProps {
  onToggleChat: () => void
  chatOpen: boolean
}

export default function Header({ onToggleChat, chatOpen }: HeaderProps) {
  const navigate = useNavigate()
  const location = useLocation()
  const [projects, setProjects] = useState<Project[]>([])
  const [dropdownOpen, setDropdownOpen] = useState(false)
  const [searchQuery, setSearchQuery] = useState('')
  const [projectFilter, setProjectFilter] = useState('')
  const [, setTick] = useState(0) // force re-render when name mode changes
  const dropdownRef = useRef<HTMLDivElement>(null)

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
        {dropdownOpen && (() => {
          const q = projectFilter.toLowerCase()
          const filtered = q
            ? projects.filter(p =>
                p.dir_name.toLowerCase().includes(q) ||
                (p.display_name || '').toLowerCase().includes(q) ||
                formatProjectName(p.dir_name, 0, mode).toLowerCase().includes(q)
              )
            : projects
          return (
            <div className="absolute top-full left-0 mt-1 w-80 bg-white dark:bg-gray-800 border border-gray-300 dark:border-gray-700 rounded-lg shadow-xl z-50 flex flex-col max-h-96">
              <div className="p-2 border-b border-gray-300 dark:border-gray-700">
                <input
                  type="text"
                  value={projectFilter}
                  onChange={e => setProjectFilter(e.target.value)}
                  placeholder="Search projects..."
                  autoFocus
                  className="w-full bg-gray-50 dark:bg-gray-900 border border-gray-400 dark:border-gray-600 rounded px-2.5 py-1.5 text-xs text-gray-700 dark:text-gray-300 placeholder-gray-500 focus:outline-none focus:border-indigo-500"
                />
              </div>
              <div className="overflow-y-auto flex-1">
                {filtered.map((p, i) => (
                  <button
                    key={p.dir_name}
                    onClick={() => { handleProjectSelect(p.dir_name); setProjectFilter('') }}
                    className={`w-full text-left px-3 py-2 text-sm hover:bg-gray-300 dark:hover:bg-gray-700 ${
                      p.dir_name === currentProject ? 'text-indigo-400 bg-gray-200/50 dark:bg-gray-700/50' : 'text-gray-700 dark:text-gray-300'
                    }`}
                  >
                    <div className="truncate">{formatProjectName(p.dir_name, i, mode)}</div>
                    <div className="text-xs text-gray-500">{p.file_count} files</div>
                  </button>
                ))}
                {filtered.length === 0 && (
                  <div className="px-3 py-3 text-xs text-gray-500 text-center">No matches</div>
                )}
              </div>
            </div>
          )
        })()}
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
