import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'

// `npm run dev` talks to a running daemon (dev mode: 4778). The daemon's guard wants its own
// Host and no foreign Origin, so the proxy rewrites both.
const daemon = process.env.LANTERN_DAEMON_URL ?? 'http://127.0.0.1:4778'

export default defineConfig({
  plugins: [react(), tailwindcss()],
  build: { outDir: 'dist', emptyOutDir: true },
  server: {
    host: '127.0.0.1',
    proxy: {
      '/api': {
        target: daemon,
        changeOrigin: true,
        configure: (proxy) => proxy.on('proxyReq', (req) => req.removeHeader('origin')),
      },
    },
  },
})
