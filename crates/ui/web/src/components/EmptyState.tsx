import { Archive, type LucideIcon } from "lucide-react";
import type { ReactNode } from "react";

/**
 * Empty: says what would appear here and how to make it appear.
 *
 * "Nothing here" teaches nothing. An empty snapshot list says that snapshots
 * taken under a policy would be listed, and that a schedule or "snapshot now"
 * creates one — the state is the interface's own documentation.
 */
export interface EmptyStateProps {
  /** What would appear here: "No snapshots in prod". */
  title: string;
  /** How it comes to exist: which policy, schedule or action produces it. */
  children?: ReactNode;
  /** An action that creates the thing, when the caller may perform it. */
  action?: ReactNode;
  icon?: LucideIcon | undefined;
}

export function EmptyState({ title, children, action, icon: Icon = Archive }: EmptyStateProps) {
  return (
    <div className="state" role="status">
      <h2 className="state__title">
        <Icon size={18} strokeWidth={1.75} aria-hidden="true" />
        <span>{title}</span>
      </h2>
      {children !== undefined && children !== null ? (
        <div className="state__body">{children}</div>
      ) : null}
      {action !== undefined && action !== null ? (
        <div className="state__actions">{action}</div>
      ) : null}
    </div>
  );
}
