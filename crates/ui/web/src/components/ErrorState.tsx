import { CircleAlert, ShieldOff } from "lucide-react";
import type { ReactNode } from "react";

import { isNotPermitted } from "../api/problem";
import type { Problem } from "../api/types";
import { ActionButton } from "./ActionButton";
import { ProblemBanner } from "./ProblemBanner";

/**
 * Error: the problem's what / why / fix, with fix visually distinct.
 *
 * A 403 renders as the not-permitted state — same three lines, but framed as
 * "you may not", with the fix (which is always an RBAC binding) leading. The
 * state is derived from the problem's status, never from a hard-coded set of
 * types (addenda item 24).
 */
export interface ErrorStateProps {
  problem: Problem;
  /** What was being loaded: "snapshots", "the repository nas". */
  what?: string | undefined;
  onRetry?: (() => void) | undefined;
  /** Extra actions, e.g. a link back to the list. */
  actions?: ReactNode;
}

export function ErrorState({ problem, what, onRetry, actions }: ErrorStateProps) {
  if (isNotPermitted(problem)) {
    return <NotPermittedState problem={problem} what={what} actions={actions} />;
  }
  return (
    <div className="state">
      <h2 className="state__title">
        <CircleAlert size={18} strokeWidth={1.75} aria-hidden="true" />
        <span>{what !== undefined ? `Could not load ${what}` : "Something failed"}</span>
      </h2>
      <ProblemBanner problem={problem} />
      {onRetry !== undefined || actions !== undefined ? (
        <div className="state__actions">
          {onRetry !== undefined ? <ActionButton onClick={onRetry}>Retry</ActionButton> : null}
          {actions}
        </div>
      ) : null}
    </div>
  );
}

export interface NotPermittedStateProps {
  problem: Problem;
  what?: string | undefined;
  actions?: ReactNode;
}

/**
 * Not permitted: the caller's identity may not do this. Nothing to retry —
 * the fix is a binding, and the state says so rather than offering a button
 * that would 403 again.
 */
export function NotPermittedState({ problem, what, actions }: NotPermittedStateProps) {
  return (
    <div className="state" data-state="not-permitted">
      <h2 className="state__title">
        <ShieldOff size={18} strokeWidth={1.75} aria-hidden="true" />
        <span>{what !== undefined ? `Not permitted to view ${what}` : "Not permitted"}</span>
      </h2>
      <p className="state__body">
        kopiur-ui asked the cluster on your behalf and was refused. The console has no access of its
        own: what you can see and do here is exactly what your RBAC bindings allow.
      </p>
      <ProblemBanner problem={problem} />
      {actions !== undefined ? <div className="state__actions">{actions}</div> : null}
    </div>
  );
}
