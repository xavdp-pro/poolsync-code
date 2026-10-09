import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'

const PUBLIC_HOST = process.env.VITE_PUBLIC_HOST || 'localhost'
const HMR_CLIENT_PORT = Number(process.env.VITE_HMR_CLIENT_PORT || 9471)

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    dedupe: ['react', 'react-dom'],
  },
  server: {
    host: '127.0.0.1',
    port: 9471,
    strictPort: true,
    origin: `http://${PUBLIC_HOST}:9471`,
    allowedHosts: [PUBLIC_HOST, 'localhost'],
    proxy: {
      '/api': 'http://127.0.0.1:9470',
      '/health': 'http://127.0.0.1:9470',
    },
    hmr: {
      host: PUBLIC_HOST,
      protocol: 'ws',
      clientPort: HMR_CLIENT_PORT,
    },
  },
  preview: {
    host: '127.0.0.1',
    port: 9471,
    strictPort: true,
    proxy: {
      '/api': 'http://127.0.0.1:9470',
      '/health': 'http://127.0.0.1:9470',
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
})
