import type { ReactNode } from "react";

import type { ObjectKind } from "../api/types";
import type { Lamp } from "./health";
import { KindChip, KindName } from "./KindMark";
import { KIND_META } from "./kind";
import { ObjectRef } from "./ObjectRef";
import { type Stat, StatStrip } from "./StatStrip";

/** One upstream hop: the object this one was made by, or made into. */
export interface TrailHop {
  kind: ObjectKind;
  name: string;
  namespace?: string | undefined;
  health?: Lamp | undefined;
}

export interface DetailHeaderProps {
  kind: ObjectKind;
  name: string;
  namespace?: string | undefined;
  lamp: Lamp;
  /** The verdict's accessible name: "Snapshot verdict". */
  verdictLabel: string;
  /** One sentence: what state the object is in and why it matters. */
  verdict: ReactNode;
  /** Upstream hops, outermost first; the trail ends at this object. */
  trail?: readonly TrailHop[] | undefined;
  actions?: ReactNode;
  /** Four facts, shown as the page stat strip under the hero. */
  stats?: readonly Stat[] | undefined;
}

/**
 * The top of an object's page: where it sits (the trail), then a hero card
 * striped in its kind — chip, kind name, name, namespace and pill — with the
 * one-sentence verdict and the actions, then its four facts.
 *
 * Renders no `h1`: the shell's page title is the page's one. The verdict keeps
 * the `.verdict__lamp` lettered lamp every detail screen has always had.
 */
export function DetailHeader({
  kind,
  name,
  namespace,
  lamp,
  verdictLabel,
  verdict,
  trail,
  actions,
  stats,
}: DetailHeaderProps) {
  const meta = KIND_META[kind];
  const Icon = lamp.icon;
  return (
    <div className="detail-header">
      {trail !== undefined && trail.length > 0 ? (
        <ol className="trail" aria-label="Where this sits">
          {trail.map((hop) => (
            <li key={`${hop.kind}/${hop.namespace ?? ""}/${hop.name}`}>
              <ObjectRef
                kind={hop.kind}
                name={hop.name}
                namespace={hop.namespace}
                contextNamespace={namespace}
                health={hop.health}
              />
              <span className="trail__sep" aria-hidden="true">
                ›
              </span>
            </li>
          ))}
          <li className="trail__here">
            this {meta.label.replace(/^Snapshot(?=Policy|Schedule)/, "").toLowerCase()}
          </li>
        </ol>
      ) : null}

      <section
        className="detail-hero has-stripe"
        data-kind={meta.slug}
        aria-label={`${meta.label} ${name}`}
      >
        <div className="detail-hero__top">
          <KindChip kind={kind} size="lg" />
          <h2 className="detail-hero__id">
            <KindName kind={kind} />
            <span className="detail-hero__name">{name}</span>
            {namespace !== undefined ? <span className="object-ns">{namespace}</span> : null}
          </h2>
        </div>
        <p className="verdict" role="status" aria-label={verdictLabel}>
          <span className="verdict__lamp" data-health={lamp.key}>
            <Icon size={16} strokeWidth={2} aria-hidden="true" />
            <span>{lamp.word}</span>
          </span>
          <span className="verdict__text">{verdict}</span>
        </p>
        {actions !== undefined && actions !== null ? (
          <section className="detail-hero__actions" aria-label="Actions">
            {actions}
          </section>
        ) : null}
      </section>

      {stats !== undefined && stats.length > 0 ? (
        <StatStrip label={`${meta.label} ${name} at a glance`} stats={stats} />
      ) : null}
    </div>
  );
}
