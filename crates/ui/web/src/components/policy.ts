/**
 * Reading a `PolicyRow` / `PolicyDetail`: the labels, the retention rules and
 * the one sentence the detail screen leads with.
 *
 * # A policy carries no operator health, and nothing here invents one
 *
 * `RepositorySummary.health` is a `Health` the backend computed; a policy has
 * no such field, no phase, and no condition the server folds into a lamp. So
 * these screens report **facts** — is it suspended, when did it last succeed,
 * when was it last verified — and the only lamp they light is the suspended
 * one, which is `spec.suspend` rendered rather than a verdict derived. A
 * phase-to-health table in this bundle is exactly what the addenda forbids,
 * and the absence of any phase to feed one is why there is not even a
 * temptation here.
 */

import type { PolicyRow, RetentionView } from "../api/types";
import { EMPTY_CELL } from "../util/format";

/**
 * The bare `metadata.name` inside a repository key.
 *
 * `PolicyRow.repositories` holds the server's own `repo_key`
 * (`kopiur_api::common::repo_key`): `Repository/<namespace>/<name>` or
 * `ClusterRepository/<name>`. `SnapshotNowBody.repository` takes the bare
 * name — it is matched against `repository_refs(&policy.spec)` by name in
 * `kopiur_ops::actions::snapshot` — so the last segment is what the fan-out
 * restriction has to send. A Kubernetes name cannot contain `/`, so the last
 * segment is unambiguous.
 */
export function repositoryName(key: string): string {
  const segments = key.split("/");
  return segments[segments.length - 1] ?? key;
}

/** One GFS rule the policy actually sets. */
export interface RetentionRule {
  /** The wire field, e.g. `keepDaily` — the name the manifest uses. */
  field: string;
  /** What it keeps, in words. */
  term: string;
  count: number;
}

/**
 * The GFS rules a policy sets, in slot order, skipping the ones it leaves
 * unset.
 *
 * An unset rule is not a zero. `keepDaily: 0` would mean "keep no daily
 * slots"; an absent `keepDaily` means the policy never mentioned dailies, and
 * the operator's own default applies. The two must not render alike, so only
 * the ones that are present are returned and the caller says "none set" for
 * an empty list.
 */
export function retentionRules(retention: RetentionView | null | undefined): RetentionRule[] {
  if (retention === null || retention === undefined) {
    return [];
  }
  const slots: readonly (readonly [string, string, number | null | undefined])[] = [
    ["keepLatest", "most recent, whatever their age", retention.keepLatest],
    ["keepHourly", "hourly slots", retention.keepHourly],
    ["keepDaily", "daily slots", retention.keepDaily],
    ["keepWeekly", "weekly slots", retention.keepWeekly],
    ["keepMonthly", "monthly slots", retention.keepMonthly],
    ["keepAnnual", "annual slots", retention.keepAnnual],
  ];
  const rules: RetentionRule[] = [];
  for (const [field, term, count] of slots) {
    if (count !== null && count !== undefined) {
      rules.push({ field, term, count });
    }
  }
  return rules;
}

/** A lamp-free verdict line: what this policy is and what it last did. */
export interface PolicyVerdict {
  /** True when the suspended lamp leads the line. */
  suspended: boolean;
  text: string;
}

/**
 * One sentence answering "what does this recipe do, and is it doing it?".
 *
 * It never asserts health. Suspension is stated because it is a spec field;
 * everything else is the last *observed* fact, and an absent one is said to
 * be absent rather than filled in — a policy that has never produced a
 * snapshot reads as never having produced one, not as fine.
 */
export function policyVerdict(row: PolicyRow): PolicyVerdict {
  const targets =
    row.repositories.length === 0
      ? "names no repository, so it has nowhere to write"
      : row.repositories.length === 1
        ? `writes into ${row.repositories.join("")}`
        : `fans out into ${row.repositories.length.toString()} repositories`;
  const last =
    row.lastSuccessfulSnapshot === null || row.lastSuccessfulSnapshot === undefined
      ? "it has never recorded a successful snapshot"
      : "its last snapshot succeeded";
  if (row.suspended) {
    return {
      suspended: true,
      text: `Suspended: it ${targets}, and no schedule will fire it until it is resumed — ${last}.`,
    };
  }
  return { suspended: false, text: `This policy ${targets}; ${last}.` };
}

/** A count where zero is a measurement and absent is not one. */
export function snapshotCount(value: number | null | undefined): string {
  return value === null || value === undefined ? EMPTY_CELL : value.toString();
}
