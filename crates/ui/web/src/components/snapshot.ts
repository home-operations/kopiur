/**
 * Reading a `SnapshotRow`: the narrowings, the filter vocabularies, and the
 * two sentences the delete confirmation is built on.
 *
 * Three rules this module exists to keep.
 *
 * **A phase this build does not know is never green.** `SnapshotPhaseView` is
 * narrowed in the shape `crates/ui-model/src/lib.rs` prescribes (addenda item
 * 11): `typeof === "string"` first, then the single-key `{ unknown: { raw } }`
 * object, whose `raw` is the operator's own word. An absent phase says the
 * operator has written none rather than naming one.
 *
 * **`OriginView` has no fallback variant at all** (addenda item 11), so its
 * `default` arm has to render the raw value at run time rather than throw —
 * `unknownVariant`, not `assertNever`. A newer operator's origin reaches the
 * screen as its own string.
 *
 * **An absent `deletionPolicy` is not `Delete`.** The wire doc on
 * `SnapshotRow.deletionPolicy` spells out why: the effective policy is the
 * operator's decision at delete time (a produced backup behaves as `Delete`, a
 * discovered one is forced to `Retain`), so filling the blank would tell one
 * of those two owners the exact opposite of what will happen to their data
 * (addenda item 19). [`deletionConsequence`] therefore reports `known: false`
 * and names both outcomes instead of picking one.
 *
 * The filter vocabularies are the *server's* accepted values — the strings
 * `kopiur_ui::api::snapshots::parse_phase` and `kopiur_api::Origin::parse`
 * take — so a value the SPA puts in the URL is one the handler accepts rather
 * than one it answers 400 for.
 */

import { CircleHelp } from "lucide-react";

import type { OriginView, SnapshotPhaseView, SnapshotRow } from "../api/types";
import { unknownVariant } from "../util/assertNever";
import { EMPTY_CELL } from "../util/format";
import { type Lamp, healthLamp } from "./health";

/** A lamp carrying a phase's own word instead of the health word. */
function lampWith(key: Parameters<typeof healthLamp>[0], word: string): Lamp {
  return { ...healthLamp(key), word };
}

/**
 * The lamp for a snapshot phase — colour, icon and the phase's own word.
 *
 * `succeeded`, `discovered` and `unchanged` are the healthy lamp: all three
 * mean the run did what it was asked to. `unchanged` in particular is not a
 * failure — kopia found nothing new, so the previous snapshot still covers the
 * source; the detail's browse blocker is what says there is no new tree to
 * open.
 */
export function snapshotPhaseLamp(phase: SnapshotPhaseView | null | undefined): Lamp {
  if (phase === null || phase === undefined) {
    return { key: "unknown", word: "Unreconciled", icon: CircleHelp };
  }
  if (typeof phase === "string") {
    switch (phase) {
      case "pending":
        return lampWith("pending", "Pending");
      case "running":
        return lampWith("pending", "Running");
      case "succeeded":
        return lampWith("healthy", "Succeeded");
      case "failed":
        return lampWith("failed", "Failed");
      case "deleting":
        return lampWith("pending", "Deleting");
      case "discovered":
        return lampWith("healthy", "Discovered");
      case "unchanged":
        return lampWith("healthy", "Unchanged");
      default:
        return lampWith("unknown", unknownVariant(phase, "SnapshotPhaseView"));
    }
  }
  return lampWith("unknown", phase.unknown.raw);
}

/** The phase as a word, for a cell that has no room for a lamp. */
export function snapshotPhaseLabel(phase: SnapshotPhaseView | null | undefined): string {
  return phase === null || phase === undefined ? EMPTY_CELL : snapshotPhaseLamp(phase).word;
}

/**
 * How the snapshot came to exist.
 *
 * Exhaustive over a union with **no** fallback variant, so the default arm
 * renders the server's own string and keeps drawing.
 */
export function originLabel(origin: OriginView | null | undefined): string {
  if (origin === null || origin === undefined) {
    return EMPTY_CELL;
  }
  switch (origin) {
    case "scheduled":
      return "Scheduled";
    case "manual":
      return "Manual";
    case "discovered":
      return "Discovered";
    case "adopted":
      return "Adopted";
    case "replicated":
      return "Replicated";
    default:
      return unknownVariant(origin, "OriginView");
  }
}

/** One selectable filter value and the word beside it. */
export interface FilterOption {
  value: string;
  label: string;
}

/**
 * The `?phase=` values the handler accepts, in the CRD's own lifecycle order.
 *
 * `unknown` is last and is not a phase: it asks for every row whose phase this
 * *build of the server* does not recognise, which is how an operator finds
 * rows written by a newer controller.
 */
export const PHASE_FILTERS: readonly FilterOption[] = [
  { value: "pending", label: "Pending" },
  { value: "running", label: "Running" },
  { value: "succeeded", label: "Succeeded" },
  { value: "failed", label: "Failed" },
  { value: "deleting", label: "Deleting" },
  { value: "discovered", label: "Discovered" },
  { value: "unchanged", label: "Unchanged" },
  { value: "unknown", label: "Unrecognised by the operator" },
];

/** The `?origin=` values `kopiur_api::Origin::parse` accepts. */
export const ORIGIN_FILTERS: readonly FilterOption[] = [
  { value: "scheduled", label: "Scheduled" },
  { value: "manual", label: "Manual" },
  { value: "discovered", label: "Discovered" },
  { value: "adopted", label: "Adopted" },
  { value: "replicated", label: "Replicated" },
];

function isOneOf(options: readonly FilterOption[], value: unknown): boolean {
  return typeof value === "string" && options.some((option) => option.value === value);
}

/** Whether `?phase=` is a value the handler would accept rather than 400 on. */
export function isPhaseFilter(value: unknown): boolean {
  return isOneOf(PHASE_FILTERS, value);
}

/** Whether `?origin=` is a value the handler would accept rather than 400 on. */
export function isOriginFilter(value: unknown): boolean {
  return isOneOf(ORIGIN_FILTERS, value);
}

/** The three the CRD declares, for recognising a value rather than listing one. */
const DELETION_POLICIES = ["Delete", "Retain", "Orphan"] as const;

/**
 * `spec.deletionPolicy` as a word — and **never** a default.
 *
 * An absent value says so in words. A value this build does not know is
 * printed verbatim, because a policy it cannot interpret must not be described
 * as one of the three it can.
 */
export function deletionPolicyLabel(policy: string | null | undefined): string {
  if (policy === null || policy === undefined || policy.length === 0) {
    return "not set (the operator decides)";
  }
  return policy;
}

/** What deleting the `Snapshot` resource does to the kopia snapshot itself. */
export interface DeletionConsequence {
  /** Whether the CR says which of the three applies. */
  known: boolean;
  /** Whether confirming destroys the restore point. `false` when unknown. */
  destroys: boolean;
  /** The sentence the confirmation shows. */
  text: string;
}

/**
 * The irreversible half of a delete, stated before it is confirmed.
 *
 * Deleting the `Snapshot` resource is not the dangerous part — what the
 * finalizer then does to the kopia manifest is. Under `Delete` that manifest
 * goes, and the restore point with it; under `Retain` and `Orphan` it stays.
 *
 * With no policy on the CR this reports `known: false` and names **both**
 * outcomes. That is the whole point of addenda item 19: the effective policy
 * depends on how the snapshot was produced, the field does not mirror it, and
 * a console that guessed would tell exactly one of the two owners that their
 * data is safe when it is about to be destroyed.
 */
export function deletionConsequence(policy: string | null | undefined): DeletionConsequence {
  if (policy === "Delete") {
    return {
      known: true,
      destroys: true,
      text: "The kopia snapshot is deleted from the repository with the resource. This restore point is gone and cannot be restored from afterwards.",
    };
  }
  if (policy === "Retain") {
    return {
      known: true,
      destroys: false,
      text: "The kopia snapshot stays in the repository; only this Kubernetes resource goes away. The restore point survives, and a catalog scan would discover it again.",
    };
  }
  if (policy === "Orphan") {
    return {
      known: true,
      destroys: false,
      text: "The finalizer is dropped without touching kopia: the snapshot stays in the repository and kopiur stops tracking it. The restore point survives, unmanaged.",
    };
  }
  if (policy === null || policy === undefined || policy.length === 0) {
    return {
      known: false,
      destroys: false,
      text: "spec.deletionPolicy is not set on this snapshot, so the operator decides at delete time: a produced backup behaves as Delete and its kopia snapshot is destroyed, while a discovered one is forced to Retain and survives. This console cannot tell you which will apply — check how the snapshot was produced before confirming.",
    };
  }
  return {
    known: false,
    destroys: false,
    text: `${policy} is not one of the three deletion policies this console knows (${DELETION_POLICIES.join(", ")}), so it cannot say what will happen to the kopia snapshot. Read spec.deletionPolicy on the resource, and upgrade kopiur-ui to the operator's version, before confirming.`,
  };
}

/** A lamp and the one sentence beside it. */
export interface SnapshotVerdict {
  lamp: Lamp;
  text: string;
}

/**
 * One sentence answering "what is this snapshot?", for the detail's verdict
 * line.
 *
 * Never asserts more than the row carries: a row naming no policy says so
 * rather than leaving the clause out, and a phase this build cannot interpret
 * keeps the unknown lamp and the operator's own word.
 */
export function snapshotVerdict(row: SnapshotRow): SnapshotVerdict {
  const lamp = snapshotPhaseLamp(row.phase);
  const policy =
    row.policy !== null && row.policy !== undefined && row.policy.length > 0
      ? `under SnapshotPolicy ${row.policy}`
      : "under no SnapshotPolicy";
  const origin = row.origin !== null && row.origin !== undefined ? originLabel(row.origin) : "";
  const how = origin.length > 0 ? `${origin.toLowerCase()}, ` : "";
  const pin = row.pinned
    ? " It is pinned, so GFS retention will never prune it, whatever the policy's rules say."
    : "";
  return { lamp, text: `${lamp.word}: ${how}${policy}.${pin}` };
}
