import React from 'react'
import ReactDOM from 'react-dom/client'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import App from './App'
import ErrorBoundary from './components/common/ErrorBoundary'
import './index.css'

// Apply the persisted theme class before React's first paint so there is no
// flash of the wrong theme. Must stay in sync with the constants in
// `hooks/useTheme.ts`.
;(function applyInitialTheme() {
  try {
    const stored = window.localStorage.getItem('suf:theme')
    const theme = stored === 'light' ? 'light' : 'dark'
    if (theme === 'light') {
      document.documentElement.classList.remove('dark')
    } else {
      document.documentElement.classList.add('dark')
    }
  } catch {
    // localStorage unavailable — fall back to the default (dark).
    document.documentElement.classList.add('dark')
  }
})()

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      staleTime: 30_000,
      retry: 1,
      refetchOnWindowFocus: false,
    },
  },
})

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
      <ErrorBoundary>
        <App />
      </ErrorBoundary>
    </QueryClientProvider>
  </React.StrictMode>
)
