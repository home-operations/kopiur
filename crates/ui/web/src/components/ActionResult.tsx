import { CircleCheck } from "lucide-react";

import type { ActionReceipt, Problem } from "../api/types";
import { LampBadge } from "./HealthBadge";
import { kindOfLabel } from "./kind";
import { ObjectRef } from "./ObjectRef";
import { ProblemBanner } from "./ProblemBanner";
import { relativeTime } from "../util/format";

/** The receipt's pill: accepted, in the healthy lamp's colours, worded as asked. */
const REQUESTED = { key: "healthy", word: "Requested", icon: CircleCheck } as const;

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
      <span className="receipt__pill">
        <LampBadge lamp={REQUESTED} />
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
              <li key={`${ref.namespace}/${ref.name}`}>
                <CreatedRef kind={receipt.kind} namespace={ref.namespace} name={ref.name} />
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

/**
 * One created object, as a reference to its page in its own kind. A kind
 * this bundle does not know is still named — a newer server's object must
 * not vanish from the receipt that says it was made.
 */
function CreatedRef({ kind, namespace, name }: { kind: string; namespace: string; name: string }) {
  const known = kindOfLabel(kind);
  if (known === null) {
    return (
      <span className="label-strip">
        <span className="label-strip__kind">{namespace}</span>
        <span className="label-strip__name">{name}</span>
      </span>
    );
  }
  return <ObjectRef kind={known} name={name} namespace={namespace} />;
}
