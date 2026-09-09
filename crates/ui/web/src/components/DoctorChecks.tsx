import { ShieldOff } from "lucide-react";

import type { DoctorCheckView } from "../api/types";
import { EMPTY_CELL } from "../util/format";
import { Finding } from "./Finding";
import { HealthBadge } from "./HealthBadge";
import { doctorCheckScope, doctorOutcomeLamp, isRbacDegraded } from "./doctor";

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

function scopeCell(checkId: string, namespace: string | undefined) {
  switch (doctorCheckScope(checkId)) {
    case "namespace":
      return namespace !== undefined ? (
        <>
          scoped to <span className="mono">{namespace}</span>
        </>
      ) : (
        "every namespace"
      );
    case "installation":
      return namespace !== undefined ? (
        <>
          installation-wide <span className="doctor-checks__note">(namespace does not apply)</span>
        </>
      ) : (
        "installation-wide"
      );
    case "unknown":
      return <span className="doctor-checks__note">scope not known to this bundle</span>;
  }
}

export function DoctorChecks({ checks, namespace }: DoctorChecksProps) {
  return (
    <table className="ledger doctor-checks" aria-label="Doctor checks">
      <thead>
        <tr>
          <th scope="col">Outcome</th>
          <th scope="col">Check</th>
          <th scope="col">Scope</th>
          <th scope="col">Finding</th>
        </tr>
      </thead>
      <tbody>
        {checks.map((check) => {
          const lamp = doctorOutcomeLamp(check.outcome);
          const rbac = isRbacDegraded(check);
          return (
            <tr key={check.check} data-degraded={rbac ? "rbac" : undefined}>
              <td>
                <HealthBadge health={lamp.key} label={lamp.word} />
              </td>
              <td className="doctor-checks__check">
                <span className="doctor-checks__title">{check.title}</span>
                <span className="mono doctor-checks__id">{check.check}</span>
              </td>
              <td className="doctor-checks__scope">{scopeCell(check.check, namespace)}</td>
              <td className="doctor-checks__finding">
                {check.what !== undefined && check.what !== null && check.what.length > 0 ? (
                  <Finding what={check.what} why={check.why} fix={check.fix} />
                ) : (
                  EMPTY_CELL
                )}
                {rbac ? (
                  <p className="doctor-checks__rbac">
                    <ShieldOff size={14} strokeWidth={2} aria-hidden="true" />
                    <span>
                      Not permitted for the signed-in user: kopiur-ui ran this check as you and the
                      cluster refused. The grant named above enables it; the cluster itself may be
                      fine.
                    </span>
                  </p>
                ) : null}
              </td>
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}
