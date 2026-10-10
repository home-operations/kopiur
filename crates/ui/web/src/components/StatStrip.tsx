import { OctagonX } from "lucide-react";
import type { ReactNode } from "react";

import { formatTimestamp } from "../util/format";
import { NotReported } from "./NotReported";
import type { UnwiredField } from "./unwired";

/**
 * A value that is not there, said as what kind of not-there it is.
 *
 * - `loud` — a real problem the operator should act on ("never verified");
 * - `unreported` — the CRD declares it, no controller writes it;
 * - `na` — it does not apply to this object.
 *
 * A blank cell, or a bare `0`, would say none of these.
 */
export type Absence =
  | { absent: "loud"; text: string }
  | { absent: "unreported"; field: UnwiredField }
  | { absent: "na" };

export interface Stat {
  label: string;
  value: ReactNode | Absence;
  /** RFC3339 instant beneath a relative time, rendered as a `<time>`. */
  abs?: string | undefined;
}

function isAbsence(value: ReactNode | Absence): value is Absence {
  return typeof value === "object" && value !== null && "absent" in value;
}

/** One absence, worded. */
export function AbsenceText({ absence }: { absence: Absence }) {
  switch (absence.absent) {
    case "loud":
      return (
        <span className="absent--loud">
          <OctagonX size={13} strokeWidth={2} aria-hidden="true" />
          {absence.text}
        </span>
      );
    case "unreported":
      return <NotReported field={absence.field} />;
    case "na":
      return <span className="absent">—</span>;
  }
}

/**
 * A handful of named values about one object: one card, cells divided by
 * hairlines, label above value. `page` lays out four across; `card` is the
 * three-up strip inside an object card.
 */
export function StatStrip({
  stats,
  label,
  variant = "page",
}: {
  stats: readonly Stat[];
  label: string;
  variant?: "page" | "card";
}) {
  return (
    <dl className={`stats stats--${variant}`} aria-label={label}>
      {stats.map((stat) => (
        <div className="stats__item" key={stat.label}>
          <dt>{stat.label}</dt>
          <dd>
            {isAbsence(stat.value) ? <AbsenceText absence={stat.value} /> : stat.value}
            {stat.abs !== undefined ? (
              <time className="stats__abs" dateTime={stat.abs}>
                {formatTimestamp(stat.abs)}
              </time>
            ) : null}
          </dd>
        </div>
      ))}
    </dl>
  );
}
