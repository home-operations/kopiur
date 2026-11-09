import { ShieldOff } from "lucide-react";
import type { ReactNode } from "react";

import type { DoctorCheckView, DoctorScopeView } from "../api/types";
import { unknownVariant } from "../util/assertNever";
import { EMPTY_CELL } from "../util/format";
import { ColumnLedger } from "./ColumnLedger";
import { Finding } from "./Finding";
import { HealthBadge } from "./HealthBadge";
import { doctorOutcomeLamp, isRbacDegraded } from "./doctor";
import type { SortValue } from "./tableColumns";
import { DOCTOR_COLUMNS, type DoctorColumn } from "./listColumns";
import { healthRank } from "./health";

/**
 * The doctor report as a ledger: one row per check, the outcome as a lamp,
 * what the namespace scope does to that check, and the finding.
 *
 * Only a failure carries what / why / fix; a warning is one sentence and is
 * rendered as one. A warning doctor issued because the signed-in user lacks
 * a grant is marked as such — the console asked the cluster as that user
 * and was refused, so the check could not run, not the cluster is broken.
 */
export interface DoctorChecksProps {
  checks: readonly DoctorCheckView[];
  /** The `?namespace=` the report was scoped to; undefined is the whole installation. */
  namespace: string | undefined;
}

/**
 * How much of the cluster a check read, in the operator's words.
 *
 * The value is the server's (`DoctorCheckView.scope`), exhaustive over the
 * generated `DoctorScopeView`. It replaced a two-way table kept here beside
 * a prose contract, and that table had already gone wrong: `list_repos` lists
 * `Repository` inside `?namespace=` but `ClusterRepository` cluster-wide, so
 * the repository checks were filed as namespace-scoped and the row said
 * "scoped to media" over answers that covered every other namespace too.
 * `mixed` is the server saying that out loud.
 *
 * `DoctorScopeView` is unit-only with NO fallback variant (ui-model doc), so
 * the `default` arm is unreachable at compile time and still renders the raw
 * word at run time — a newer server's scope must read as unread, not as a
 * guess in either direction.
 */
function scopeCell(scope: DoctorScopeView, namespace: string | undefined) {
  switch (scope) {
    case "namespace":
      return namespace !== undefined ? (
        <>
          scoped to <span className="mono">{namespace}</span>
        </>
      ) : (
        "every namespace"
      );
    case "mixed":
      return namespace !== undefined ? (
        <>
          scoped to <span className="mono">{namespace}</span>{" "}
          <span className="doctor-checks__note">(plus cluster-scoped objects)</span>
        </>
      ) : (
        <>
          every namespace <span className="doctor-checks__note">(and cluster-scoped objects)</span>
        </>
      );
    case "installation":
      return namespace !== undefined ? (
        <>
          installation-wide <span className="doctor-checks__note">(namespace does not apply)</span>
        </>
      ) : (
        "installation-wide"
      );
    default:
      return (
        <span className="doctor-checks__note">
          scope not known to this bundle:{" "}
          <span className="mono">{unknownVariant(scope, "DoctorScopeView")}</span>
        </span>
      );
  }
}

export function DoctorChecks({ checks, namespace }: DoctorChecksProps) {
  return (
    <ColumnLedger
      id="doctor"
      label="Doctor checks"
      className="doctor-checks"
      columns={DOCTOR_COLUMNS}
      rows={checks}
      rowKey={(check) => check.check}
      rowProps={(check) => ({ "data-degraded": isRbacDegraded(check) ? "rbac" : undefined })}
      cell={(check, id) => doctorCell(check, id, namespace)}
      sortValue={doctorSortValue}
    />
  );
}

function doctorSortValue(check: DoctorCheckView, id: DoctorColumn): SortValue {
  switch (id) {
    case "outcome":
      return healthRank(doctorOutcomeLamp(check.outcome).key);
    case "check":
      return check.title;
    case "scope":
      // Grouped by what the check covers: the cluster, a namespace, or both.
      return check.scope;
    case "finding":
      return null;
    default:
      return id satisfies never;
  }
}

function doctorCell(
  check: DoctorCheckView,
  id: DoctorColumn,
  namespace: string | undefined,
): ReactNode {
  switch (id) {
    case "outcome": {
      const lamp = doctorOutcomeLamp(check.outcome);
      return <HealthBadge health={lamp.key} label={lamp.word} />;
    }
    case "check":
      // One line, id first, the way the check reads in `?checks=` and the
      // CLI's `-o json`: `recent-failures: no recent failed snapshots/restores`.
      // Stacked on two lines the id read as a second, unexplained finding
      // (#695).
      return (
        <p className="doctor-checks__check">
          <span className="mono doctor-checks__id">{check.check}</span>
          <span className="doctor-checks__sep" aria-hidden="true">
            :{" "}
          </span>
          <span className="doctor-checks__title">{check.title}</span>
        </p>
      );
    case "scope":
      return scopeCell(check.scope, namespace);
    case "finding":
      return (
        <div className="doctor-checks__finding">
          {/* A pass has nothing to say: whatever text rode along, a passing
              row shows the empty cell, so an ok check never reads as a finding. */}
          {check.outcome !== "Pass" &&
          check.what !== undefined &&
          check.what !== null &&
          check.what.length > 0 ? (
            <Finding what={check.what} why={check.why} fix={check.fix} />
          ) : (
            EMPTY_CELL
          )}
          {isRbacDegraded(check) ? (
            <p className="doctor-checks__rbac">
              <ShieldOff size={14} strokeWidth={2} aria-hidden="true" />
              <span>
                You are not permitted to run this check. The grant named above enables it; the
                cluster itself may be fine.
              </span>
            </p>
          ) : null}
        </div>
      );
    default:
      return id satisfies never;
  }
}
