import { useState } from 'react'
import { BrowserRouter, Routes, Route } from 'react-router-dom'
import Header from './components/layout/Header'
import ChatDrawer from './components/layout/ChatDrawer'
import Overview from './pages/Overview'
import ProjectDashboard from './pages/ProjectDashboard'
import QADetailPage from './pages/QADetailPage'

function AppLayout() {
  const [chatOpen, setChatOpen] = useState(false)

  return (
    <div className="h-screen w-screen bg-gray-950 flex flex-col">
      <Header onToggleChat={() => setChatOpen(v => !v)} chatOpen={chatOpen} />
      <main className="flex-1 overflow-auto">
        <Routes>
          <Route path="/" element={<Overview />} />
          <Route path="/project/:name" element={<ProjectDashboard />} />
          <Route path="/project/:name/qa/:qaId" element={<QADetailPage />} />
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
