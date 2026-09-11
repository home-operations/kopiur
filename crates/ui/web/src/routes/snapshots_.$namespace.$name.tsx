import { createFileRoute } from "@tanstack/react-router";

export const Route = createFileRoute("/snapshots_/$namespace/$name")({
  component: () => null,
});
