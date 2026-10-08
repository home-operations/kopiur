import { Link } from "@tanstack/react-router";

import { LampBadge } from "./HealthBadge";
import { KindChip, KindName } from "./KindMark";
import { KIND_META } from "./kind";
import { type CardRow, cardFacts } from "./objectCard";
import { StatStrip } from "./StatStrip";

/**
 * One object as a card, wherever objects appear as cards: the overview's
 * attention list, a detail page's flow lanes, related objects. The stripe and
 * chip say the kind; the name is the link (when the kind has a page); the
 * `stats` variant adds the kind's three facts.
 */
export function ObjectCard({
  card,
  variant = "summary",
  now = new Date(),
}: {
  card: CardRow;
  variant?: "summary" | "stats";
  now?: Date;
}) {
  const facts = cardFacts(card, now);
  const meta = KIND_META[facts.kind];
  return (
    <article className="object-card has-stripe" data-kind={meta.slug}>
      <header className="object-card__head">
        <KindChip kind={facts.kind} />
        <span className="object-id">
          <KindName kind={facts.kind} />
          {facts.to !== undefined ? (
            <Link className="object-name" to={facts.to}>
              {facts.name}
            </Link>
          ) : (
            <span className="object-name">{facts.name}</span>
          )}
          {facts.namespace !== undefined ? (
            <span className="object-ns">{facts.namespace}</span>
          ) : (
            <span className="object-ns">cluster-scoped</span>
          )}
        </span>
        <LampBadge lamp={facts.lamp} />
      </header>
      <p className="object-card__meta">{facts.meta}</p>
      {variant === "stats" ? (
        <StatStrip label={`${meta.label} ${facts.name}`} variant="card" stats={facts.stats} />
      ) : null}
    </article>
  );
}
