/**
 * Reading a `RetentionPlan` — the one screen in this console where a
 * misreading costs a restore point.
 *
 * The plan is the *prune's own* answer: the population, the bucketing and the
 * selection all come from `kopiur_api::retention`, the same code
 * `kopiur_controller::snapshot_policy::backups_to_delete` runs. Nothing here
 * recomputes a verdict; this module only decides how the server's answer is
 * worded. That division is the point — a second implementation in TypeScript
 * could disagree with what the operator will actually delete.
 *
 * Three wordings this module exists to get right.
 *
 * **`unbounded` is "no retention configured", never "will be pruned."** With
 * no `spec.retention` the operator runs no selection at all, so every
 * candidate comes back `kept: true` with an empty `rules` list. Reading that
 * empty list as "no rule holds it" would invert the fact exactly (addenda item
 * 21), so the flag is checked first, everywhere.
 *
 * **A prune is not a failure.** A GFS policy is *supposed* to drop older
 * snapshots; a bucket under `keepDaily: 7` legitimately prunes most of its
 * rows. Colouring each of those with the failed lamp would make the screen a
 * wall of alarm and hide the one row that matters. Pruned rows take a neutral
 * lamp with a distinct icon and the word "Pruned"; the single loud statement
 * is the verdict line, and only when the *subject* is the row being dropped.
 *
 * **The whole rule list is shown, as text.** `pinned` is a reason and is not
 * necessarily the only one — `["pinned", "keepDaily slot 1"]` says unpinning
 * would not lose the snapshot, which is exactly what a reader needs. The slot
 * (`keepDaily slot 3` = the third-newest day that rule holds) says how close a
 * restore point is to ageing out, so it is on the row, not behind a hover.
 */

import { CircleCheck, CircleHelp, Trash2 } from "lucide-react";

import type { RetentionBucket, RetentionCandidate, RetentionPlan } from "../api/types";
import { type Lamp, healthLamp } from "./health";

/** Kept: the healthy lamp, with a word that answers "will I still have it?". */
const KEPT_LAMP: Lamp = { ...healthLamp("healthy"), word: "Kept", icon: CircleCheck };

/**
 * Pruned: a neutral field under a distinct icon.
 *
 * Deliberately not the failed or degraded lamp — see the module doc. The icon
 * and the word carry the meaning, as every lamp on this console does.
 */
const PRUNED_LAMP: Lamp = { ...healthLamp("suspended"), word: "Pruned", icon: Trash2 };

/** One candidate's verdict, with the rule spelled out for the row to show. */
export interface CandidateVerdict {
  kept: boolean;
  lamp: Lamp;
  /** Why, in words: the holding rules, or why nothing holds it. */
  rule: string;
}

/**
 * What today's selection does to one candidate.
 *
 * `unbounded` is the plan's flag and must be passed: it changes the meaning of
 * an empty `rules` list from "nothing holds it" to "nothing prunes anything".
 */
export function candidateVerdict(
  candidate: RetentionCandidate,
  unbounded: boolean,
): CandidateVerdict {
  if (unbounded) {
    return {
      kept: true,
      lamp: KEPT_LAMP,
      rule: "no GFS retention is configured on this policy, so nothing is dropped",
    };
  }
  if (!candidate.kept) {
    return {
      kept: false,
      lamp: PRUNED_LAMP,
      rule: "No rule keeps it — the next retention run removes this snapshot",
    };
  }
  if (candidate.rules.length === 0) {
    // The server attributes a rule to every ordinary keep, so an empty list on
    // a kept row means this build could not be told why. Saying so is better
    // than presenting an unexplained keep as a guarantee.
    return {
      kept: true,
      lamp: KEPT_LAMP,
      rule: "kept, but no rule was attributed — read the policy's retention block",
    };
  }
  return { kept: true, lamp: KEPT_LAMP, rule: candidate.rules.join(", ") };
}

/**
 * The heading for one bucket.
 *
 * The key is **opaque** — the controller's grouping token (the backup source,
 * plus the pinned repository while the policy is multi-repo) — so it is
 * printed verbatim and never parsed into parts. An empty key is the single
 * bucket of an un-fanned, single-repository policy, which has no token to
 * show and so gets a sentence instead.
 */
export function bucketLabel(key: string): string {
  return key.length === 0 ? "Every snapshot of this policy" : key;
}

/** Whether a bucket key names a fan-out group rather than the whole policy. */
export function isFanOutBucket(key: string): boolean {
  return key.length > 0;
}

/** What a plan adds up to across all of its buckets. */
export interface PlanTotals {
  buckets: number;
  candidates: number;
  kept: number;
  pruned: number;
}

/**
 * Kept and pruned across every bucket.
 *
 * Counted from `kept`, which the server has already resolved for an unbounded
 * plan (every candidate `true`) — so this never has to re-apply the flag.
 */
export function planTotals(plan: RetentionPlan): PlanTotals {
  let kept = 0;
  let candidates = 0;
  for (const bucket of plan.buckets) {
    for (const candidate of bucket.candidates) {
      candidates += 1;
      if (candidate.kept) {
        kept += 1;
      }
    }
  }
  return { buckets: plan.buckets.length, candidates, kept, pruned: candidates - kept };
}

/** The bucket the requested snapshot competes in, if the server flagged one. */
export function subjectBucket(plan: RetentionPlan): RetentionBucket | undefined {
  return plan.buckets.find((bucket) => bucket.candidates.some((c) => c.subject));
}

/** The requested snapshot's own row, if the server flagged one. */
export function subjectCandidate(plan: RetentionPlan): RetentionCandidate | undefined {
  for (const bucket of plan.buckets) {
    const found = bucket.candidates.find((c) => c.subject);
    if (found !== undefined) {
      return found;
    }
  }
  return undefined;
}

/** The lamp and the one sentence above the buckets. */
export interface PlanVerdict {
  lamp: Lamp;
  text: string;
}

/**
 * The plan's answer to "will I still have this restore point?", in one
 * sentence — the single loud statement on a deliberately calm screen.
 *
 * The degraded lamp appears here and nowhere else on this screen, and only
 * when the *subject* is the row being dropped: that is the fact worth an alarm,
 * and spending the lamp on it is what keeps it legible.
 */
export function planVerdict(plan: RetentionPlan): PlanVerdict {
  const subject = subjectCandidate(plan);
  const bucket = subjectBucket(plan);
  const competitors = bucket === undefined ? 0 : bucket.candidates.length - 1;
  const company = competitors === 1 ? "1 other snapshot" : `${String(competitors)} others`;

  if (plan.unbounded) {
    const totals = planTotals(plan);
    return {
      lamp: KEPT_LAMP,
      text: `Kept: no GFS retention is configured on SnapshotPolicy ${plan.policy.name}, so the operator runs no selection and none of the ${String(totals.candidates)} snapshots here is dropped by retention.`,
    };
  }

  if (subject === undefined) {
    return {
      lamp: { ...healthLamp("unknown"), word: "Unknown", icon: CircleHelp },
      text: `This snapshot does not appear in the plan the server returned, so there is no verdict to show for it. The buckets below are still SnapshotPolicy ${plan.policy.name}'s, but none of their rows is marked as this one.`,
    };
  }

  if (!subject.kept) {
    return {
      lamp: { ...healthLamp("degraded"), word: "Pruned" },
      text: `This snapshot is not kept: no retention rule holds it, so the next retention run for SnapshotPolicy ${plan.policy.name} removes it and it will no longer be restorable. It competes with ${company} in its bucket. Pin it, or widen the policy's rules, to keep it.`,
    };
  }

  const verdict = candidateVerdict(subject, false);
  return {
    lamp: KEPT_LAMP,
    text: `Kept by ${verdict.rule}. This snapshot competes with ${company} in its bucket under SnapshotPolicy ${plan.policy.name}.`,
  };
}
