import { EMPTY_CELL } from "../../util/format";
import { admitsText } from "../admits";
import type { InspectTarget } from "../inspect";
import { parseRef } from "../kind";
import type { CardRow } from "../objectCard";
import { replicationPhaseLabel } from "../replication";
import { repositoryPhaseLabel } from "../repository";
import { restorePhaseLabel } from "../restore";
import { deletionPolicyLabel, originLabel } from "../snapshot";
import type { Absence } from "../StatStrip";

/** One labelled value in the drawer's facts list. */
export interface DrawerFact {
  label: string;
  value: string | Absence;
  /** A value you would copy (a cron, an id, an identity) is set in mono. */
  mono?: boolean | undefined;
}

/** Another resource this one names, opened in the same drawer. */
export interface DrawerRelation {
  label: string;
  target: InspectTarget;
  /** Whether it feeds this resource (`before`) or is fed by it (`after`), for the chain. */
  side: "before" | "after";
}

export interface DrawerFacts {
  facts: DrawerFact[];
  related: DrawerRelation[];
}

const NA: Absence = { absent: "na" };

function text(value: string | null | undefined): string | Absence {
  return value === null || value === undefined || value.length === 0 || value === EMPTY_CELL
    ? NA
    : value;
}

function count(n: number | null | undefined): string | Absence {
  return n === null || n === undefined ? NA : n.toLocaleString("en-US");
}

function yesNo(value: boolean): string {
  return value ? "yes" : "no";
}

/**
 * The drawer's facts, built while walking a row: a wire reference the console
 * understands becomes a relation; one it does not (a `Backend/…`, a bare
 * string from a newer server) stays a fact, as the server wrote it — never a
 * link guessed into existence.
 */
class Builder {
  readonly facts: DrawerFact[] = [];
  readonly related: DrawerRelation[] = [];

  fact(label: string, value: string | Absence, mono = false): this {
    this.facts.push({ label, value, mono });
    return this;
  }

  /** A wire reference string (`Repository/media/nas`). */
  ref(
    label: string,
    value: string | null | undefined,
    side: DrawerRelation["side"] = "after",
  ): this {
    if (value === null || value === undefined || value.length === 0) return this;
    const parsed = parseRef(value);
    if (parsed === null) return this.fact(label, value, true);
    this.related.push({ label, target: parsed, side });
    return this;
  }

  /** A bare name of an object in the same namespace. */
  sibling(
    label: string,
    kind: InspectTarget["kind"],
    name: string | null | undefined,
    namespace: string,
    side: DrawerRelation["side"] = "after",
  ): this {
    if (name === null || name === undefined || name.length === 0) return this;
    this.related.push({ label, target: { kind, name, namespace }, side });
    return this;
  }

  done(): DrawerFacts {
    return { facts: this.facts, related: this.related };
  }
}

/**
 * What the drawer says about a resource beyond its card: the facts a card has
 * no room for, and the resources it names. Exhaustive over {@link CardRow} —
 * a new kind does not compile until it says what its drawer shows.
 */
export function drawerFacts(card: CardRow): DrawerFacts {
  const b = new Builder();
  switch (card.kind) {
    case "repository":
    case "clusterRepository": {
      const r = card.row;
      b.fact("Backend", text(r.backend))
        .fact("Mode", r.mode)
        .fact("Phase", text(repositoryPhaseLabel(r.phase)))
        .fact("Server", text(r.serverEndpoint), true);
      if (r.admits !== null && r.admits !== undefined) b.fact("Admits", admitsText(r.admits));
      return b.done();
    }
    case "snapshotPolicy": {
      const p = card.row;
      b.fact("Multi-repository", yesNo(p.multiRepo)).fact("Suspended", yesNo(p.suspended));
      for (const repo of p.repositories) b.ref("Writes into", repo);
      return b.done();
    }
    case "snapshotSchedule": {
      const s = card.row;
      b.fact("Cron", `${s.cron}${s.timezone ? ` · ${s.timezone}` : ""}`, true);
      if (s.policySelector !== null && s.policySelector !== undefined) {
        b.fact("Fires policies matching", s.policySelector, true);
      }
      b.fact("Suspended", yesNo(s.suspended));
      b.sibling("Fires", "snapshotPolicy", s.policy, s.namespace);
      b.sibling("Last snapshot", "snapshot", s.lastSnapshot, s.namespace);
      return b.done();
    }
    case "snapshot": {
      const s = card.row;
      b.fact("Origin", text(originLabel(s.origin)))
        .fact("Identity", text(s.identity), true)
        .fact("Kopia snapshot", text(s.kopiaSnapshotId), true)
        .fact("Pinned", yesNo(s.pinned))
        .fact("Deletion policy", deletionPolicyLabel(s.deletionPolicy));
      b.sibling("Policy", "snapshotPolicy", s.policy, s.namespace, "before");
      b.ref("Repository", s.repository, "before");
      b.ref("Copied from", s.copiedFrom, "before");
      return b.done();
    }
    case "restore": {
      const r = card.row;
      b.fact("Phase", text(restorePhaseLabel(r.phase)))
        .fact("Source", text(r.sourceKind))
        .fact("Target", r.targetKind)
        .fact(
          "Claims",
          r.claims.length > 0 ? r.claims.map((c) => `${c.pvc} (${c.phase})`).join(", ") : NA,
          true,
        )
        .fact("Kopia snapshot", text(r.kopiaSnapshotId), true);
      b.ref("Repository", r.repository, "before");
      return b.done();
    }
    case "maintenance": {
      const m = card.row;
      const run = m.manualRun;
      b.fact("Managed by", m.managedByRepository ? "its repository" : "authored by hand").fact(
        "Manual run",
        run === null || run === undefined
          ? NA
          : [run.mode, run.phase].filter((p): p is string => typeof p === "string").join(" · ") ||
              "requested",
      );
      b.ref("Repository", m.repository);
      // A projected Maintenance is usually owned by the repository it governs;
      // naming the same object twice says nothing the first did not.
      if (m.owner !== m.repository) b.ref("Owner", m.owner, "before");
      return b.done();
    }
    case "repositoryReplication": {
      const r = card.row;
      b.fact("Destination", text(r.destinationBackend))
        .fact("Cron", r.cron, true)
        .fact("Phase", text(replicationPhaseLabel(r.phase)))
        .fact("Suspended", yesNo(r.suspended));
      b.ref("Source", r.source, "before");
      return b.done();
    }
    case "snapshotReplication": {
      const r = card.row;
      b.fact("Cron", r.cron, true)
        .fact("Phase", text(replicationPhaseLabel(r.phase)))
        .fact("Suspended", yesNo(r.suspended))
        .fact("Identities selected", count(r.identitiesSelected))
        .fact("Pruned", count(r.pruned));
      b.ref("Source", r.source, "before");
      b.ref("Destination", r.destination);
      return b.done();
    }
  }
}
