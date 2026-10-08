import { ScrollText, Wrench } from "lucide-react";

import type {
  PolicyRow,
  ReplicationRef,
  ReplicationsView,
  RepositoryDetail,
} from "../../../api/types";
import { humanBytes, relativeTime } from "../../../util/format";
import { ConditionsTable } from "../../ConditionsTable";
import { cardFacts } from "../../objectCard";
import { repositoryVerdict } from "../../repository";
import { RepositoryActions } from "../../RepositoryActions";
import { Chain, type ChainItem } from "../Chain";
import { DrawerHead } from "../DrawerHead";
import { DrawerSection, GateFindings } from "../DrawerSection";
import type { DrawerView } from "../views";
import { CatalogTab, MaintenanceCoverage, SessionsTab, StorageTab } from "./RepositoryTabs";

/** What the repository drawer reads besides the detail: rows that give its chain live pills. */
export interface RepositoryExtras {
  policies?: readonly PolicyRow[] | undefined;
  replications?: ReplicationsView | undefined;
}

/**
 * A replication the server named, with its own pill when its row is loaded —
 * matched on kind, namespace and name, never a same-named one elsewhere.
 */
function replicationItem(
  ref: ReplicationRef,
  rows: ReplicationsView | undefined,
  now: Date,
): ChainItem {
  const same = (r: { namespace: string; name: string }) =>
    r.namespace === ref.namespace && r.name === ref.name;
  const target = { kind: ref.kind, name: ref.name, namespace: ref.namespace };
  switch (ref.kind) {
    case "snapshotReplication": {
      const row = rows?.snapshot.find(same);
      return { ref: target, health: row && cardFacts({ kind: ref.kind, row }, now).lamp };
    }
    case "repositoryReplication": {
      const row = rows?.repository.find(same);
      return { ref: target, health: row && cardFacts({ kind: ref.kind, row }, now).lamp };
    }
  }
}

/**
 * One repository: what it is doing (the verdict), what feeds it and what it
 * feeds (the chain), its headline numbers and anything holding it — then,
 * a tab away each, what is in it, whether it can be reached, how it is
 * maintained, who is browsing it, and the raw conditions.
 */
export function repositoryView(
  kind: "repository" | "clusterRepository",
  detail: RepositoryDetail,
  extras: RepositoryExtras,
  now: Date,
): DrawerView {
  const { summary } = detail;
  const verdict = repositoryVerdict(summary);
  const namespace = summary.namespace ?? undefined;
  const written: ChainItem[] = detail.policies.map((p) => {
    const row = extras.policies?.find((r) => r.namespace === p.namespace && r.name === p.name);
    return {
      ref: { kind: "snapshotPolicy", name: p.name, namespace: p.namespace },
      health: row && cardFacts({ kind: "snapshotPolicy", row }, now).lamp,
    };
  });
  return {
    lamp: verdict.lamp,
    head: (
      <DrawerHead
        verdict={{ label: "Repository verdict", lamp: verdict.lamp, text: verdict.text }}
        chain={
          <Chain
            kind={kind}
            namespace={namespace}
            before={[
              {
                label: "Fired by",
                items: detail.schedules.map((row) => ({
                  ref: { kind: "snapshotSchedule", name: row.name, namespace: row.namespace },
                  health: cardFacts({ kind: "snapshotSchedule", row }, now).lamp,
                })),
              },
              { label: "Written by", items: written },
              {
                label: "Copied in by",
                items: detail.replicationsIn.map((r) =>
                  replicationItem(r, extras.replications, now),
                ),
              },
            ]}
            after={[
              {
                label: "Copies to",
                items: detail.replicationsOut.map((r) =>
                  replicationItem(r, extras.replications, now),
                ),
              },
            ]}
          />
        }
        stats={[
          {
            label: "Snapshots",
            value:
              summary.snapshotCount !== null && summary.snapshotCount !== undefined
                ? summary.snapshotCount.toLocaleString()
                : { absent: "na" },
          },
          { label: "Stored", value: humanBytes(summary.totalSizeBytes) },
          {
            label: "Index blobs",
            value:
              summary.indexBlobCount !== null && summary.indexBlobCount !== undefined
                ? summary.indexBlobCount.toLocaleString()
                : { absent: "na" },
          },
          {
            label: "Last observed",
            value:
              summary.lastObservedAt !== null && summary.lastObservedAt !== undefined
                ? relativeTime(summary.lastObservedAt, now)
                : { absent: "unreported", field: "repositoryLastObserved" },
          },
        ]}
        statsLabel={`Repository ${summary.name} at a glance`}
        findings={
          <GateFindings
            gates={detail.gates}
            title="Gates holding this repository"
            namespace={namespace}
          />
        }
      />
    ),
    tabsLabel: "About this repository",
    tabs: [
      { id: "storage", label: "Storage", render: () => <StorageTab detail={detail} now={now} /> },
      { id: "catalog", label: "Catalog", render: () => <CatalogTab detail={detail} now={now} /> },
      {
        id: "maintenance",
        label: "Maintenance",
        render: () => (
          <DrawerSection title="Maintenance" icon={Wrench}>
            <MaintenanceCoverage maintenance={detail.maintenance} now={now} />
          </DrawerSection>
        ),
      },
      {
        id: "sessions",
        label: "Sessions",
        count: detail.sessions.length,
        render: () => <SessionsTab detail={detail} now={now} />,
      },
      {
        id: "conditions",
        label: "Conditions",
        render: () => (
          <DrawerSection title="Conditions" icon={ScrollText}>
            <ConditionsTable
              conditions={detail.conditions}
              now={now}
              emptyNote="No conditions yet: the operator has not reconciled this repository."
            />
          </DrawerSection>
        ),
      },
    ],
    actions: (
      <div className="drawer-actions">
        <RepositoryActions detail={detail} />
      </div>
    ),
  };
}
