import { createFileRoute } from "@tanstack/react-router";
import { Stethoscope } from "lucide-react";

import { EmptyState } from "../components/EmptyState";

/**
 * Doctor — replaced by Task 3. This placeholder exists so the rail's link
 * type-checks against the route tree; it renders the empty state until the
 * real view lands.
 */
export const Route = createFileRoute("/doctor")({
  component: Placeholder,
});

function Placeholder() {
  return (
    <EmptyState title="Doctor is not built in this bundle" icon={Stethoscope}>
      This view will run the operator's doctor checks, with the stuck-threshold and failure-lookback
      controls and a namespace selector. It arrives in Task 3 of this milestone.
    </EmptyState>
  );
}
