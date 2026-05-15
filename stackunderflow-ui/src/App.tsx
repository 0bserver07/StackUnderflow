import { useState, useEffect } from 'react'
import { BrowserRouter, Routes, Route, useLocation } from 'react-router-dom'
import Header from './components/layout/Header'
import MetaAgentSidebar from './components/layout/MetaAgentSidebar'
import Overview from './pages/Overview'
import ProjectDashboard from './pages/ProjectDashboard'
import Settings from './pages/Settings'
import { CurrencyProvider } from './services/currency'
import { FiltersProvider } from './services/filters'

function useCurrentProject(): string | null {
  const location = useLocation()
  const match = location.pathname.match(/^\/project\/(.+?)(?:\/|$)/)
  return match ? decodeURIComponent(match[1]!) : null
}

function AppLayout() {
  const [overlayOpen, setOverlayOpen] = useState(false)
  const [isNarrow, setIsNarrow] = useState<boolean>(() =>
    typeof window === 'undefined' ? false : window.innerWidth < 768,
  )
  const currentProject = useCurrentProject()

  // Track viewport so the header's chat button switches between
  // "expand sidebar" (when docked) and "open overlay" (when hidden).
  useEffect(() => {
    const onResize = () => setIsNarrow(window.innerWidth < 768)
    window.addEventListener('resize', onResize)
    return () => window.removeEventListener('resize', onResize)
  }, [])

  // The header button: on narrow viewports it opens the overlay; on
  // wider viewports it toggles the sidebar's persisted expanded state
  // via a custom event so MetaAgentSidebar handles it itself.
  const handleToggleChat = () => {
    if (isNarrow) {
      setOverlayOpen((v) => !v)
    } else {
      // Force re-expand: the sidebar reads its persisted state on
      // mount; flip it here.
      try {
        const cur = localStorage.getItem('stackunderflow_metaAgentSidebar')
        const next = cur === 'expanded' ? 'collapsed' : 'expanded'
        localStorage.setItem('stackunderflow_metaAgentSidebar', next)
      } catch {
        // ignore
      }
      // Trigger a one-off resize to make the sidebar pick up the new
      // state without a custom hook.
      window.dispatchEvent(new Event('resize'))
    }
  }

  return (
    <div className="h-screen w-screen bg-white dark:bg-gray-950 flex flex-col">
      <Header onToggleChat={handleToggleChat} chatOpen={overlayOpen} />
      <div className="flex-1 flex overflow-hidden min-h-0">
        <main className="flex-1 overflow-auto min-w-0">
          <Routes>
            <Route path="/" element={<Overview />} />
            <Route path="/project/:name" element={<ProjectDashboard />} />
            <Route path="/settings" element={<Settings />} />
          </Routes>
        </main>
        <MetaAgentSidebar
          selectedProject={currentProject}
          forceOverlay={isNarrow && overlayOpen}
          onCloseOverlay={() => setOverlayOpen(false)}
        />
      </div>
    </div>
  )
}

export default function App() {
  return (
    <BrowserRouter>
      <CurrencyProvider>
        <FiltersProvider>
          <AppLayout />
        </FiltersProvider>
      </CurrencyProvider>
    </BrowserRouter>
  )
}
