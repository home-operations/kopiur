import { NOT_REPORTED, type UnwiredField, unwiredReason } from "./unwired";

/**
 * The cell for a value that was never published — see `unwired.ts` for which
 * fields those are and why an absence is rendered as a word.
 */
export interface NotReportedProps {
  /** One of the seven; supplies the reason. */
  field?: UnwiredField | undefined;
  /** A reason of the caller's own, for a value the wire simply does not carry. */
  reason?: string | undefined;
}

/**
 * The cell for a value that was never published.
 *
 * The reason is the element's `title` (the pointer affordance) *and* a
 * visually-hidden sentence beside it, so a screen-reader user gets the
 * explanation a hover would otherwise keep from them — `title` alone is not
 * reliably announced.
 */
export function NotReported({ field, reason }: NotReportedProps) {
  const text = reason ?? (field !== undefined ? unwiredReason(field) : undefined);
  return (
    <>
      <span className="not-reported" title={text}>
        {NOT_REPORTED}
      </span>
      {text !== undefined ? <span className="visually-hidden"> — {text}</span> : null}
    </>
  );
}
