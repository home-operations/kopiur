import { ScrollText } from "lucide-react";

import type { RestoreDetail } from "../../../api/types";
import { humanBytes, humanDuration, relativeTime } from "../../../util/format";
import { ConditionsTable } from "../../ConditionsTable";
import { Finding } from "../../Finding";
import { healthLamp } from "../../health";
import { parseRef } from "../../kind";
import { restoreVerdict } from "../../restore";
import { Chain, type ChainItem } from "../Chain";
import { DrawerHead } from "../DrawerHead";
import { DrawerSection } from "../DrawerSection";
import type { DrawerView } from "../views";
import { LogTab, SourceTab, TargetTab } from "./RestoreTabs";

/** Seconds from start to end; `null` while it runs or when either is unwritten. */
function tookSeconds(start: string | null | undefined, end: string | null | undefined) {
  if (start === null || start === undefined || end === null || end === undefined) return null;
  const ms = Date.parse(end) - Date.parse(start);
  return Number.isNaN(ms) ? null : ms / 1000;
}

/**
 * One restore, in the order someone recovering data needs it: its phase and
 * how far it got, the repository and snapshot it reads and the claims it
 * writes, and — when it failed — why, at the top. The wire carries no policy
 * for the snapshot, so the chain stops there rather than guessing one. A
 * restore has nothing to act on, so it has no action bar.
 */
export function restoreView(detail: RestoreDetail, now: Date): DrawerView {
  const { row } = detail;
  const verdict = restoreVerdict(row);
  const repo = row.repository ? parseRef(row.repository) : null;
  const snapshot = detail.source?.snapshot;
  const failure = detail.failure;
  const readFrom: ChainItem[] = [];
  if (repo !== null) readFrom.push({ ref: repo });
  else if (row.repository) readFrom.push({ text: row.repository });
  if (snapshot !== null && snapshot !== undefined) {
    readFrom.push({
      ref: { kind: "snapshot", name: snapshot.name, namespace: snapshot.namespace },
    });
  }
  return {
    lamp: verdict.lamp,
    head: (
      <DrawerHead
        verdict={{ label: "Restore verdict", lamp: verdict.lamp, text: verdict.text }}
        chain={
          <Chain
            kind="restore"
            namespace={row.namespace}
            before={[{ label: "Reads", items: readFrom }]}
            after={[
              {
                label: "Writes into",
                items: (row.claims.length > 0
                  ? row.claims.map((c) => c.pvc)
                  : detail.target?.pvc
                    ? [detail.target.pvc]
                    : []
                ).map((pvc) => ({ text: pvc })),
              },
            ]}
          />
        }
        stats={[
          { label: "Restored", value: humanBytes(row.bytesRestored) },
          {
            label: "Files",
            value:
              row.filesRestored !== null && row.filesRestored !== undefined
                ? row.filesRestored.toLocaleString()
                : { absent: "na" },
          },
          { label: "Took", value: humanDuration(tookSeconds(row.startTime, row.endTime)) },
          {
            label: "Started",
            value:
              row.startTime !== null && row.startTime !== undefined
                ? relativeTime(row.startTime, now)
                : { absent: "na" },
            abs: row.startTime ?? undefined,
          },
        ]}
        statsLabel={`Restore ${row.name} at a glance`}
        findings={
          failure !== null && failure !== undefined ? (
            <section className="drawer__region" aria-label="Why it failed">
              <Finding
                title={failure.kopiaErrorClass ?? "Failure"}
                what={failure.message ?? "The operator recorded a failure with no message."}
                why={
                  failure.op !== null && failure.op !== undefined
                    ? `The ${failure.op} step of the mover Job is what failed.`
                    : undefined
                }
                fix={
                  failure.retryRecommended === true
                    ? "A retry is likely to succeed: create the restore again."
                    : failure.retryRecommended === false
                      ? "A retry will not help until the cause above is fixed."
                      : undefined
                }
                lamp={healthLamp("failed")}
                meta={
                  failure.exitCode !== null && failure.exitCode !== undefined ? (
                    <span className="mono">mover exit code {failure.exitCode}</span>
                  ) : undefined
                }
              />
            </section>
          ) : undefined
        }
      />
    ),
    tabsLabel: "About this restore",
    tabs: [
      { id: "source", label: "Source", render: () => <SourceTab detail={detail} now={now} /> },
      { id: "target", label: "Target", render: () => <TargetTab detail={detail} now={now} /> },
      { id: "log", label: "Log", render: () => <LogTab detail={detail} /> },
      {
        id: "conditions",
        label: "Conditions",
        render: () => (
          <DrawerSection title="Conditions" icon={ScrollText}>
            <ConditionsTable
              conditions={detail.conditions}
              now={now}
              emptyNote="No conditions yet: the operator has not reconciled this restore."
            />
          </DrawerSection>
        ),
      },
    ],
  };
}
