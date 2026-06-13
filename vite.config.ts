import { defineConfig } from "vite";
import { readFileSync } from "node:fs";

// Treelens uses Vite as the UI build. Source lives in ./ui, output goes to ./ui/dist,
// which is what tauri.conf.json's frontendDist points to.
const pkg = JSON.parse(readFileSync("./package.json", "utf-8")) as { version: string };

export default defineConfig({
  root: "ui",
  publicDir: "../public",
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
    host: "127.0.0.1",
  },
  define: {
    // Single source of truth for the displayed version — package.json. Eliminates
    // the v0.1.0/v0.1.2/v0.1.3 drift we hit when this was hardcoded in HTML.
    __APP_VERSION__: JSON.stringify(pkg.version),
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    target: ["chrome120", "edge120"],
    sourcemap: false,
    minify: "esbuild",
  },
});
