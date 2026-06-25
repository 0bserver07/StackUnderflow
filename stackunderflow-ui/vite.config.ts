import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5175,
    proxy: {
      '/api': {
        target: 'http://localhost:8081',
        changeOrigin: true,
      },
      '/ollama-api': {
        target: 'http://localhost:11434',
        changeOrigin: true,
        rewrite: (path) => path.replace(/^\/ollama-api/, '/api'),
        configure: (proxy) => {
          proxy.on('error', (_err, _req, res) => {
            // Silently handle Ollama connection errors (it's optional)
            if ('writeHead' in res && typeof res.writeHead === 'function') {
              res.writeHead(502, { 'Content-Type': 'application/json' })
              res.end(JSON.stringify({ error: 'Ollama not available' }))
            }
          })
        },
      },
    },
  },
  build: {
    outDir: '../stackunderflow/static/react',
    emptyOutDir: true,
    rollupOptions: {
      output: {
        // Split the heaviest third-party libs into their own chunks so they
        // (a) download in parallel instead of serializing behind one 1.86MB
        // file, (b) cache independently of app code, and (c) — combined with
        // the React.lazy tab splitting in ProjectDashboard — stay off the
        // first-paint critical path when only reachable through a lazy tab.
        // Order matters: the specific packages (react-syntax-highlighter,
        // react-markdown) are matched before the generic `react` rule so they
        // don't get swept into react-vendor. Anything not matched returns
        // undefined and falls back to Rollup's default chunking, which keeps
        // async-only modules in async chunks (never forced eager).
        manualChunks(id) {
          if (!id.includes('node_modules')) return undefined
          // Syntax highlighting (react-syntax-highlighter + refractor/prism).
          if (/[\\/]node_modules[\\/](react-syntax-highlighter|refractor|highlight\.js|lowlight|prismjs|parse-entities|character-entities[\w-]*|fault|format)[\\/]/.test(id)) {
            return 'syntax-highlighter'
          }
          // Charting (recharts pulls in the d3-* family + victory-vendor).
          if (/[\\/]node_modules[\\/](recharts|d3-[\w-]+|victory-vendor|internmap|decimal\.js-light)[\\/]/.test(id)) {
            return 'recharts'
          }
          // Markdown rendering (react-markdown + the remark/micromark/mdast/
          // hast/unist unified pipeline and its many small helpers).
          if (/[\\/]node_modules[\\/](react-markdown|remark[\w-]*|micromark[\w-]*|mdast[\w-]*|hast[\w-]*|hastscript|unist[\w-]*|unified|vfile[\w-]*|property-information|[\w-]+-separated-tokens|decode-named-character-reference|trim-lines|trough|bail|is-plain-obj|devlop|zwitch|longest-streak|ccount|markdown-table|html-void-elements|web-namespaces|estree-util-is-identifier-name)[\\/]/.test(id)) {
            return 'markdown'
          }
          // Core framework — eager but stable, so it earns a long-lived cache
          // entry that app-code changes never invalidate.
          if (/[\\/]node_modules[\\/](react|react-dom|react-router|react-router-dom|scheduler|@tanstack)[\\/]/.test(id)) {
            return 'react-vendor'
          }
          return undefined
        },
      },
    },
  },
})
