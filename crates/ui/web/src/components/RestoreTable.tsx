import type { ReactNode } from "react";

import type { RestoreRow } from "../api/types";
import { relativeTime } from "../util/format";
import { ColumnLedger } from "./ColumnLedger";
import { LampBadge } from "./HealthBadge";
import { restorePhaseLamp, restoreProgress } from "./restore";
import { KindChip } from "./KindMark";
import { WireRef } from "./ObjectRef";
import { InspectLink } from "./InspectLink";
import { type SortValue, nameValue, timeValue } from "./tableColumns";
import { RESTORE_COLUMNS, type RestoreColumn } from "./listColumns";
import { healthRank } from "./health";

/**
 * Every `Restore` in scope: where it reads, where it writes, how far it got.
 *
 * The lamp is the phase wearing a colour — see `restore.ts`; the operator
 * publishes no health for a restore, and the mapping is one-to-one with the
 * phases the CRD defines, so nothing is inferred.
 *
 * There is no percentage column and there must not be: `status.progress`
 * carries bytes and files with no total to divide by, so a percentage would
 * have to be invented, and a restore showing a fabricated one is worse than a
 * restore showing bytes.
 */
export interface RestoreTableProps {
  restores: readonly RestoreRow[];
  now?: Date | undefined;
}

export function RestoreTable({ restores, now = new Date() }: RestoreTableProps) {
  return (
    <ColumnLedger
      id="restores"
      rowLinks
      label="Restores"
      className="restore-table"
      columns={RESTORE_COLUMNS}
      rows={restores}
      rowKey={(r) => `${r.namespace}/${r.name}`}
      rowProps={() => ({ "data-kind": "restore" })}
      cell={(restore, id) => restoreCell(restore, id, now)}
      sortValue={restoreSortValue}
    />
  );
}

function restoreSortValue(restore: RestoreRow, id: RestoreColumn): SortValue {
  switch (id) {
    case "restore":
      return nameValue(restore.name, restore.namespace);
    case "phase":
      return healthRank(restorePhaseLamp(restore.phase).key);
    case "repository":
      return restore.repository ?? null;
    case "restored":
      // Bytes, the cell's first figure; a restore that reports only files
      // has nothing comparable to sort by.
      return restore.bytesRestored ?? null;
    case "started":
      return timeValue(restore.startTime);
    case "route":
      return null;
    default:
      return id satisfies never;
  }
}

function restoreCell(restore: RestoreRow, id: RestoreColumn, now: Date): ReactNode {
  switch (id) {
    case "restore":
      return (
        <div className="table__object">
          <KindChip kind="restore" size="sm" />
          <div className="restore-table__object">
            <span className="label-strip">
              <span className="label-strip__name">
                <InspectLink
                  className="row-link"
                  target={{
                    kind: "restore",
                    namespace: restore.namespace,
                    name: restore.name,
                  }}
                >
                  {restore.name}
                </InspectLink>
              </span>
            </span>
            <span className="restore-table__namespace mono">{restore.namespace}</span>
          </div>
        </div>
      );
    case "phase":
      return <LampBadge lamp={restorePhaseLamp(restore.phase)} />;
    case "route":
      return (
        <div className="restore-table__route">
          <span className="mono">{restore.sourceKind ?? "source not pinned"}</span>
          <span className="restore-table__arrow" aria-hidden="true">
            →
          </span>
          <span className="visually-hidden">into</span>
          <span className="mono">{restore.targetKind}</span>
          {restore.claims.length > 0 ? (
            <span className="restore-table__note">
              {restore.claims.length} {restore.claims.length === 1 ? "claim" : "claims"}
            </span>
          ) : null}
        </div>
      );
    case "repository":
      return <WireRef value={restore.repository} contextNamespace={restore.namespace} />;
    case "restored":
      return restoreProgress(restore);
    case "started":
      return restore.startTime !== null && restore.startTime !== undefined ? (
        <time dateTime={restore.startTime} title={restore.startTime}>
          {relativeTime(restore.startTime, now)}
        </time>
      ) : (
        <span className="restore-table__absent">not started</span>
      );
    default:
      return id satisfies never;
  }
}
