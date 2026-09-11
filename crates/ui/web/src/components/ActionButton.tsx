import { type ButtonHTMLAttributes, type MouseEvent, useId } from "react";

/**
 * A button that can be disabled *with a reason*.
 *
 * Actions the user cannot perform are never hidden — the UI teaches the RBAC
 * model — so a control the caller's `/me` capabilities forbid stays visible,
 * reads as disabled, and says why on hover and on keyboard focus.
 *
 * The contract, for assistive technology as much as for the eye:
 *
 * - `aria-disabled`, not `disabled`, so the control stays focusable and a
 *   keyboard user can reach the reason.
 * - The reason is the control's accessible *description* (`aria-describedby`
 *   → a visually-hidden sibling), so a screen reader announces the action's
 *   name and then why it is unavailable — two facts, not one run-on name.
 * - `data-reason` drives the visible tooltip (`styles.css`, `.button[data-reason]`).
 * - A click while blocked is swallowed; `onClick` never runs.
 *
 * The hidden sibling also carries `button__reason`, which the stylesheet uses
 * to *reveal* it below 900px. There is no hover on a phone, so the tooltip is
 * unreachable there and the reason would simply be invisible; inline, it is
 * read by everyone. Inside a `.ledger-scroll` it stays hidden, because a
 * ledger shows the short word in the cell instead (`actions/reason.ts`).
 */
export interface ActionButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: "default" | "primary" | "quiet" | "danger" | undefined;
  /** When set, the button is disabled and this is shown as the reason. */
  disabledReason?: string | undefined;
}

export function ActionButton({
  variant = "default",
  disabledReason,
  className,
  onClick,
  children,
  ...rest
}: ActionButtonProps) {
  const reasonId = useId();
  const blocked = disabledReason !== undefined && disabledReason.length > 0;
  const classes = ["button", variant === "default" ? "" : `button--${variant}`, className ?? ""]
    .filter((c) => c.length > 0)
    .join(" ");
  const handleClick = (event: MouseEvent<HTMLButtonElement>) => {
    if (blocked) {
      event.preventDefault();
      return;
    }
    onClick?.(event);
  };
  return (
    <>
      <button
        type="button"
        {...rest}
        className={classes}
        aria-disabled={blocked ? "true" : undefined}
        aria-describedby={blocked ? reasonId : rest["aria-describedby"]}
        data-reason={blocked ? disabledReason : undefined}
        onClick={handleClick}
      >
        {children}
      </button>
      {blocked ? (
        <span id={reasonId} className="visually-hidden button__reason">
          {disabledReason}
        </span>
      ) : null}
    </>
  );
}
