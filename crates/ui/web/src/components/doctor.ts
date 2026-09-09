import { CircleCheck, CircleHelp, OctagonX, TriangleAlert } from "lucide-react";

import type { DoctorCheckView } from "../api/types";
import type { Lamp } from "./health";

/**
 * Pure helpers behind the doctor route.
 *
 * `DoctorCheckView.outcome` is a `String` on the wire, not an enum
 * (ui-model doc: "stringly-typed on purpose"), so there is no exhaustive
 * switch to lean on: the three words the server writes today are matched
 * exactly and anything else is the unknown lamp carrying the raw word — an
 * outcome this bundle cannot read must never render as a pass.
 */
export function doctorOutcomeLamp(outcome: string): Lamp {
  switch (outcome) {
    case "Pass":
      return { key: "healthy", word: "Pass", icon: CircleCheck };
    case "Warn":
      return { key: "degraded", word: "Warn", icon: TriangleAlert };
    case "Fail":
      return { key: "failed", word: "Fail", icon: OctagonX };
    default:
      console.warn(`kopiur-ui: unrecognised doctor outcome ${JSON.stringify(outcome)}`);
      return { key: "unknown", word: outcome.length > 0 ? outcome : "Unknown", icon: CircleHelp };
  }
}

/**
 * Whether a warning is doctor degrading a check it may not perform as the
 * caller: `kopiur_ops::doctor::warn_for` writes `cannot <verb> <resource>
 * (RBAC); grant …`, and the admission probe writes its own `(RBAC)` line.
 * The marker is the server's; the console only recognises it so the row can
 * say that the console asked as the signed-in user and lacked the grant.
 */
export function isRbacDegraded(check: DoctorCheckView): boolean {
  return check.outcome === "Warn" && (check.what ?? "").includes("(RBAC)");
}

/** How a report reads at a glance: counts per outcome. */
export interface DoctorSummary {
  pass: number;
  /**
   * Warnings **about the cluster**. Excludes [`DoctorSummary.rbac`]: those
   * two are different facts and a screen that adds them up says something
   * untrue about whichever one it names.
   */
  warn: number;
  /**
   * Checks that warned only because the console ran them as the signed-in
   * user and the cluster refused. Not a verdict on the cluster — a gap in
   * what this viewer could have verified.
   */
  rbac: number;
  fail: number;
  /** Outcomes this bundle could not read. */
  other: number;
  total: number;
}

/**
 * Counted from the checks themselves rather than read from `exitCode`, which
 * is a two-state flag (`0` unless something failed) and so cannot tell "all
 * clear" from "passed, but two checks could not run".
 *
 * `warn` and `rbac` are counted apart on purpose. A warning doctor issued
 * because the *viewer* lacks a grant is not a warning about the cluster, and
 * folding the two together told a read-only operator on a healthy cluster
 * that it was degraded — with the detail suppressed from the overview, so
 * nothing on that screen could correct the impression.
 */
export function summarizeDoctor(checks: readonly DoctorCheckView[]): DoctorSummary {
  const summary: DoctorSummary = {
    pass: 0,
    warn: 0,
    rbac: 0,
    fail: 0,
    other: 0,
    total: checks.length,
  };
  for (const check of checks) {
    switch (check.outcome) {
      case "Pass":
        summary.pass += 1;
        break;
      case "Warn":
        if (isRbacDegraded(check)) {
          summary.rbac += 1;
        } else {
          summary.warn += 1;
        }
        break;
      case "Fail":
        summary.fail += 1;
        break;
      default:
        summary.other += 1;
    }
  }
  return summary;
}

/**
 * The server's defaults (`crates/ui/src/api/doctor.rs`), as the CLI spells
 * them: an empty control means exactly this.
 */
export const DOCTOR_DEFAULTS = { stuckThreshold: "1h", failureLookback: "24h" } as const;
