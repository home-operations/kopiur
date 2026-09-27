import { Outlet, createRootRoute } from "@tanstack/react-router";
import { Compass } from "lucide-react";

import { AppShell } from "../components/AppShell";
import { EmptyState } from "../components/EmptyState";
import { namespaceFromSearch } from "../util/namespace";

/** Search params every route shares: the namespace scope. Absent means cluster-wide. */
export interface RootSearch {
  namespace?: string;
}

export const Route = createRootRoute({
  validateSearch: (search: Record<string, unknown>): RootSearch => {
    const namespace = namespaceFromSearch(search);
    return namespace === undefined ? {} : { namespace };
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
      Nothing is served at this address. The sections in the rail are every view kopiur-ui has; a
      deep link into a snapshot, policy or restore carries its namespace and name in the path.
    </EmptyState>
  );
}
