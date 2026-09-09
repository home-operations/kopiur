import { createFileRoute } from "@tanstack/react-router";
import { LayoutDashboard } from "lucide-react";

import { EmptyState } from "../components/EmptyState";

/**
 * Overview — replaced by Task 3 (`src/routes/index.tsx`). This placeholder
 * exists so the shell has a home route and every rail link type-checks; it
 * renders the empty state so the shell can be seen end to end.
 */
export const Route = createFileRoute("/")({
  component: OverviewPlaceholder,
});

function OverviewPlaceholder() {
  return (
    <EmptyState title="Overview is not built in this bundle" icon={LayoutDashboard}>
      This view will show repository health by count, work in flight, and the most recent failures
      with their fix. It arrives in a later task of this milestone; the rail, the identity strip,
      the theme switch and the problem banner around it are complete.
    </EmptyState>
  );
}
