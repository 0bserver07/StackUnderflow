import { useState } from 'react'
import { BrowserRouter, Routes, Route, Link } from 'react-router-dom'
import Header from './components/layout/Header'
import ChatDrawer from './components/layout/ChatDrawer'
import Overview from './pages/Overview'
import ProjectDashboard from './pages/ProjectDashboard'
// TODO(beta-settings): swap this inline stub for `import Settings from './pages/Settings'`
// once the `beta-settings` agent's Settings page lands on feat/beta-features.
function Settings() {
  return (
    <div className="max-w-3xl mx-auto px-6 py-8 text-sm text-gray-700 dark:text-gray-300">
      <Link to="/" className="text-indigo-500 hover:underline">← Back</Link>
      <h1 className="mt-4 text-lg font-bold text-gray-900 dark:text-gray-100">Settings</h1>
      <p className="mt-2 text-gray-500">
        Settings page stub — the real page (theme, beta toggle, tab visibility, reset) is being
        assembled by the <code>beta-settings</code> agent.
      </p>
    </div>
  )
}

function AppLayout() {
  const [chatOpen, setChatOpen] = useState(false)

  return (
    <div className="h-screen w-screen bg-white dark:bg-gray-950 flex flex-col">
      <Header onToggleChat={() => setChatOpen(v => !v)} chatOpen={chatOpen} />
      <main className="flex-1 overflow-auto">
        <Routes>
          <Route path="/" element={<Overview />} />
          <Route path="/project/:name" element={<ProjectDashboard />} />
          <Route path="/settings" element={<Settings />} />
        </Routes>
      </main>
      <ChatDrawer open={chatOpen} onClose={() => setChatOpen(false)} />
    </div>
  )
}

export default function App() {
  return (
    <BrowserRouter>
      <AppLayout />
    </BrowserRouter>
  )
}
