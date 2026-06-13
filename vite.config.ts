import { defineConfig } from "vite";

// Treelens uses Vite as the UI build. Source lives in ./ui, output goes to ./ui/dist,
// which is what tauri.conf.json's frontendDist points to.
export default defineConfig({
  root: "ui",
  publicDir: "../public",
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
    host: "127.0.0.1",
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    target: ["chrome120", "edge120"],
    sourcemap: false,
    minify: "esbuild",
  },
});
