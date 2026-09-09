import type { ReactNode } from "react";

import type { Lamp } from "./health";

/**
 * A finding: what was found, why it matters, and the fix on its own plate.
 *
 * The same three lines a `Problem` carries, for things that are not request
 * errors — a doctor check that failed, a gate that parked an object. The
 * text is the server's own; nothing here paraphrases. The fix plate is the
 * problem banner's plate (same label, same surface), so the operator's eye
 * learns one place to look for the next step.
 *
 * A finding with only `what` is a single sentence and renders as one: a
 * warning's whole story is its one line, and an empty why/fix plate would
 * put words in the check's mouth.
 *
 * `title` also decides the element. Named, it is an `article` a screen-reader
 * user can find and identify. Unnamed — inside a table row that already names
 * the check — it is a plain `div`: an `article` whose `aria-label` is
 * `undefined` is an article-shaped thing with no accessible name, which is
 * noise in the a11y tree rather than structure.
 */
export interface FindingProps {
  /** The check or gate this finding belongs to; omitted when the row already names it. */
  title?: string | undefined;
  what: string;
  why?: string | null | undefined;
  fix?: string | null | undefined;
  /** The lamp whose icon and colour lead the finding; none draws no icon. */
  lamp?: Lamp | undefined;
  /** Where or when: an object name, an age. Rendered faint beneath. */
  meta?: ReactNode;
}

export function Finding({ title, what, why, fix, lamp, meta }: FindingProps) {
  const Icon = lamp?.icon;
  const Wrapper = title !== undefined ? "article" : "div";
  return (
    <Wrapper className="finding" data-health={lamp?.key} aria-label={title}>
      {Icon !== undefined ? (
        <span className="finding__icon">
          <Icon size={16} strokeWidth={2} aria-hidden="true" />
        </span>
      ) : null}
      <div className="finding__body">
        {title !== undefined ? <h3 className="finding__title">{title}</h3> : null}
        <p className="finding__what">{what}</p>
        {why !== undefined && why !== null && why.length > 0 ? (
          <p className="finding__why">{why}</p>
        ) : null}
        {fix !== undefined && fix !== null && fix.length > 0 ? (
          <p className="finding__fix">
            <span className="finding__fix-label">Fix</span>
            <span>{fix}</span>
          </p>
        ) : null}
        {meta !== undefined && meta !== null ? <div className="finding__meta">{meta}</div> : null}
      </div>
    </Wrapper>
  );
}
