import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

const SERVER_PORT = process.env.SERVER_PORT ?? "8787";

// Dev: `npm --workspace web run dev` proxies /api to the Fastify server.
// Build: emits ./dist, which the Fastify server serves in the demo.
export default defineConfig({
  base: "./",
  plugins: [react()],
  server: {
    port: 5173,
    proxy: { "/api": `http://localhost:${SERVER_PORT}` },
  },
  build: { outDir: "dist", emptyOutDir: true },
});
