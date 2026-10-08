import type { ReactNode } from "react";

import type { Lamp } from "../health";
import { type Stat, StatStrip } from "../StatStrip";

export interface DrawerVerdict {
  /** Names the verdict for assistive technology: "Snapshot verdict". */
  label: string;
  lamp: Lamp;
  /** One sentence: what state it is in and why that matters. */
  text: ReactNode;
}

export interface DrawerHeadProps {
  verdict?: DrawerVerdict | undefined;
  /** Where it sits — a {@link Chain}. */
  chain?: ReactNode;
  stats?: readonly Stat[] | undefined;
  statsLabel?: string | undefined;
  /** What is holding it back or why it failed; loud, and only when there is any. */
  findings?: ReactNode;
}

/**
 * The top of the resource drawer: its main information, above the tabs. The
 * verdict first (a lettered lamp and one sentence), then where it sits, then
 * its few headline facts, then anything that needs fixing.
 *
 * No heading: the panel's title is the resource's name.
 */
export function DrawerHead({ verdict, chain, stats, statsLabel, findings }: DrawerHeadProps) {
  const Icon = verdict?.lamp.icon;
  return (
    <div className="drawer__head">
      {verdict !== undefined && Icon !== undefined ? (
        <p className="verdict" role="status" aria-label={verdict.label}>
          <span className="verdict__lamp" data-health={verdict.lamp.key}>
            <Icon size={16} strokeWidth={2} aria-hidden="true" />
            <span>{verdict.lamp.word}</span>
          </span>
          <span className="verdict__text">{verdict.text}</span>
        </p>
      ) : null}
      {chain}
      {stats !== undefined && stats.length > 0 ? (
        <StatStrip label={statsLabel ?? "At a glance"} variant="card" stats={stats} />
      ) : null}
      {findings !== undefined && findings !== null && findings !== false ? (
        <div className="drawer__findings">{findings}</div>
      ) : null}
    </div>
  );
}
