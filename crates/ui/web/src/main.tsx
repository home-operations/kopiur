import { QueryClientProvider } from "@tanstack/react-query";
import { RouterProvider, createRouter } from "@tanstack/react-router";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { createQueryClient } from "./api/queryClient";
import { routeTree } from "./routeTree.gen";
import "./styles.css";
import { initTheme } from "./util/theme";

// Before the first paint: the stored theme override, if any. The backend's
// CSP forbids an inline script in index.html, so this is the earliest point.
initTheme();

const router = createRouter({
  routeTree,
  defaultPreload: "intent",
  // A stale bundle after a release: the chunk a route needs is gone. Reload
  // once rather than showing a broken route.
  defaultPreloadStaleTime: 0,
});

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}

const queryClient = createQueryClient();

const container = document.getElementById("root");
if (container === null) {
  throw new Error("index.html has no #root element");
}

createRoot(container).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>
  </StrictMode>,
);
