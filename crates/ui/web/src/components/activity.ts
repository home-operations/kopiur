import type {
  MaintenanceRow,
  ReplicationsView,
  RestoreRow,
  RunStatusView,
  SnapshotRow,
} from "../api/types";
import { EMPTY_CELL, humanBytes, humanDuration } from "../util/format";
import { type Lamp, healthLamp } from "./health";
import type { InspectTarget } from "./inspect";
import { parseRef } from "./kind";
import { replicationHealth, replicationPhaseLabel } from "./replication";
import { restorePhaseLamp } from "./restore";
import { snapshotPhaseLamp } from "./snapshot";

/**
 * The overview's "Recent activity": every run the fleet has made, of every
 * kind, as one timeline — newest first, nothing held back.
 *
 * A row is one run: a snapshot or a restore (each is its own run), a
 * replication's last pass, a maintenance object's last quick and last full
 * run. Something that has never run has no place on a timeline and is left
 * out rather than dated "never".
 */
export interface ActivityItem {
  target: InspectTarget;
  /** How the run ended, in its own word ("Succeeded", "Failed", "Running"). */
  lamp: Lamp;
  /** What ran: "photos → nas · 2.0 KiB · took 30s". */
  what: string;
  /** RFC3339 instant it finished, or started when it has not finished. */
  at: string;
}

export interface ActivitySources {
  snapshots?: readonly SnapshotRow[] | undefined;
  restores?: readonly RestoreRow[] | undefined;
  replications?: ReplicationsView | undefined;
  maintenance?: readonly MaintenanceRow[] | undefined;
}

/** A wire reference (`Repository/media/nas`) → its name; anything else as written. */
function refName(ref: string | null | undefined): string | null {
  if (ref === null || ref === undefined || ref.length === 0) return null;
  return parseRef(ref)?.name ?? ref;
}

function took(start: string | null | undefined, end: string | null | undefined): string | null {
  if (!start || !end) return null;
  const seconds = (Date.parse(end) - Date.parse(start)) / 1000;
  return Number.isNaN(seconds) ? null : `took ${humanDuration(seconds)}`;
}

function bytes(n: number | null | undefined): string | null {
  const text = humanBytes(n);
  return text === EMPTY_CELL ? null : text;
}

/** Parts joined with a middle dot, the absent ones dropped. */
function line(...parts: (string | null)[]): string {
  return parts.filter((p): p is string => p !== null && p.length > 0).join(" · ");
}

function arrow(from: string | null, to: string | null): string | null {
  if (from === null && to === null) return null;
  return `${from ?? "?"} → ${to ?? "?"}`;
}

const SUCCEEDED: Lamp = { ...healthLamp("healthy"), word: "Succeeded" };
const FAILED: Lamp = { ...healthLamp("failed"), word: "Failed" };

function maintenanceRun(
  m: MaintenanceRow,
  run: RunStatusView,
  tier: "Quick" | "Full",
): ActivityItem | null {
  if (!run.lastRunAt) return null;
  return {
    target: { kind: "maintenance", namespace: m.namespace, name: m.name },
    lamp: run.consecutiveFailures > 0 ? FAILED : SUCCEEDED,
    what: `${tier} maintenance of ${refName(m.repository) ?? m.name}`,
    at: run.lastRunAt,
  };
}

export function activity(sources: ActivitySources): ActivityItem[] {
  const items: ActivityItem[] = [];
  for (const s of sources.snapshots ?? []) {
    const at = s.endTime ?? s.startTime;
    if (!at) continue;
    items.push({
      target: { kind: "snapshot", namespace: s.namespace, name: s.name },
      lamp: snapshotPhaseLamp(s.phase),
      what: line(
        arrow(s.policy ?? null, refName(s.repository)),
        bytes(s.sizeBytes),
        took(s.startTime, s.endTime),
      ),
      at,
    });
  }
  for (const r of sources.restores ?? []) {
    const at = r.endTime ?? r.startTime;
    if (!at) continue;
    items.push({
      target: { kind: "restore", namespace: r.namespace, name: r.name },
      lamp: restorePhaseLamp(r.phase),
      what: line(
        arrow(refName(r.repository), r.targetKind),
        bytes(r.bytesRestored),
        took(r.startTime, r.endTime),
      ),
      at,
    });
  }
  for (const r of sources.replications?.repository ?? []) {
    if (!r.lastReplicated) continue;
    items.push({
      target: { kind: "repositoryReplication", namespace: r.namespace, name: r.name },
      lamp: {
        ...healthLamp(replicationHealth(r.phase, r.suspended)),
        word: replicationPhaseLabel(r.phase),
      },
      what: line(
        arrow(refName(r.source), r.destinationBackend ?? null),
        bytes(r.lastReplicatedBytes),
      ),
      at: r.lastReplicated,
    });
  }
  for (const r of sources.replications?.snapshot ?? []) {
    if (!r.lastReplicated) continue;
    const copied =
      r.snapshotsCopied !== null && r.snapshotsCopied !== undefined
        ? `${String(r.snapshotsCopied)} copied`
        : null;
    items.push({
      target: { kind: "snapshotReplication", namespace: r.namespace, name: r.name },
      lamp: {
        ...healthLamp(replicationHealth(r.phase, r.suspended)),
        word: replicationPhaseLabel(r.phase),
      },
      what: line(arrow(refName(r.source), refName(r.destination)), copied),
      at: r.lastReplicated,
    });
  }
  for (const m of sources.maintenance ?? []) {
    for (const item of [maintenanceRun(m, m.quick, "Quick"), maintenanceRun(m, m.full, "Full")]) {
      if (item !== null) items.push(item);
    }
  }
  return items.sort((a, b) => Date.parse(b.at) - Date.parse(a.at));
}
