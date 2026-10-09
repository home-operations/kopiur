import type { Health, ObjectKind, OverviewView } from "../api/types";
import { HEALTH_ORDER, type HealthKey, healthLamp } from "./health";
import type { NavPath } from "./nav";

/**
 * What a sidebar section says about its kind: how many there are in scope,
 * by health, worst first — the count and bar beside its name, and the same
 * in words for the link's description.
 */
export interface NavTally {
  total: number;
  /** Non-empty health buckets, worst first. */
  parts: { key: HealthKey; n: number }[];
  /** How many are failed; the section shows a failed pill when non-zero. */
  failing: number;
  /** "4 in the last 24h: 1 failed, 2 ok" — the bar as a sentence. */
  words: string;
}

/**
 * The server kinds each section counts. Repositories counts both repository
 * kinds and Replications both replication kinds; Snapshots counts the
 * overview's window, not every snapshot ever taken.
 */
const COUNTS: Partial<Record<NavPath, readonly ObjectKind[]>> = {
  "/repositories": ["repository", "clusterRepository"],
  "/maintenance": ["maintenance"],
  "/replications": ["repositoryReplication", "snapshotReplication"],
  "/policies": ["snapshotPolicy"],
  "/schedules": ["snapshotSchedule"],
  "/snapshots": ["snapshot"],
  "/restores": ["restore"],
};

/** The word a count reads with: "1 failed", "2 ok". */
const WORD: Record<HealthKey, string> = {
  failed: "failed",
  degraded: "degraded",
  pending: "pending",
  unknown: "unknown",
  suspended: "suspended",
  healthy: "ok",
};

/** A `Health` from the wire as a lamp key; anything unrecognised is unknown. */
function keyOf(health: Health): HealthKey {
  return healthLamp(health).key;
}

/** The tally for one section, or `null` when it lists no kind or nothing was read. */
export function navTally(overview: OverviewView | undefined, to: NavPath): NavTally | null {
  const counts = COUNTS[to];
  if (counts === undefined || overview === undefined || !Array.isArray(overview.kinds)) {
    return null;
  }
  const tallies = overview.kinds.filter((k) => counts.includes(k.kind));
  const total = tallies.reduce((sum, t) => sum + t.total, 0);
  const byKey = new Map<HealthKey, number>();
  for (const t of tallies) {
    for (const h of t.byHealth) {
      const key = keyOf(h.health);
      byKey.set(key, (byKey.get(key) ?? 0) + h.count);
    }
  }
  const parts = HEALTH_ORDER.flatMap((key) => {
    const n = byKey.get(key) ?? 0;
    return n > 0 ? [{ key, n }] : [];
  });
  const window = to === "/snapshots" ? ` in the last ${String(overview.snapshotWindowHours)}h` : "";
  const words =
    parts.length === 0
      ? "none in scope"
      : `${String(total)}${window}: ${parts.map((p) => `${String(p.n)} ${WORD[p.key]}`).join(", ")}`;
  return { total, parts, failing: byKey.get("failed") ?? 0, words };
}
