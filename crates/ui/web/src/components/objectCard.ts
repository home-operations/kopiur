import type {
  MaintenanceRow,
  ObjectKind,
  PolicyRow,
  RepositoryReplicationRow,
  RepositorySummary,
  RestoreRow,
  ScheduleRow,
  SnapshotReplicationRow,
  SnapshotRow,
} from "../api/types";
import { humanAge, humanBytes, humanDuration, relativeTime } from "../util/format";
import { admitsText } from "./admits";
import { type Lamp, healthLamp, loudLamp } from "./health";
import { parseRef } from "./kind";
import { replicationHealth, replicationPhaseLabel } from "./replication";
import { restorePhaseLamp } from "./restore";
import { snapshotPhaseLamp } from "./snapshot";
import type { Stat } from "./StatStrip";

/** One object, as the row the server sent for its kind. */
export type CardRow =
  | { kind: "repository" | "clusterRepository"; row: RepositorySummary }
  | { kind: "snapshotPolicy"; row: PolicyRow }
  | { kind: "snapshotSchedule"; row: ScheduleRow }
  | { kind: "snapshot"; row: SnapshotRow }
  | { kind: "restore"; row: RestoreRow }
  | { kind: "maintenance"; row: MaintenanceRow }
  | { kind: "repositoryReplication"; row: RepositoryReplicationRow }
  | { kind: "snapshotReplication"; row: SnapshotReplicationRow };

/** What a card shows: identity, its pill, one meta line and three facts. */
export interface CardFacts {
  kind: ObjectKind;
  name: string;
  namespace?: string | undefined;
  lamp: Lamp;
  meta: string;
  stats: readonly [Stat, Stat, Stat];
  /** The detail route; absent for a kind that has none. */
  to?: string | undefined;
}

function seconds(start: string | null | undefined, end: string | null | undefined): number | null {
  if (!start || !end) return null;
  const ms = Date.parse(end) - Date.parse(start);
  return Number.isNaN(ms) ? null : ms / 1000;
}

function count(n: number | null | undefined): string {
  return n === null || n === undefined ? "—" : n.toLocaleString("en-US");
}

/** A run of consecutive failures as a loud pill, or `ok` when there are none. */
function failures(n: number, ok: Lamp): Lamp {
  return n > 0 ? loudLamp(`${String(n)} failed run${n === 1 ? "" : "s"}`) : ok;
}

/** Wire reference → its name, for a meta line. */
function refName(ref: string): string {
  return parseRef(ref)?.name ?? ref;
}

/**
 * The facts every object card shows, per kind — the `kinds.md` table of the
 * `kopiur-ui-design` skill. Exhaustive over {@link CardRow}: a new kind does
 * not compile until it says what its card carries.
 */
export function cardFacts(card: CardRow, now: Date): CardFacts {
  switch (card.kind) {
    case "repository":
    case "clusterRepository": {
      const r = card.row;
      const admits =
        r.admits !== null && r.admits !== undefined ? ` · ${admitsText(r.admits)}` : "";
      return {
        kind: card.kind,
        name: r.name,
        namespace: r.namespace ?? undefined,
        lamp: healthLamp(r.health),
        meta: `${r.backend ?? "backend not reported"} · ${r.mode}${admits}`,
        stats: [
          { label: "Snapshots", value: count(r.snapshotCount) },
          { label: "Stored", value: humanBytes(r.totalSizeBytes) },
          {
            label: "Last observed",
            value: humanAge(r.lastObservedAt, now) + (r.lastObservedAt ? " ago" : ""),
          },
        ],
        to: `/repositories/${r.kindPath}/${r.name}${
          r.namespace ? `?namespace=${encodeURIComponent(r.namespace)}` : ""
        }`,
      };
    }
    case "snapshotPolicy": {
      const p = card.row;
      return {
        kind: card.kind,
        name: p.name,
        namespace: p.namespace,
        lamp: p.suspended ? healthLamp("suspended") : { ...healthLamp("healthy"), word: "Active" },
        meta:
          p.repositories.length > 0
            ? `writes into ${p.repositories.map(refName).join(", ")}`
            : "names no repository",
        stats: [
          { label: "Live snapshots", value: count(p.activeSnapshotCount) },
          {
            label: "Last success",
            value: p.lastSuccessfulSnapshot
              ? relativeTime(p.lastSuccessfulSnapshot, now)
              : { absent: "loud", text: "never succeeded" },
          },
          {
            label: "Last verified",
            value: p.lastVerified
              ? relativeTime(p.lastVerified, now)
              : { absent: "loud", text: "never verified" },
          },
        ],
        to: `/policies/${p.namespace}/${p.name}`,
      };
    }
    case "snapshotSchedule": {
      const s = card.row;
      return {
        kind: card.kind,
        name: s.name,
        namespace: s.namespace,
        lamp: s.suspended
          ? healthLamp("suspended")
          : failures(s.consecutiveFailures, { ...healthLamp("healthy"), word: "Active" }),
        meta: `${s.cron}${s.timezone ? ` · ${s.timezone}` : ""}`,
        stats: [
          {
            label: "Next fire",
            value: s.nextFire ? relativeTime(s.nextFire, now) : { absent: "na" },
          },
          {
            label: "Last fire",
            value: s.lastFire
              ? relativeTime(s.lastFire, now)
              : { absent: "loud", text: "never fired" },
          },
          { label: "Failures", value: count(s.consecutiveFailures) },
        ],
      };
    }
    case "snapshot": {
      const s = card.row;
      const took = seconds(s.startTime, s.endTime);
      const parts = [s.origin ? s.origin.charAt(0).toUpperCase() + s.origin.slice(1) : null];
      if (s.pinned) parts.push("pinned");
      if (s.deletionPolicy) parts.push(s.deletionPolicy);
      return {
        kind: card.kind,
        name: s.name,
        namespace: s.namespace,
        lamp: snapshotPhaseLamp(s.phase),
        meta: parts.filter((p): p is string => p !== null).join(" · "),
        stats: [
          { label: "Size", value: humanBytes(s.sizeBytes) },
          { label: "Files", value: count(s.filesTotal) },
          { label: "Took", value: took === null ? { absent: "na" } : humanDuration(took) },
        ],
        to: `/snapshots/${s.namespace}/${s.name}`,
      };
    }
    case "restore": {
      const r = card.row;
      const took = seconds(r.startTime, r.endTime);
      const claim = r.claims[0]?.pvc;
      return {
        kind: card.kind,
        name: r.name,
        namespace: r.namespace,
        lamp: restorePhaseLamp(r.phase),
        meta:
          claim === undefined
            ? `into ${r.targetKind}`
            : r.targetKind === "Pvc"
              ? `into new claim ${claim}`
              : `into ${claim}`,
        stats: [
          { label: "Restored", value: humanBytes(r.bytesRestored) },
          { label: "Files", value: count(r.filesRestored) },
          { label: "Took", value: took === null ? { absent: "na" } : humanDuration(took) },
        ],
        to: `/restores/${r.namespace}/${r.name}`,
      };
    }
    case "maintenance": {
      const m = card.row;
      const failed = m.quick.consecutiveFailures + m.full.consecutiveFailures;
      const reclaimed = m.full.lastContentReclaimedBytes ?? m.quick.lastContentReclaimedBytes;
      return {
        kind: card.kind,
        name: m.name,
        namespace: m.namespace,
        lamp: failures(failed, { ...healthLamp("healthy"), word: "OK" }),
        meta: m.managedByRepository
          ? `managed by ${refName(m.repository)}`
          : `for ${refName(m.repository)}`,
        stats: [
          {
            label: "Quick",
            value: m.quick.lastRunAt
              ? relativeTime(m.quick.lastRunAt, now)
              : { absent: "loud", text: "never run" },
          },
          {
            label: "Full",
            value: m.full.lastRunAt
              ? relativeTime(m.full.lastRunAt, now)
              : { absent: "loud", text: "never run" },
          },
          { label: "Reclaimed", value: humanBytes(reclaimed) },
        ],
      };
    }
    case "repositoryReplication": {
      const r = card.row;
      return {
        kind: card.kind,
        name: r.name,
        namespace: r.namespace,
        lamp: {
          ...healthLamp(replicationHealth(r.phase, r.suspended)),
          word: replicationPhaseLabel(r.phase),
        },
        meta: `every blob → ${r.destinationBackend ?? "backend"} · ${r.cron}`,
        stats: [
          {
            label: "Last replicated",
            value: r.lastReplicated
              ? relativeTime(r.lastReplicated, now)
              : { absent: "loud", text: "never replicated" },
          },
          { label: "Copied", value: humanBytes(r.lastReplicatedBytes) },
          { label: "Blobs", value: count(r.lastReplicatedBlobs) },
        ],
      };
    }
    case "snapshotReplication": {
      const r = card.row;
      return {
        kind: card.kind,
        name: r.name,
        namespace: r.namespace,
        lamp: {
          ...healthLamp(replicationHealth(r.phase, r.suspended)),
          word: replicationPhaseLabel(r.phase),
        },
        meta: `${refName(r.source)} → ${refName(r.destination)} · ${r.cron}`,
        stats: [
          { label: "Copied", value: count(r.snapshotsCopied) },
          { label: "Already there", value: count(r.alreadyPresent) },
          { label: "Failed", value: count(r.failed) },
        ],
      };
    }
  }
}
