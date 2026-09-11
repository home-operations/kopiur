import { Link } from "@tanstack/react-router";

import type { PolicyRow } from "../api/types";
import { relativeTime } from "../util/format";
import { LampBadge } from "./HealthBadge";
import { healthLamp } from "./health";
import { snapshotCount } from "./policy";

/**
 * Every `SnapshotPolicy` in scope: what it writes into, whether it is
 * running, and when it last worked.
 *
 * There is no health column, because a policy has none — see `policy.ts`.
 * What the ledger reports instead is the pair of facts an operator actually
 * asks a recipe about: is it suspended, and when did it last succeed. A
 * policy that has never produced a snapshot says so in words rather than
 * leaving the cell blank, because "never" is the single most important thing
 * a backup recipe can tell you about itself.
 */
export interface PolicyTableProps {
  policies: readonly PolicyRow[];
  /** The clock ages are measured against. */
  now?: Date | undefined;
}

export function PolicyTable({ policies, now = new Date() }: PolicyTableProps) {
  return (
    <div className="ledger-scroll">
      <table className="ledger policy-table" aria-label="Policies">
        <thead>
          <tr>
            <th scope="col">Policy</th>
            <th scope="col">Writes into</th>
            <th scope="col">State</th>
            <th scope="col">Last snapshot</th>
            <th scope="col">Last verified</th>
            <th scope="col" className="num">
              Snapshots
            </th>
          </tr>
        </thead>
        <tbody>
          {policies.map((policy) => (
            <tr key={`${policy.namespace}/${policy.name}`}>
              <td>
                <div className="policy-table__object">
                  <span className="label-strip">
                    <span className="label-strip__kind">SnapshotPolicy</span>
                    <span className="label-strip__name">
                      <Link
                        to="/policies/$namespace/$name"
                        params={{ namespace: policy.namespace, name: policy.name }}
                      >
                        {policy.name}
                      </Link>
                    </span>
                  </span>
                  <span className="policy-table__namespace mono">{policy.namespace}</span>
                </div>
              </td>
              <td>
                <Repositories policy={policy} />
              </td>
              <td>
                {policy.suspended ? (
                  <LampBadge lamp={healthLamp("suspended")} />
                ) : (
                  <span className="policy-table__active">Active</span>
                )}
              </td>
              <td>
                <Instant at={policy.lastSuccessfulSnapshot} now={now} never="never succeeded" />
              </td>
              <td>
                <Instant at={policy.lastVerified} now={now} never="never verified" />
              </td>
              <td className="num">{snapshotCount(policy.activeSnapshotCount)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

/** The repositories a policy writes into, and whether that is a fan-out. */
function Repositories({ policy }: { policy: PolicyRow }) {
  if (policy.repositories.length === 0) {
    return <span className="policy-table__never">names no repository</span>;
  }
  return (
    <div className="policy-table__repos">
      <ul className="ref-list" aria-label={`Repositories for ${policy.name}`}>
        {policy.repositories.map((repository) => (
          <li key={repository} className="label-strip">
            <span className="label-strip__name">{repository}</span>
          </li>
        ))}
      </ul>
      {policy.multiRepo ? (
        <span className="policy-table__note">fans out — one Snapshot per repository, per run</span>
      ) : null}
    </div>
  );
}

/**
 * An instant with a direction, or the loud word for a thing that has never
 * happened. A dash here would read as "not applicable"; for "has this backup
 * ever worked" it never is.
 */
function Instant({ at, now, never }: { at: string | null | undefined; now: Date; never: string }) {
  if (at === null || at === undefined || at.length === 0) {
    return <span className="policy-table__never">{never}</span>;
  }
  return (
    <time dateTime={at} title={at}>
      {relativeTime(at, now)}
    </time>
  );
}

/**
 * The same cell, for the detail screen — so the list and the detail cannot
 * word the same absence differently.
 */
export function LastSuccess({
  at,
  now = new Date(),
  never = "never succeeded",
}: {
  at: string | null | undefined;
  now?: Date;
  never?: string;
}) {
  return <Instant at={at} now={now} never={never} />;
}
