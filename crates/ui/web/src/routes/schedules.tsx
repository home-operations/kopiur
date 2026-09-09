import { createFileRoute } from "@tanstack/react-router";
import { CalendarClock } from "lucide-react";

import { EmptyState } from "../components/EmptyState";

/**
 * Schedules — replaced by Task 8. This placeholder exists so the rail's link
 * type-checks against the route tree; it renders the empty state until the
 * real view lands.
 */
export const Route = createFileRoute("/schedules")({
  component: Placeholder,
});

function Placeholder() {
  return (
    <EmptyState title="Schedules is not built in this bundle" icon={CalendarClock}>
      This view will list every SnapshotSchedule with its cron, next run and suspend state. It
      arrives in Task 8 of this milestone.
    </EmptyState>
  );
}
