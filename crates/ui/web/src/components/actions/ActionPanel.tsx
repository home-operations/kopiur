import type { LucideIcon } from "lucide-react";
import { type ReactNode, useState } from "react";

import type { ActionReceipt, Problem } from "../../api/types";
import { ActionButton } from "../ActionButton";
import { ActionResult } from "../ActionResult";

/**
 * The shell every mutating control on this console is built from: a trigger,
 * the question it opens, and the answer the server gave.
 *
 * It is a confirmation *panel*, not a modal. The committed direction's
 * **Action** pattern (DESIGN.md) is a bare `action-bar` of triggers whose
 * first box is the `action__confirm` panel below them; a `<dialog>` here
 * would be a second enclosure over the same content, and the focus-trap pass
 * that a real modal needs is Task 9's. Every dialog in this directory is
 * therefore this shell plus its own fields, which is also why they can be
 * dropped into a ledger row, a detail page or an action bar unchanged.
 *
 * The four rules it exists to keep, so no caller has to remember them:
 *
 * **Never hidden.** A control the caller's RBAC forbids stays visible,
 * focusable and explained (`ActionButton.disabledReason`), because the
 * console is also how an operator learns what their bindings allow.
 *
 * **Two refusals, not one.** `disabledReason` is "you may not" and closes the
 * question entirely; `blockedReason` is "you have not answered yet" and
 * blocks only the confirm button, so a required choice (a snapshot's `pin`, a
 * restore's `overwrite`) cannot be skipped past but the form stays readable.
 *
 * **Asked before it runs.** Opening the trigger shows what the action will
 * do, in the terms the API uses, before anything is sent.
 *
 * **Answered by the server.** The outcome is the `ActionReceipt` — `note`
 * included, which is where an accepted-but-not-performed action explains
 * itself — or the problem's what / why / fix. Never a word of this component's
 * own.
 */
export interface ActionPanelProps {
  /** The action's name: the trigger's words and the result's label. */
  label: string;
  icon?: LucideIcon | undefined;
  /** `danger` for anything that stops backups or destroys data. */
  variant?: "default" | "danger" | undefined;
  /** Disabled with this reason — RBAC, or a state that makes it meaningless. */
  disabledReason?: string | undefined;
  /**
   * A few words naming the refusal, rendered visibly beside the trigger.
   *
   * **Required whenever this panel sits inside a `.ledger-scroll`.** The
   * stylesheet suppresses `ActionButton`'s floating tooltip there — a scroll
   * container clips it, and the hidden one still drags phantom scrollbars
   * onto a table that fits — so without this a disabled control in a ledger
   * is a dead button with no reason a sighted mouse user can reach. The full
   * sentence stays on `aria-describedby` and `title` either way.
   */
  shortReason?: string | undefined;
  /** The confirm button's words, e.g. "Take the snapshot". */
  confirmLabel: string;
  /** Blocks the confirm button only: a required choice not yet made. */
  blockedReason?: string | undefined;
  /** True while the request is in flight. */
  running: boolean;
  onConfirm: () => void;
  receipt?: ActionReceipt | undefined;
  problem?: Problem | undefined;
  /** What the action will do, and the fields it needs. */
  children: ReactNode;
  /**
   * Controlled open state, for a bar that allows one open question at a time.
   * Omit both and the panel manages its own.
   */
  open?: boolean | undefined;
  onOpenChange?: ((open: boolean) => void) | undefined;
}

export function ActionPanel({
  label,
  icon: Icon,
  variant = "default",
  disabledReason,
  shortReason,
  confirmLabel,
  blockedReason,
  running,
  onConfirm,
  receipt,
  problem,
  children,
  open,
  onOpenChange,
}: ActionPanelProps) {
  const [ownOpen, setOwnOpen] = useState(false);
  const controlled = open !== undefined;
  const isOpen = controlled ? open : ownOpen;

  const setOpen = (next: boolean) => {
    if (controlled) {
      onOpenChange?.(next);
      return;
    }
    setOwnOpen(next);
  };

  // In flight beats every other reason: a second click would send a second
  // request, and "you may not" would be the wrong sentence for it.
  const confirmReason = running ? "The request is in flight." : (disabledReason ?? blockedReason);
  const blocked = disabledReason !== undefined && disabledReason.length > 0;

  return (
    <div className="action">
      <ActionButton
        variant={variant}
        disabledReason={disabledReason}
        aria-expanded={isOpen}
        onClick={() => {
          setOpen(!isOpen);
        }}
      >
        {Icon !== undefined ? <Icon size={14} strokeWidth={2} aria-hidden="true" /> : null}
        {label}
      </ActionButton>

      {blocked && shortReason !== undefined ? (
        <span className="action__short">{shortReason}</span>
      ) : null}

      {isOpen ? (
        <div className="action__confirm" role="group" aria-label={label}>
          <div className="action__prose">{children}</div>
          <div className="action__actions">
            <ActionButton
              variant="primary"
              disabledReason={confirmReason}
              onClick={() => {
                onConfirm();
                setOpen(false);
              }}
            >
              {confirmLabel}
            </ActionButton>
            <ActionButton
              variant="quiet"
              onClick={() => {
                setOpen(false);
              }}
            >
              Cancel
            </ActionButton>
          </div>
        </div>
      ) : null}

      <ActionResult label={label} receipt={receipt} problem={problem} />
    </div>
  );
}
