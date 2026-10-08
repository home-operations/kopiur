import { Link } from "@tanstack/react-router";

import type { Health, ObjectKind, OverviewView } from "../api/types";
import { KindChip } from "./KindMark";
import { KIND_META } from "./kind";
import { HEALTH_ORDER, type HealthKey, healthLamp } from "./health";

type TilePath =
  | "/repositories"
  | "/snapshots"
  | "/policies"
  | "/schedules"
  | "/restores"
  | "/maintenance"
  | "/replications";

interface TileSpec {
  /** The kind whose colour and glyph the tile wears. */
  kind: ObjectKind;
  /** The server kinds counted on it — two repository kinds, two replication kinds. */
  counts: readonly ObjectKind[];
  label: (windowHours: number) => string;
  to: TilePath;
}

const TILES: readonly TileSpec[] = [
  {
    kind: "repository",
    counts: ["repository", "clusterRepository"],
    label: () => "Repositories",
    to: "/repositories",
  },
  { kind: "snapshotPolicy", counts: ["snapshotPolicy"], label: () => "Policies", to: "/policies" },
  {
    kind: "snapshotSchedule",
    counts: ["snapshotSchedule"],
    label: () => "Schedules",
    to: "/schedules",
  },
  {
    kind: "snapshot",
    counts: ["snapshot"],
    label: (h) => `Snapshots · ${String(h)}h`,
    to: "/snapshots",
  },
  { kind: "restore", counts: ["restore"], label: () => "Restores", to: "/restores" },
  {
    kind: "snapshotReplication",
    counts: ["repositoryReplication", "snapshotReplication"],
    label: () => "Replications",
    to: "/replications",
  },
  { kind: "maintenance", counts: ["maintenance"], label: () => "Maintenance", to: "/maintenance" },
];

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

/**
 * The fleet by kind: one tile per kind in scope, each a count, a status bar
 * and the breakdown in words. The bar is decoration beside the words — the
 * words are the tile's account of its state — and a tile with anything
 * failed is marked so it is found first.
 */
export function KindTiles({
  overview,
  namespace,
}: {
  overview: OverviewView;
  namespace: string | undefined;
}) {
  const scope = namespace !== undefined ? { namespace } : {};
  return (
    <ul className="kind-tiles" aria-label="Fleet by kind">
      {TILES.map((tile) => {
        const tallies = overview.kinds.filter((k) => tile.counts.includes(k.kind));
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
        const failing = (byKey.get("failed") ?? 0) > 0;
        const label = tile.label(overview.snapshotWindowHours);
        const search =
          tile.to === "/repositories" && failing ? { health: "failed", ...scope } : scope;
        return (
          <li key={tile.to}>
            <Link
              className="kind-tile has-stripe"
              data-kind={KIND_META[tile.kind].slug}
              data-failing={failing ? "true" : undefined}
              to={tile.to}
              search={search}
            >
              <span className="kind-tile__head">
                <KindChip kind={tile.kind} />
                <span className="kind-tile__label">{label}</span>
                <span className="kind-tile__count">{total}</span>
              </span>
              <span className="status-bar" aria-hidden="true">
                {parts.map((p) => (
                  <span key={p.key} data-health={p.key} style={{ flexGrow: p.n }} />
                ))}
              </span>
              <span className="kind-tile__breakdown">
                {parts.length === 0 ? (
                  <span className="kind-tile__none">none in scope</span>
                ) : (
                  parts.map((p) => {
                    const lamp = healthLamp(p.key);
                    const Icon = lamp.icon;
                    return (
                      <span key={p.key} className="kind-tile__part" data-health={p.key}>
                        <Icon size={12} strokeWidth={2} aria-hidden="true" />
                        {`${String(p.n)} ${WORD[p.key]}`}
                      </span>
                    );
                  })
                )}
              </span>
            </Link>
          </li>
        );
      })}
    </ul>
  );
}
