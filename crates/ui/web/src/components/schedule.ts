/**
 * Reading a `ScheduleRow`: what it fires, and what it has been doing.
 *
 * As with policies, a schedule has no operator health: no phase, no
 * condition the server folds into a lamp. The only lamp these screens light
 * is the suspended one, which is `spec.schedule.suspend` rendered rather than
 * a verdict derived. A run of failures is loud as a *count*, in the failed
 * lamp's ink, because the number is the fact and turning it into a lamp would
 * be this bundle inventing a health the operator never published.
 */

import type { ScheduleRow } from "../api/types";

/**
 * What this schedule fires, in the terms it was written in.
 *
 * A `policyRef` names one policy in the schedule's own namespace — the CRD
 * gives a schedule no way to reach one elsewhere. A `policySelector` is
 * shown as the selector, and the server's answer to "which policies does it
 * actually match" is **exact**: `fires_policy` evaluates it with the
 * operator's own matcher, `matchExpressions` included, which is why the
 * policy detail can list its schedules without hedging.
 *
 * A schedule that sets neither fires nothing, and says so — the shape is
 * representable and reads as a mistake worth naming rather than a blank.
 */
export function scheduleFires(row: ScheduleRow): string {
  const named = row.policy;
  if (named !== null && named !== undefined && named.length > 0) {
    return named;
  }
  const selector = row.policySelector;
  if (selector !== null && selector !== undefined && selector.length > 0) {
    return selector;
  }
  return "nothing — it names no policy and sets no selector";
}

/** Whether the schedule points at policies by label rather than by name. */
export function firesBySelector(row: ScheduleRow): boolean {
  const named = row.policy;
  if (named !== null && named !== undefined && named.length > 0) {
    return false;
  }
  const selector = row.policySelector;
  return selector !== null && selector !== undefined && selector.length > 0;
}

/**
 * The cron as written, jitter token and all.
 *
 * `H` is deterministic per schedule but resolved by the operator, so
 * rewriting it here would show a reader a time they cannot find in their own
 * manifest. The server keeps it verbatim for the same reason.
 */
export function scheduleCron(row: ScheduleRow): string {
  const zone = row.timezone;
  return zone !== null && zone !== undefined && zone.length > 0
    ? `${row.cron} (${zone})`
    : row.cron;
}

/**
 * One sentence for a schedule's state, never asserting health.
 *
 * Suspension first, because nothing else about the schedule is happening
 * while it holds. Then the failure run, which is a count the operator wrote.
 */
export function scheduleSummary(row: ScheduleRow): string {
  if (row.suspended) {
    return `Suspended — ${scheduleCron(row)} is not being evaluated.`;
  }
  if (row.consecutiveFailures > 0) {
    const runs = row.consecutiveFailures === 1 ? "run" : "runs";
    return `${row.consecutiveFailures.toString()} ${runs} have failed since the last success.`;
  }
  return `Firing on ${scheduleCron(row)}.`;
}
