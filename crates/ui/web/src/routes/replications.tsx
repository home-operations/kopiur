import { createFileRoute } from "@tanstack/react-router";
import { ArrowLeftRight } from "lucide-react";

import { EmptyState } from "../components/EmptyState";

/**
 * Replications — replaced by Task 5. This placeholder exists so the rail's link
 * type-checks against the route tree; it renders the empty state until the
 * real view lands.
 */
export const Route = createFileRoute("/replications")({
  component: Placeholder,
});

function Placeholder() {
  return (
    <EmptyState title="Replications is not built in this bundle" icon={ArrowLeftRight}>
      This view will list repository and snapshot replications with their lag and phase. It arrives
      in Task 5 of this milestone.
    </EmptyState>
  );
}
