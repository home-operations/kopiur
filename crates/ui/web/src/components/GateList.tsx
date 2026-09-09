import type { GateDescriptor } from "../api/types";
import { HealthBadge } from "./HealthBadge";
import { gateScopeKinds, gateSeverityLamp, sortGates } from "./gates";

/**
 * The structural-gate registry as a ledger: severity lamp, the condition
 * that carries the gate, the status value that means blocked (per gate —
 * some block on `False`, some on `True`), the reason the reconciler writes,
 * and the kinds the condition appears on.
 *
 * Rendered from `GateDescriptor` alone, so a gate a newer operator raises
 * is explained in the operator's own words with no lookup table here.
 */
export interface GateListProps {
  gates: readonly GateDescriptor[];
}

export function GateList({ gates }: GateListProps) {
  return (
    <table className="ledger gate-list" aria-label="Structural gates">
      <thead>
        <tr>
          <th scope="col">Severity</th>
          <th scope="col">Condition</th>
          <th scope="col">Blocked when</th>
          <th scope="col">Reason</th>
          <th scope="col">Applies to</th>
        </tr>
      </thead>
      <tbody>
        {sortGates(gates).map((gate) => {
          const lamp = gateSeverityLamp(gate.severity);
          return (
            <tr key={`${gate.condition}/${gate.reason}`}>
              <td>
                <HealthBadge health={lamp.key} label={lamp.word} />
              </td>
              <td className="mono">{gate.condition}</td>
              <td className="mono gate-list__status">
                <span className="gate-list__status-eq">status =</span> {gate.blockedStatus}
              </td>
              <td className="mono">{gate.reason}</td>
              <td>
                <ul className="gate-list__kinds" aria-label="Kinds">
                  {gateScopeKinds(gate.scope).map((kind) => (
                    <li key={kind} className="label-strip">
                      <span className="label-strip__kind">{kind}</span>
                    </li>
                  ))}
                </ul>
              </td>
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}
