import { Outlet, createRootRoute } from "@tanstack/react-router";
import { Compass } from "lucide-react";

import { AppShell } from "../components/AppShell";
import { EmptyState } from "../components/EmptyState";
import { parseInspect } from "../components/inspect";
import { namespaceFromSearch } from "../util/namespace";

/**
 * Search params every route shares: the namespace scope (absent means
 * cluster-wide) and the resource the side panel is showing (absent means it is
 * closed; one that does not parse is dropped, never thrown).
 */
export interface RootSearch {
  namespace?: string;
  inspect?: string;
}

export const Route = createRootRoute({
  validateSearch: (search: Record<string, unknown>): RootSearch => {
    const namespace = namespaceFromSearch(search);
    const inspect = parseInspect(search.inspect) !== null ? (search.inspect as string) : undefined;
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
