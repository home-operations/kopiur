import { Outlet, createRootRoute } from "@tanstack/react-router";
import { Compass } from "lucide-react";

import { AppShell } from "../components/AppShell";
import { EmptyState } from "../components/EmptyState";
import { inspectStackParam, parseInspectStack } from "../components/inspect";
import { namespaceFromSearch } from "../util/namespace";

/**
 * Search params every route shares: the namespace scope (absent means
 * cluster-wide) and the drawers open, bottom first (absent means none; the
 * part that does not parse is dropped, never thrown).
 */
export interface RootSearch {
  namespace?: string;
  inspect?: string;
}

export const Route = createRootRoute({
  validateSearch: (search: Record<string, unknown>): RootSearch => {
    const namespace = namespaceFromSearch(search);
    const stack = parseInspectStack(search.inspect);
    const inspect = stack.length > 0 ? inspectStackParam(stack) : undefined;
    return {
      ...(namespace === undefined ? {} : { namespace }),
      ...(inspect === undefined ? {} : { inspect }),
    };
  },
  component: RootLayout,
  notFoundComponent: NotFound,
});

function RootLayout() {
  return (
    <AppShell>
      <Outlet />
    </AppShell>
  );
}

function NotFound() {
  return (
    <EmptyState title="No such page" icon={Compass}>
      Nothing is served at this address. Pick a section from the rail.
    </EmptyState>
  );
}
