import { ArrowRight } from "lucide-react";
import { Fragment } from "react";

import type { ObjectKind } from "../api/types";
import { detailHref } from "./kind";
import { ObjectCard } from "./ObjectCard";
import type { CardRow } from "./objectCard";
import { ObjectRef } from "./ObjectRef";

/**
 * One object in a lane: a card when its row is loaded, otherwise a reference —
 * a name the server gave with no row behind it is still named, never guessed.
 */
export type LaneItem =
  | { card: CardRow }
  | { ref: { kind: ObjectKind; name: string; namespace?: string | undefined } };

export interface Lane {
  /** The visible lane title: "Written by". */
  title: string;
  /** The lane's accessible name, when it should say more: "Policies writing here". */
  label: string;
  items: readonly LaneItem[];
  /** Why the lane is empty, in a sentence. */
  empty: string;
  /** `stats` for the object the page is about; the lanes around it are summaries. */
  variant?: "summary" | "stats" | undefined;
}

/**
 * What feeds an object and what it feeds, left to right: fired by → written
 * by → this object → copies to. Every item is a real card or reference, so
 * the lanes are also the page's spoken account of the relationships; the
 * arrows between them are decoration.
 */
export function FlowLanes({ label, lanes }: { label: string; lanes: readonly Lane[] }) {
  return (
    <section className="flow-lanes" aria-label={label}>
      {lanes.map((lane, index) => (
        <Fragment key={lane.label}>
          {index > 0 ? (
            <span className="flow-lanes__arrow" aria-hidden="true">
              <ArrowRight size={16} strokeWidth={2} />
            </span>
          ) : null}
          <section className="flow-lane" aria-label={lane.label}>
            <h3 className="flow-lane__title">{lane.title}</h3>
            {lane.items.length === 0 ? (
              <p className="flow-lane__empty">{lane.empty}</p>
            ) : (
              lane.items.map((item) =>
                "card" in item ? (
                  <ObjectCard
                    key={`${item.card.kind}/${item.card.row.namespace ?? ""}/${item.card.row.name}`}
                    card={item.card}
                    variant={lane.variant ?? "summary"}
                  />
                ) : (
                  <ObjectRef
                    key={`${item.ref.kind}/${item.ref.namespace ?? ""}/${item.ref.name}`}
                    kind={item.ref.kind}
                    name={item.ref.name}
                    namespace={item.ref.namespace}
                    to={detailHref(item.ref.kind, item.ref.name, item.ref.namespace)}
                  />
                ),
              )
            )}
          </section>
        </Fragment>
      ))}
    </section>
  );
}
