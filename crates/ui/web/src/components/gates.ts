import { CircleHelp, OctagonX, TriangleAlert } from "lucide-react";

import type { GateDescriptor, GateSeverityView } from "../api/types";
import { unknownVariant } from "../util/assertNever";
import type { Lamp } from "./health";

/**
 * The lamp for a gate's severity — exhaustive over the generated union.
 *
 * `GateSeverityView` is unit-only with NO fallback variant (ui-model doc), so
 * the `default` arm is unreachable at compile time (`unknownVariant` takes
 * `never`, as `assertNever` does) and still renders the raw word at run
 * time: a newer server may add a level, and a gate it reports must not
 * vanish or read as a warning.
 */
export function gateSeverityLamp(severity: GateSeverityView): Lamp {
  switch (severity) {
    case "error":
      return { key: "failed", word: "Error", icon: OctagonX };
    case "warning":
      return { key: "degraded", word: "Warning", icon: TriangleAlert };
    default:
      return {
        key: "unknown",
        word: unknownVariant(severity, "GateSeverityView"),
        icon: CircleHelp,
      };
  }
}

/**
 * The CRD kinds a gate's condition appears on. `GateDescriptor.scope` is one
 * display string the server joins with commas (`Snapshot,Restore`); one
 * label strip per kind reads better than the joined text.
 */
export function gateScopeKinds(scope: string): string[] {
  return scope
    .split(",")
    .map((kind) => kind.trim())
    .filter((kind) => kind.length > 0);
}

/** Errors first, then warnings, then anything unrecognised; stable within a level. */
function severityRank(severity: GateSeverityView): number {
  const key = gateSeverityLamp(severity).key;
  if (key === "failed") {
    return 0;
  }
  return key === "degraded" ? 1 : 2;
}

/** A sorted copy of the registry: severity, then condition, then reason. */
export function sortGates(gates: readonly GateDescriptor[]): GateDescriptor[] {
  return [...gates].sort(
    (a, b) =>
      severityRank(a.severity) - severityRank(b.severity) ||
      a.condition.localeCompare(b.condition) ||
      a.reason.localeCompare(b.reason),
  );
}
