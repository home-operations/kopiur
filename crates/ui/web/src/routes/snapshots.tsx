import { createFileRoute } from "@tanstack/react-router";
import { Camera } from "lucide-react";

import { EmptyState } from "../components/EmptyState";

/**
 * Snapshots — replaced by Task 6. This placeholder exists so the rail's link
 * type-checks against the route tree; it renders the empty state until the
 * real view lands.
 */
export const Route = createFileRoute("/snapshots")({
  component: Placeholder,
});

function Placeholder() {
  return (
    <EmptyState title="Snapshots is not built in this bundle" icon={Camera}>
      This view will list snapshots with URL-bound filters and pagination, and open each one's
      detail, retention plan and file browser. It arrives in Task 6 of this milestone.
    </EmptyState>
  );
}
