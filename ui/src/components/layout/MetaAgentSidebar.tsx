// Permanent right-docked meta-agent sidebar.
//
// Replaces the old fixed-overlay ``ChatDrawer``. Layout contract:
//
//   * On viewports >= 1280px wide, the sidebar is a full-width docked
//     column. The collapsed state is "icon rail" — a thin strip with a
//     single button to re-expand.
//   * On viewports >= 768px (the tablet band), the sidebar collapses to
//     an icon rail by default but can still be expanded.
//   * Below 768px the sidebar hides entirely; the header chat button
//     toggles a temporary overlay (handled by ``App.tsx``).
//
// The expanded/collapsed state persists in ``localStorage``.

import { useEffect, useState, useCallback } from 'react'
import { IconChevronRight, IconChevronLeft, IconMessageChatbot } from '@tabler/icons-react'
import MetaAgentInterface from '../discussion/MetaAgentInterface'
import {
  _readPersisted,
  _writePersisted,
  _resolveInitialState,
  type SidebarState,
} from './metaAgentSidebarHelpers'

export { _resolveInitialState }

const COLLAPSED: SidebarState = 'collapsed'
const EXPANDED: SidebarState = 'expanded'
const HIDDEN: SidebarState = 'hidden'

interface MetaAgentSidebarProps {
  selectedProject: string | null
  // When the user opens the sidebar from the header on a viewport too
  // narrow for the docked layout, ``forceOverlay`` makes it render as
  // a fullscreen pane over the main content rather than a column.
  forceOverlay?: boolean
  onCloseOverlay?: () => void
}

export default function MetaAgentSidebar({
  selectedProject,
  forceOverlay,
  onCloseOverlay,
}: MetaAgentSidebarProps) {
  const [state, setState] = useState<SidebarState>(() =>
    _resolveInitialState(
      _readPersisted(),
      typeof window !== 'undefined' ? window.innerWidth : 1280,
    ),
  )

  // Track viewport — switch to hidden when the window narrows below
  // the breakpoint, restore the persisted state when it grows again.
  useEffect(() => {
    const onResize = () => {
      const w = window.innerWidth
      if (w < 768) setState(HIDDEN)
      else if (state === HIDDEN) {
        setState(_readPersisted() || (w >= 1280 ? EXPANDED : COLLAPSED))
      } else {
        // Pick up persisted-state changes triggered by the header toggle.
        const persisted = _readPersisted()
        if (persisted && persisted !== state) setState(persisted)
      }
    }
    window.addEventListener('resize', onResize)
    return () => window.removeEventListener('resize', onResize)
  }, [state])

  const expand = useCallback(() => {
    setState(EXPANDED)
    _writePersisted(EXPANDED)
  }, [])
  const collapse = useCallback(() => {
    setState(COLLAPSED)
    _writePersisted(COLLAPSED)
  }, [])

  // Overlay mode — used by the header chat button on narrow viewports.
  if (forceOverlay) {
    return (
      <div className="fixed inset-y-0 right-0 w-full max-w-md bg-white dark:bg-gray-950 border-l border-gray-200 dark:border-gray-800 shadow-2xl z-40 flex flex-col">
        <div className="flex items-center justify-between px-3 py-2 border-b border-gray-200 dark:border-gray-800">
          <span className="text-sm font-medium text-gray-700 dark:text-gray-300">
            Ask StackUnderflow
          </span>
          <button
            onClick={onCloseOverlay}
            className="p-1 text-gray-500 hover:text-gray-700 dark:hover:text-gray-300 rounded hover:bg-gray-200 dark:hover:bg-gray-800"
            aria-label="Close meta agent"
          >
            <IconChevronRight size={16} />
          </button>
        </div>
        <div className="flex-1 overflow-hidden">
          <MetaAgentInterface
            currentQA={null}
            currentSessionFile={null}
            selectedProject={selectedProject}
          />
        </div>
      </div>
    )
  }

  if (state === HIDDEN) {
    // The sidebar is suppressed entirely on this viewport. The header
    // button drives an overlay instead — App.tsx handles that.
    return null
  }

  if (state === COLLAPSED) {
    return (
      <aside
        data-testid="meta-agent-sidebar-rail"
        className="w-10 shrink-0 border-l border-gray-200 dark:border-gray-800 bg-gray-50 dark:bg-gray-900 flex flex-col items-center py-2 gap-2"
      >
        <button
          onClick={expand}
          className="p-1.5 rounded text-gray-600 dark:text-gray-400 hover:text-gray-800 dark:hover:text-gray-200 hover:bg-gray-200 dark:hover:bg-gray-800"
          title="Open Ask StackUnderflow"
          aria-label="Expand meta-agent sidebar"
        >
          <IconMessageChatbot size={18} />
        </button>
      </aside>
    )
  }

  // EXPANDED — full docked column.
  return (
    <aside
      data-testid="meta-agent-sidebar"
      className="w-96 shrink-0 border-l border-gray-200 dark:border-gray-800 bg-white dark:bg-gray-950 flex flex-col"
    >
      <header className="flex items-center gap-2 px-3 py-2 border-b border-gray-200 dark:border-gray-800">
        <IconMessageChatbot size={16} className="text-indigo-500" />
        <span className="text-sm font-medium text-gray-700 dark:text-gray-200">
          Ask StackUnderflow
        </span>
        {selectedProject && (
          <span
            className="px-1.5 py-0.5 text-[10px] bg-indigo-100 dark:bg-indigo-900/40 text-indigo-700 dark:text-indigo-300 rounded font-mono truncate max-w-[120px]"
            title={`Scoped to ${selectedProject}`}
          >
            {selectedProject}
          </span>
        )}
        <span className="flex-1" />
        <button
          onClick={collapse}
          className="p-1 rounded text-gray-500 hover:text-gray-700 dark:hover:text-gray-300 hover:bg-gray-200 dark:hover:bg-gray-800"
          title="Collapse sidebar"
          aria-label="Collapse meta-agent sidebar"
        >
          <IconChevronLeft size={14} />
        </button>
      </header>
      <div className="flex-1 overflow-hidden">
        <MetaAgentInterface
          currentQA={null}
          currentSessionFile={null}
          selectedProject={selectedProject}
        />
      </div>
    </aside>
  )
}
