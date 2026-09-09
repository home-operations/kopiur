import { createFileRoute } from "@tanstack/react-router";
import { ScrollText } from "lucide-react";

import { EmptyState } from "../components/EmptyState";

/**
 * Policies — replaced by Task 8. This placeholder exists so the rail's link
 * type-checks against the route tree; it renders the empty state until the
 * real view lands.
 */
export const Route = createFileRoute("/policies")({
  component: Placeholder,
});

function Placeholder() {
  return (
    <EmptyState title="Policies is not built in this bundle" icon={ScrollText}>
      This view will list every SnapshotPolicy with its repositories, retention and suspend state.
      It arrives in Task 8 of this milestone.
    </EmptyState>
  );
}
