import { CircleCheck } from "lucide-react";

import type { ActionReceipt, Problem } from "../api/types";
import { ProblemBanner } from "./ProblemBanner";
import { relativeTime } from "../util/format";

/**
 * What a mutating action answered: the receipt, or the problem that refused
 * it.
 *
 * Two rules the API forces on every action surface, so they live here once.
 *
 * **`note` is not decoration.** It is where an action that was accepted but
 * not performed explains itself — a suspend that changed nothing because the
 * object was already suspended, a delete the mass-deletion breaker is holding
 * (addenda item 19). Rendering the receipt without it turns "held" into
 * "done".
 *
 * **Accepted is not done.** Three of the actions answer `202`: the request
 * stamped an annotation a reconciler will honor. The wording says *requested*
 * and the receipt's `requestedAt` says when, because that instant is also the
 * token the operator echoes back on the run it produced.
 *
 * A live region, so a keyboard user who pressed the button hears the answer
 * without hunting for it.
 */
export interface ActionResultProps {
  /** The action's own name, e.g. "Suspend" — read back in the answer. */
  label: string;
  receipt?: ActionReceipt | undefined;
  problem?: Problem | undefined;
}

export function ActionResult({ label, receipt, problem }: ActionResultProps) {
  if (problem !== undefined) {
    return <ProblemBanner problem={problem} source={label} />;
  }
  if (receipt === undefined) {
    return null;
  }
  const note = receipt.note;
  return (
    <div className="receipt" role="status">
      <span className="receipt__icon">
        <CircleCheck size={16} strokeWidth={2} aria-hidden="true" />
      </span>
      <div className="receipt__body">
        <p className="receipt__what">
          {label} requested{receipt.kind.length > 0 ? ` for ${receipt.kind}` : ""}.
        </p>
        {note !== null && note !== undefined && note.length > 0 ? (
          <p className="receipt__note">{note}</p>
        ) : null}
        {receipt.created.length > 0 ? (
          <ul className="receipt__created" aria-label="Created">
            {receipt.created.map((ref) => (
              <li key={`${ref.namespace}/${ref.name}`} className="label-strip">
                <span className="label-strip__kind">{ref.namespace}</span>
                <span className="label-strip__name">{ref.name}</span>
              </li>
            ))}
          </ul>
        ) : null}
        {receipt.requestedAt !== null && receipt.requestedAt !== undefined ? (
          <p className="receipt__meta mono">
            <time dateTime={receipt.requestedAt}>{relativeTime(receipt.requestedAt)}</time>
          </p>
        ) : null}
      </div>
    </div>
  );
}
