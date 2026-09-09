import { tanstackRouter } from "@tanstack/router-plugin/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

export default defineConfig({
  plugins: [
    // File-based routing: every file under src/routes becomes a route and the
    // plugin regenerates src/routeTree.gen.ts (checked in). autoCodeSplitting
    // makes each route its own lazy chunk, which is what keeps the topology
    // route (and, through manualChunks below, elkjs) out of the entry bundle.
    tanstackRouter({
      target: "react",
      autoCodeSplitting: true,
      routesDirectory: "src/routes",
      generatedRouteTree: "src/routeTree.gen.ts",
    }),
    react(),
  ],
  server: {
    proxy: {
      // A local `kopiur-ui` on :8090. `changeOrigin` stays false on purpose:
      // the backend's CSRF gate compares the request's Origin against its
      // Host, and rewriting Host to 127.0.0.1:8090 would make every mutation
      // from the dev server a cross-origin refusal.
      "/api": { target: "http://127.0.0.1:8090", changeOrigin: false },
    },
  },
  build: {
    outDir: "dist",
    rollupOptions: {
      output: {
        // Only the topology route touches elkjs; keep it in a chunk of its
        // own so the lazy route stays lazy. What lands here is the ~10 kB
        // `elk-api` client — the 1.4 MB engine is imported only by
        // `topology/layout.worker.ts` and is emitted as that worker's own
        // asset, which never enters this graph at all.
        manualChunks(id: string) {
          if (id.includes("/node_modules/elkjs/")) {
            return "elk";
          }
          return undefined;
        },
      },
    },
  },
  worker: { format: "es" },
  test: {
    environment: "jsdom",
    setupFiles: ["src/test-setup.ts"],
    include: ["src/**/*.test.{ts,tsx}"],
    css: false,
  },
});
