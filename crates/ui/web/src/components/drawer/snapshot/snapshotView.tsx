import { ScrollText } from "lucide-react";

import type { SnapshotDetail } from "../../../api/types";
import { humanBytes, humanDuration, relativeTime } from "../../../util/format";
import { ConditionsTable } from "../../ConditionsTable";
import { Finding } from "../../Finding";
import { healthLamp } from "../../health";
import { parseRef } from "../../kind";
import { durationSeconds, snapshotVerdict } from "../../snapshot";
import { SnapshotActions } from "../../SnapshotActions";
import { Chain, type ChainItem } from "../Chain";
import { DrawerHead } from "../DrawerHead";
import { DrawerSection, GateFindings } from "../DrawerSection";
import type { DrawerView } from "../views";
import { BrowseFiles } from "./BrowseFiles";
import { LineageTab, LogTab, RetentionTab, RunTab, StorageTab } from "./SnapshotTabs";

/** A repository key as a chain item: a reference when it reads as one, its text otherwise. */
function repositoryItem(key: string | null | undefined): ChainItem[] {
  if (key === null || key === undefined || key.length === 0) return [];
  const ref = parseRef(key);
  return [ref === null ? { text: key } : { ref }];
}

/**
 * One snapshot: its phase in a sentence, the repository and policy it was
 * made through, its size and timing, and anything holding or failing it —
 * then what the run did, where it lives, whether retention keeps it, where it
 * was copied, the log and the conditions a tab away each. Its actions, and
 * the way into its files, are in the foot.
 */
export function snapshotView(detail: SnapshotDetail, now: Date): DrawerView {
  const { row } = detail;
  const verdict = snapshotVerdict(row);
  const failure = detail.failure;
  return {
    lamp: verdict.lamp,
    head: (
      <DrawerHead
        verdict={{ label: "Snapshot verdict", lamp: verdict.lamp, text: verdict.text }}
        chain={
          <Chain
            kind="snapshot"
            namespace={row.namespace}
            before={[
              { label: "Copied from", items: repositoryItem(row.copiedFrom) },
              { label: "In", items: repositoryItem(row.repository) },
              {
                label: "Taken under",
                items:
                  row.policy !== null && row.policy !== undefined && row.policy.length > 0
                    ? [
                        {
                          ref: {
                            kind: "snapshotPolicy",
                            name: row.policy,
                            namespace: row.namespace,
                          },
                        },
                      ]
                    : [],
              },
            ]}
            after={[]}
          />
        }
        stats={[
          { label: "Size", value: humanBytes(row.sizeBytes) },
          {
            label: "Files",
            value:
              row.filesTotal !== null && row.filesTotal !== undefined
                ? row.filesTotal.toLocaleString()
                : { absent: "na" },
          },
          { label: "Took", value: humanDuration(durationSeconds(row)) },
          {
            label: "Started",
            value:
              row.startTime !== null && row.startTime !== undefined
                ? relativeTime(row.startTime, now)
                : { absent: "na" },
            abs: row.startTime ?? undefined,
          },
        ]}
        statsLabel={`Snapshot ${row.name} at a glance`}
        findings={
          <>
            {failure !== null && failure !== undefined ? (
              <Finding
                title={failure.kopiaErrorClass ?? "Failure"}
                what={failure.message ?? "The operator recorded a failure with no message."}
                why={
                  failure.retryRecommended === true
                    ? "The operator marked this as worth retrying; the cause looks transient."
                    : failure.retryRecommended === false
                      ? "The operator marked this as not worth retrying until something changes."
                      : undefined
                }
                lamp={healthLamp("failed")}
                meta={
                  <span className="mono">
                    {[
                      failure.op !== null && failure.op !== undefined
                        ? `op ${failure.op}`
                        : undefined,
                      failure.exitCode !== null && failure.exitCode !== undefined
                        ? `exit ${String(failure.exitCode)}`
                        : undefined,
                    ]
                      .filter((part) => part !== undefined)
                      .join(" · ")}
                  </span>
                }
              />
            ) : null}
            <GateFindings
              gates={detail.gates}
              title="Gates holding this snapshot"
              namespace={row.namespace}
            />
          </>
        }
      />
    ),
    tabsLabel: "About this snapshot",
    tabs: [
      { id: "run", label: "Run", render: () => <RunTab detail={detail} now={now} /> },
      { id: "storage", label: "Storage", render: () => <StorageTab detail={detail} /> },
      { id: "retention", label: "Retention", render: () => <RetentionTab detail={detail} /> },
      { id: "lineage", label: "Lineage", render: () => <LineageTab detail={detail} /> },
      ...(detail.logTail.length > 0
        ? [{ id: "log", label: "Log", render: () => <LogTab detail={detail} /> }]
        : []),
      {
        id: "conditions",
        label: "Conditions",
        render: () => (
          <DrawerSection title="Conditions" icon={ScrollText}>
            <ConditionsTable
              conditions={detail.conditions}
              now={now}
              emptyNote="No conditions yet: the operator has not reconciled this snapshot."
            />
          </DrawerSection>
        ),
      },
    ],
    actions: (
      <div className="drawer-actions">
        <SnapshotActions
          row={row}
          extra={
            detail.browsable ? <BrowseFiles namespace={row.namespace} name={row.name} /> : undefined
          }
        />
      </div>
    ),
  };
}
