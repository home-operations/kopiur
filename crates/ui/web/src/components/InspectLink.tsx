import { Link, useRouterState } from "@tanstack/react-router";
import type { ReactNode } from "react";

import { type InspectTarget, inspectToken } from "./inspect";

export interface InspectLinkProps {
  target: InspectTarget;
  className?: string | undefined;
  "aria-label"?: string | undefined;
  "data-kind"?: string | undefined;
  onClick?: (() => void) | undefined;
  children: ReactNode;
}

/**
 * A link that opens a resource in the side panel, on the page you are on.
 *
 * A real link — it has an address, so it opens in a new tab and copies — that
 * keeps every other parameter (the scope, a list's filters and page) and adds
 * `?inspect=`. Opened from the page it pushes one history entry, so Back
 * closes the drawer; opened from inside an open drawer it replaces that entry,
 * so walking from one resource to the next never piles up history.
 */
export function InspectLink({
  target,
  className,
  "aria-label": ariaLabel,
  "data-kind": dataKind,
  onClick,
  children,
}: InspectLinkProps) {
  const location = useRouterState({ select: (s) => s.location });
  const search = location.search as Record<string, unknown>;
  const drawerOpen = typeof search.inspect === "string";
  return (
    <Link
      to={location.pathname}
      search={{ ...search, inspect: inspectToken(target) } as never}
      state={(prev) => ({ ...prev, inspect: true })}
      replace={drawerOpen}
      className={className}
      aria-label={ariaLabel}
      data-kind={dataKind}
      onClick={onClick}
    >
      {children}
    </Link>
  );
}
