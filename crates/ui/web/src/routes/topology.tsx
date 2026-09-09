import { createFileRoute } from "@tanstack/react-router";
import { Waypoints } from "lucide-react";

import { EmptyState } from "../components/EmptyState";

/**
 * Topology — replaced by Task 4. This placeholder exists so the rail's link
 * type-checks against the route tree; it renders the empty state until the
 * real view lands.
 */
export const Route = createFileRoute("/topology")({
  component: Placeholder,
});

function Placeholder() {
  return (
    <EmptyState title="Topology is not built in this bundle" icon={Waypoints}>
      This view will draw repositories, backends, policies and namespaces as a graph, with
      replication and seed edges between them. It arrives in Task 4 of this milestone.
    </EmptyState>
  );
}
