import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
const SERVER_PORT = process.env.SERVER_PORT ?? "8788";
export default defineConfig({
  base: "./",
  plugins: [react()],
  server: { port: 5174, proxy: { "/api": `http://localhost:${SERVER_PORT}` } },
  build: { outDir: "dist", emptyOutDir: true },
});
