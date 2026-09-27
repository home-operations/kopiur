/**
 * Reading a `RestoreRow` / `RestoreDetail`: the phase, the lamp it lights,
 * and the progress a restore can honestly report.
 *
 * # The lamp is the phase, and it is said so
 *
 * The backend computes a `Health` for repositories and for the topology's
 * replication edges, and this bundle never recomputes either. It computes
 * none for a restore — `RestoreRow` carries `phase` and nothing else — so the
 * lamp below is `status.phase` wearing a colour, not a verdict the operator
 * published. The mapping is one-to-one with the phase names the CRD defines,
 * so nothing is inferred: `failed` lights the failed lamp because the
 * operator wrote `Failed`, and a phase this bundle has never seen lights the
 * unknown lamp carrying the operator's own word, never the healthy one.
 *
 * Publishing `health` on `RestoreRow` would delete this file's lamp half; the
 * labels would stay.
 *
 * # There is no percentage, and one must not be invented
 *
 * `status.progress` carries `bytesRestored` and `filesRestored` with no total
 * to divide by — `RestoreDetail`'s own doc says so. A restore showing a
 * fabricated "80%" is worse than one showing bytes, so the counters are
 * reported as counters.
 */

import type { RestorePhaseView, RestoreRow } from "../api/types";
import { unknownVariant } from "../util/assertNever";
import { EMPTY_CELL, humanBytes } from "../util/format";
import { type Lamp, healthLamp } from "./health";

/**
 * `status.phase` as a word.
 *
 * Narrowed in the shape `crates/ui-model/src/lib.rs` prescribes (addenda item
 * 11): `typeof === "string"` first, then the single-key object, whose arm is
 * `{ unknown: { raw } }` and renders the operator's own spelling.
 */
export function restorePhaseLabel(phase: RestorePhaseView | null | undefined): string {
  if (phase === null || phase === undefined) {
    return EMPTY_CELL;
  }
  if (typeof phase === "string") {
    switch (phase) {
      case "pending":
        return "Pending";
      case "resolving":
        return "Resolving";
      case "restoring":
        return "Restoring";
      case "completed":
        return "Completed";
      case "failed":
        return "Failed";
      default:
        return unknownVariant(phase, "RestorePhaseView");
    }
  }
  return phase.unknown.raw;
}

/**
 * The lamp for a restore's phase, carrying the phase as its word.
 *
 * An absent phase is unknown, not pending: "the operator has not written one"
 * and "the operator wrote Pending" are different facts and a restore that had
 * never been looked at must not read as one that is queued.
 */
export function restorePhaseLamp(phase: RestorePhaseView | null | undefined): Lamp {
  const word = restorePhaseLabel(phase);
  if (phase === null || phase === undefined) {
    return { ...healthLamp("unknown"), word: "Not reconciled" };
  }
  if (typeof phase === "string") {
    switch (phase) {
      case "completed":
        return { ...healthLamp("healthy"), word };
      case "failed":
        return { ...healthLamp("failed"), word };
      case "restoring":
      case "resolving":
      case "pending":
        return { ...healthLamp("pending"), word };
      default:
        // Unreachable at compile time; at run time a newer operator's phase
        // renders as its own word on the unknown lamp, never as healthy.
        return { ...healthLamp("unknown"), word: unknownVariant(phase, "RestorePhaseView") };
    }
  }
  return { ...healthLamp("unknown"), word };
}

/**
 * How far a restore got, in the two counters it actually publishes.
 *
 * Absent counters are absent, not zero: a restore that has written nothing
 * yet and a restore the operator has not measured are different states.
 */
export function restoreProgress(row: RestoreRow): string {
  const bytes = row.bytesRestored;
  const files = row.filesRestored;
  if ((bytes === null || bytes === undefined) && (files === null || files === undefined)) {
    return EMPTY_CELL;
  }
  const parts: string[] = [];
  if (bytes !== null && bytes !== undefined) {
    parts.push(humanBytes(bytes));
  }
  if (files !== null && files !== undefined) {
    parts.push(`${files.toString()} ${files === 1 ? "file" : "files"}`);
  }
  return parts.join(" · ");
}

/**
 * Where a restore reads and writes, in one clause.
 *
 * `sourceKind` is the *pinned* discriminant when the run has resolved one and
 * the spec's otherwise, so it survives a spec edit; both are the server's own
 * strings and neither is re-spelled here.
 */
export function restoreRoute(row: RestoreRow): string {
  const from = row.sourceKind ?? "an unrecorded source";
  return `${from} → ${row.targetKind}`;
}

/** One sentence for the detail screen's verdict line. */
export function restoreVerdict(row: RestoreRow): { lamp: Lamp; text: string } {
  const lamp = restorePhaseLamp(row.phase);
  const where =
    row.claims.length > 0
      ? `${row.claims.length.toString()} ${row.claims.length === 1 ? "claim" : "claims"}`
      : "its target claim";
  const progress = restoreProgress(row);
  const wrote = progress === EMPTY_CELL ? "nothing measured yet" : `${progress} written`;
  return {
    lamp,
    text: `${lamp.word}: reading a ${row.sourceKind ?? "source the operator has not pinned"} into ${where} — ${wrote}.`,
  };
}
