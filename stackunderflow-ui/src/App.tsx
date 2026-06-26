import { useState, useEffect, lazy, Suspense } from 'react'
import { BrowserRouter, Routes, Route, useLocation } from 'react-router-dom'
import Header from './components/layout/Header'
import MetaAgentSidebar from './components/layout/MetaAgentSidebar'
import LoadingSpinner from './components/common/LoadingSpinner'
import { CurrencyProvider } from './services/currency'
import { FiltersProvider } from './services/filters'

// Route-level code splitting: each page becomes its own async chunk, so a
// page's own libraries (e.g. recharts on Overview/ProjectDashboard) stay
// out of the eager first-paint bundle and load only when that route is
// visited. All pages are default exports, so lazy() needs no .then shim.
// A single <Suspense> below covers the active route (only one is mounted).
const Overview = lazy(() => import('./pages/Overview'))
const ProjectDashboard = lazy(() => import('./pages/ProjectDashboard'))
const Live = lazy(() => import('./pages/Live'))
const Settings = lazy(() => import('./pages/Settings'))

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
          <Suspense fallback={<LoadingSpinner size="md" message="Loading…" />}>
            <Routes>
              <Route path="/" element={<Overview />} />
              <Route path="/live" element={<Live />} />
              <Route path="/project/:name" element={<ProjectDashboard />} />
              <Route path="/settings" element={<Settings />} />
            </Routes>
          </Suspense>
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
