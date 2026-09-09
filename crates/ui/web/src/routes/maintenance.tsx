import { createFileRoute } from "@tanstack/react-router";
import { Wrench } from "lucide-react";

import { EmptyState } from "../components/EmptyState";

/**
 * Maintenance — replaced by Task 8. This placeholder exists so the rail's link
 * type-checks against the route tree; it renders the empty state until the
 * real view lands.
 */
export const Route = createFileRoute("/maintenance")({
  component: Placeholder,
});

function Placeholder() {
  return (
    <EmptyState title="Maintenance is not built in this bundle" icon={Wrench}>
      This view will list every Maintenance resource with its last and next run. It arrives in Task
      8 of this milestone.
    </EmptyState>
  );
}
