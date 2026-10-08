import { ScrollText } from "lucide-react";

import type { PolicyDetail, RepositorySummary } from "../../../api/types";
import { relativeTime } from "../../../util/format";
import { ConditionsTable } from "../../ConditionsTable";
import { gateSeverityLamp } from "../../gates";
import { type Lamp, healthLamp } from "../../health";
import { parseRef } from "../../kind";
import { cardFacts } from "../../objectCard";
import { policyVerdict, snapshotCount } from "../../policy";
import { Chain, type ChainItem } from "../Chain";
import { DrawerHead } from "../DrawerHead";
import { DrawerSection, GateFindings } from "../DrawerSection";
import type { DrawerView } from "../views";
import { PolicyActions } from "./PolicyActions";
import { BacksUpTab, RetentionTab, SnapshotsTab, VerificationTab } from "./PolicyTabs";

/**
 * The verdict's pill: suspended, then the worst gate holding it, else active.
 * "Never succeeded" and "never verified" are loud facts in the stats, not a
 * pill — the same split the overview's policy tile makes.
 */
function policyLamp(detail: PolicyDetail): Lamp {
  if (detail.row.suspended) return healthLamp("suspended");
  const gates = detail.gates.map((gate) => gateSeverityLamp(gate.severity));
  return (
    gates.find((lamp) => lamp.key === "failed") ??
    gates[0] ?? { ...healthLamp("healthy"), word: "Active" }
  );
}

/**
 * The repositories it writes into, each with its own pill when its row is
 * loaded. A key the console cannot read stays its text, never dropped.
 */
function writesInto(
  keys: readonly string[],
  rows: readonly RepositorySummary[] | undefined,
  now: Date,
): ChainItem[] {
  return keys.map((key): ChainItem => {
    const ref = parseRef(key);
    if (ref === null) return { text: key };
    const row = rows?.find(
      (r) =>
        r.name === ref.name &&
        (r.kind === "ClusterRepository") === (ref.kind === "clusterRepository") &&
        (r.namespace ?? undefined) === ref.namespace,
    );
    const kind = ref.kind === "clusterRepository" ? "clusterRepository" : "repository";
    return { ref, health: row && cardFacts({ kind, row }, now).lamp };
  });
}

/**
 * One policy: what it writes and whether it has ever worked (the verdict),
 * the schedules that fire it and the repositories it writes into (the chain),
 * then the recipe, its retention, its verification and its runs a tab away.
 */
export function policyView(
  detail: PolicyDetail,
  repositories: readonly RepositorySummary[] | undefined,
  now: Date,
): DrawerView {
  const { row } = detail;
  const lamp = policyLamp(detail);
  return {
    lamp,
    head: (
      <DrawerHead
        verdict={{ label: "Policy verdict", lamp, text: policyVerdict(row).text }}
        chain={
          <Chain
            kind="snapshotPolicy"
            namespace={row.namespace}
            before={[
              {
                label: "Fired by",
                items: detail.schedules.map((schedule) => ({
                  ref: {
                    kind: "snapshotSchedule",
                    name: schedule.name,
                    namespace: schedule.namespace,
                  },
                  health: cardFacts({ kind: "snapshotSchedule", row: schedule }, now).lamp,
                })),
              },
            ]}
            after={[
              { label: "Writes into", items: writesInto(row.repositories, repositories, now) },
            ]}
          />
        }
        stats={[
          { label: "Live snapshots", value: snapshotCount(row.activeSnapshotCount) },
          {
            label: "Last success",
            value:
              row.lastSuccessfulSnapshot !== null && row.lastSuccessfulSnapshot !== undefined
                ? relativeTime(row.lastSuccessfulSnapshot, now)
                : { absent: "loud", text: "never succeeded" },
            abs: row.lastSuccessfulSnapshot ?? undefined,
          },
          {
            label: "Last verified",
            value:
              row.lastVerified !== null && row.lastVerified !== undefined
                ? relativeTime(row.lastVerified, now)
                : { absent: "loud", text: "never verified" },
            abs: row.lastVerified ?? undefined,
          },
          { label: "Schedules", value: detail.schedules.length.toLocaleString() },
        ]}
        statsLabel={`SnapshotPolicy ${row.name} at a glance`}
        findings={
          <GateFindings
            gates={detail.gates}
            title="Gates holding this policy"
            namespace={row.namespace}
          />
        }
      />
    ),
    tabsLabel: "About this policy",
    tabs: [
      { id: "backs-up", label: "Backs up", render: () => <BacksUpTab detail={detail} now={now} /> },
      { id: "retention", label: "Retention", render: () => <RetentionTab detail={detail} /> },
      {
        id: "verification",
        label: "Verification",
        render: () => <VerificationTab detail={detail} now={now} />,
      },
      {
        id: "snapshots",
        label: "Snapshots",
        count: detail.recentSnapshots.length,
        render: () => <SnapshotsTab detail={detail} now={now} />,
      },
      {
        id: "conditions",
        label: "Conditions",
        render: () => (
          <DrawerSection title="Conditions" icon={ScrollText}>
            <ConditionsTable
              conditions={detail.conditions}
              now={now}
              emptyNote="No conditions yet: the operator has not reconciled this policy."
            />
          </DrawerSection>
        ),
      },
    ],
    actions: <PolicyActions row={row} />,
  };
}
