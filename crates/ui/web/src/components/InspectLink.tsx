import { Link, useRouterState } from "@tanstack/react-router";
import type { ReactNode } from "react";

import { type InspectTarget, inspectToken } from "./inspect";

export interface InspectLinkProps {
  target: InspectTarget;
  className?: string | undefined;
  "aria-label"?: string | undefined;
  "data-kind"?: string | undefined;
  /** More `data-*` hooks for the anchor (a topology plate's node id, health, …). */
  data?: Readonly<Record<`data-${string}`, string | undefined>> | undefined;
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
 *
 * The page stays where it is: the router scrolls to the top on a navigation
 * by default, and a drawer opened from halfway down a list must not throw the
 * reader back to its head.
 */
export function InspectLink({
  target,
  className,
  "aria-label": ariaLabel,
  "data-kind": dataKind,
  data,
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
      resetScroll={false}
      className={className}
      aria-label={ariaLabel}
      data-kind={dataKind}
      {...data}
      onClick={onClick}
    >
      {children}
    </Link>
  );
}
