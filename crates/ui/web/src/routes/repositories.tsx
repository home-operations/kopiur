import { createFileRoute } from "@tanstack/react-router";
import { Database } from "lucide-react";

import { EmptyState } from "../components/EmptyState";

/**
 * Repositories — replaced by Task 5. This placeholder exists so the rail's link
 * type-checks against the route tree; it renders the empty state until the
 * real view lands.
 */
export const Route = createFileRoute("/repositories")({
  component: Placeholder,
});

function Placeholder() {
  return (
    <EmptyState title="Repositories is not built in this bundle" icon={Database}>
      This view will list every Repository and ClusterRepository with its phase, health, mode,
      storage and gates. It arrives in Task 5 of this milestone.
    </EmptyState>
  );
}
