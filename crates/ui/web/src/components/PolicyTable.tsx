import type { ReactNode } from "react";

import type { PolicyRow } from "../api/types";
import { relativeTime } from "../util/format";
import { ColumnLedger } from "./ColumnLedger";
import { LampBadge } from "./HealthBadge";
import { healthLamp, loudLamp } from "./health";
import { snapshotCount } from "./policy";
import { KindChip } from "./KindMark";
import { WireRef } from "./ObjectRef";
import { InspectLink } from "./InspectLink";
import type { ColumnSpec } from "./tableColumns";

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

type PolicyColumn =
  | "policy"
  | "writesInto"
  | "state"
  | "lastSnapshot"
  | "lastVerified"
  | "snapshots";

const POLICY_COLUMNS: readonly ColumnSpec<PolicyColumn>[] = [
  { id: "policy", label: "Policy", width: "auto", min: 220, locked: true, stripe: true },
  { id: "writesInto", label: "Writes into", width: 260, min: 220 },
  { id: "state", label: "State", width: 130, min: 80 },
  { id: "lastSnapshot", label: "Last snapshot", width: 170, min: 130 },
  { id: "lastVerified", label: "Last verified", width: 170, min: 130 },
  { id: "snapshots", label: "Snapshots", width: 110, min: 110, numeric: true },
];

export function PolicyTable({ policies, now = new Date() }: PolicyTableProps) {
  return (
    <ColumnLedger
      id="policies"
      label="Policies"
      className="policy-table"
      columns={POLICY_COLUMNS}
      rows={policies}
      rowKey={(p) => `${p.namespace}/${p.name}`}
      rowProps={() => ({ "data-kind": "snapshot-policy" })}
      cell={(policy, id) => policyCell(policy, id, now)}
    />
  );
}

function policyCell(policy: PolicyRow, id: PolicyColumn, now: Date): ReactNode {
  switch (id) {
    case "policy":
      return (
        <div className="table__object">
          <KindChip kind="snapshotPolicy" size="sm" />
          <div className="policy-table__object">
            <span className="label-strip">
              <span className="label-strip__name">
                <InspectLink
                  className="row-link"
                  target={{
                    kind: "snapshotPolicy",
                    namespace: policy.namespace,
                    name: policy.name,
                  }}
                >
                  {policy.name}
                </InspectLink>
              </span>
            </span>
            <span className="policy-table__namespace mono">{policy.namespace}</span>
          </div>
        </div>
      );
    case "writesInto":
      return <Repositories policy={policy} />;
    case "state":
      return policy.suspended ? (
        <LampBadge lamp={healthLamp("suspended")} />
      ) : (
        <span className="policy-table__active">Active</span>
      );
    case "lastSnapshot":
      return <Instant at={policy.lastSuccessfulSnapshot} now={now} never="never succeeded" />;
    case "lastVerified":
      return <Instant at={policy.lastVerified} now={now} never="never verified" />;
    case "snapshots":
      return snapshotCount(policy.activeSnapshotCount);
    default:
      return id satisfies never;
  }
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
          <li key={repository}>
            <WireRef value={repository} contextNamespace={policy.namespace} />
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
 *
 * The absence is a lamp, not coloured prose. This column is the only alarm a
 * policy row has — there is no health column, because a policy has no health
 * — so drawing it in the failed ink alone left a colour-blind reader with
 * "never verified" set exactly like "3 days ago". `loudLamp` keeps the
 * wording and adds the icon; nothing here claims a `Health`.
 */
function Instant({ at, now, never }: { at: string | null | undefined; now: Date; never: string }) {
  if (at === null || at === undefined || at.length === 0) {
    return <LampBadge lamp={loudLamp(never)} />;
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
