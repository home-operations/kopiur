import { Link } from "@tanstack/react-router";

import type { RestoreRow } from "../api/types";
import { EMPTY_CELL, relativeTime } from "../util/format";
import { LampBadge } from "./HealthBadge";
import { restorePhaseLamp, restoreProgress } from "./restore";

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
    <div className="ledger-scroll">
      <table className="ledger restore-table" aria-label="Restores">
        <thead>
          <tr>
            <th scope="col">Restore</th>
            <th scope="col">Phase</th>
            <th scope="col">Reads → writes</th>
            <th scope="col">Repository</th>
            <th scope="col" className="num">
              Restored
            </th>
            <th scope="col" className="num">
              Started
            </th>
          </tr>
        </thead>
        <tbody>
          {restores.map((restore) => (
            <tr key={`${restore.namespace}/${restore.name}`}>
              <td>
                <div className="restore-table__object">
                  <span className="label-strip">
                    <span className="label-strip__kind">Restore</span>
                    <span className="label-strip__name">
                      <Link
                        to="/restores/$namespace/$name"
                        params={{ namespace: restore.namespace, name: restore.name }}
                      >
                        {restore.name}
                      </Link>
                    </span>
                  </span>
                  <span className="restore-table__namespace mono">{restore.namespace}</span>
                </div>
              </td>
              <td>
                <LampBadge lamp={restorePhaseLamp(restore.phase)} />
              </td>
              <td>
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
              </td>
              <td className="mono">{restore.repository ?? EMPTY_CELL}</td>
              <td className="num">{restoreProgress(restore)}</td>
              <td className="num">
                {restore.startTime !== null && restore.startTime !== undefined ? (
                  <time dateTime={restore.startTime} title={restore.startTime}>
                    {relativeTime(restore.startTime, now)}
                  </time>
                ) : (
                  <span className="restore-table__absent">not started</span>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
