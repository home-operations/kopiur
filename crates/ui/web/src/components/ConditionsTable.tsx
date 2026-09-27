import type { ConditionView } from "../api/types";
import { EMPTY_CELL, relativeTime } from "../util/format";

/**
 * `status.conditions`, verbatim — the evidence for everything a detail screen
 * says above it, which is why it is last and why nothing is summarised away.
 *
 * An empty list is not health: it means the operator has not reconciled the
 * object yet, and the caller supplies the sentence that says so in its own
 * kind's words.
 */
export interface ConditionsTableProps {
  conditions: readonly ConditionView[];
  /** What "no conditions" means for this kind. */
  emptyNote: string;
  /** The clock the "Since" column is measured against. */
  now?: Date | undefined;
  /** Accessible name; distinct when a page carries two of these. */
  label?: string | undefined;
}

export function ConditionsTable({
  conditions,
  emptyNote,
  now = new Date(),
  label = "Conditions",
}: ConditionsTableProps) {
  if (conditions.length === 0) {
    return <p className="page__section-note">{emptyNote}</p>;
  }
  return (
    <div className="ledger-scroll">
      <table className="ledger" aria-label={label}>
        <thead>
          <tr>
            <th scope="col">Type</th>
            <th scope="col">Status</th>
            <th scope="col">Reason</th>
            <th scope="col">Message</th>
            <th scope="col" className="num">
              Since
            </th>
          </tr>
        </thead>
        <tbody>
          {conditions.map((condition) => (
            <tr key={condition.type}>
              <td className="mono">{condition.type}</td>
              <td className="mono">{condition.status}</td>
              <td className="mono">{condition.reason ?? EMPTY_CELL}</td>
              <td>{condition.message ?? EMPTY_CELL}</td>
              <td className="num">
                {condition.lastTransitionTime !== null &&
                condition.lastTransitionTime !== undefined ? (
                  <time dateTime={condition.lastTransitionTime}>
                    {relativeTime(condition.lastTransitionTime, now)}
                  </time>
                ) : (
                  EMPTY_CELL
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
