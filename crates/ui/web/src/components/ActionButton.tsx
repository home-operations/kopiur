import type { ButtonHTMLAttributes, MouseEvent } from "react";

/**
 * A button that can be disabled *with a reason*.
 *
 * Actions the user cannot perform are never hidden — the UI teaches the RBAC
 * model — so a control the caller's `/me` capabilities forbid stays visible,
 * reads as disabled, and says why on hover and on keyboard focus. `aria-disabled`
 * rather than `disabled` keeps it focusable so a keyboard user gets the reason
 * too; the reason is also part of the accessible name.
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
    <button
      type="button"
      {...rest}
      className={classes}
      aria-disabled={blocked ? "true" : undefined}
      data-reason={blocked ? disabledReason : undefined}
      onClick={handleClick}
    >
      {children}
      {blocked ? <span className="visually-hidden"> — {disabledReason}</span> : null}
    </button>
  );
}
