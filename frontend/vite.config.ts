import path from "path"
import react from "@vitejs/plugin-react"
import { defineConfig } from "vite"

export default defineConfig({
  base: './',
  plugins: [react()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
  // Tauri dev server settings
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
    // cargo writes PDB/lock files under src-tauri/target while the Tauri dev
    // build runs; watching it crashes the dev server with EBUSY on Windows.
    watch: {
      ignored: ['**/src-tauri/**'],
    },
    // The engine ships no CORS layer, and Phase 1 does not add one. Same-origin
    // proxying keeps the browser talking to one origin; SSE passes through.
    proxy: {
      '/eros': {
        target: 'http://127.0.0.1:8080',
        changeOrigin: true,
        rewrite: (path) => path.replace(/^\/eros/, ''),
      },
    },
  },
  envPrefix: ['VITE_', 'TAURI_'],
});
