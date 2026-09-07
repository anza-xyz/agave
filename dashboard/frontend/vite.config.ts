import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The build output is embedded into `agave-validator` by `dashboard/build.rs`.
// Assets must land under `assets/`, the only prefix the server caches
// immutably, and must be referenced absolutely: the server falls back to
// index.html for unknown paths, and relative asset URLs would resolve against
// that path instead of the root.
export default defineConfig({
  plugins: [react()],
  base: "/",
  build: {
    outDir: "dist",
    emptyOutDir: true,
    assetsDir: "assets",
    // One bundle keeps the embedded asset table small and avoids a waterfall of
    // requests on a validator that may be serving this over a slow link.
    rollupOptions: {
      output: {
        manualChunks: undefined,
      },
    },
  },
  server: {
    // `npm run dev` against a validator running the dashboard elsewhere.
    proxy: {
      "/websocket": {
        target: "ws://127.0.0.1:10999",
        ws: true,
      },
    },
  },
});
