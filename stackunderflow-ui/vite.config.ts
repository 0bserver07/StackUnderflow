import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5175,
    proxy: {
      '/api': {
        target: 'http://localhost:8095',
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
  },
})
