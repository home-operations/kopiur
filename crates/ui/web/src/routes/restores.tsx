import { createFileRoute } from "@tanstack/react-router";
import { ArchiveRestore } from "lucide-react";

import { EmptyState } from "../components/EmptyState";

/**
 * Restores — replaced by Task 8. This placeholder exists so the rail's link
 * type-checks against the route tree; it renders the empty state until the
 * real view lands.
 */
export const Route = createFileRoute("/restores")({
  component: Placeholder,
});

function Placeholder() {
  return (
    <EmptyState title="Restores is not built in this bundle" icon={ArchiveRestore}>
      This view will list every Restore with its source, target and phase. It arrives in Task 8 of
      this milestone.
    </EmptyState>
  );
}
