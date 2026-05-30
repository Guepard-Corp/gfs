import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';

// Build output is embedded into the Rust binary (rust-embed → served by Axum).
// In dev, `/api` is proxied to the running console backend on :7070.
export default defineConfig({
  plugins: [react(), tailwindcss()],
  build: { outDir: 'dist', emptyOutDir: true },
  server: {
    port: 5273,
    proxy: {
      '/api': 'http://127.0.0.1:7070',
      '/healthz': 'http://127.0.0.1:7070',
    },
  },
});
