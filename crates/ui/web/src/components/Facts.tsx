import type { ReactNode } from "react";

/**
 * A definition list: label caps beside the value they name.
 *
 * The detail screens are mostly this — a handful of named values per region,
 * each region a `page__section`. A `dl` rather than a two-column table because
 * these are labelled values, not rows of comparable records: a screen reader
 * announces "Snapshots, 412" instead of walking a one-row grid.
 *
 * A fact whose value is absent is dropped by the caller, never rendered with
 * a blank value — except where the absence is itself the fact, which is what
 * `NotReported` is for.
 */
export interface Fact {
  term: string;
  value: ReactNode;
}

export interface FactsProps {
  /** The group's accessible name — the section heading, usually. */
  label: string;
  facts: readonly Fact[];
}

export function Facts({ label, facts }: FactsProps) {
  return (
    <dl className="facts" aria-label={label}>
      {facts.map((fact) => (
        <div className="facts__row" key={fact.term}>
          <dt>{fact.term}</dt>
          <dd>{fact.value}</dd>
        </div>
      ))}
    </dl>
  );
}
