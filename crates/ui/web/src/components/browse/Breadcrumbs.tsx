import { Link } from "@tanstack/react-router";
import { ChevronRight } from "lucide-react";

import { breadcrumbs } from "./browse";

/**
 * Where in the snapshot the listing is, as a trail back to the root.
 *
 * An `ol` inside a `nav`, because the order is the meaning: each crumb is an
 * ancestor of the one after it, and the last one is where you are. Only the
 * last carries `aria-current="page"`, and it is text rather than a link —
 * a link to the page you are on is a control that does nothing.
 *
 * Each crumb drops `?offset=`. Paging is a position inside *one* directory,
 * so carrying it across a navigation would open the parent at an offset that
 * means nothing there — and, on a small parent, at an offset past its end.
 */
export interface BreadcrumbsProps {
  namespace: string;
  name: string;
  /** The normalized path being listed; empty is the snapshot root. */
  path: string;
  /** The root crumb's label — the snapshot's own name reads better than `/`. */
  rootLabel: string;
  /** The console's `?namespace=` scope, carried through every crumb. */
  scope: string | undefined;
}

export function Breadcrumbs({ namespace, name, path, rootLabel, scope }: BreadcrumbsProps) {
  const crumbs = breadcrumbs(path, rootLabel);
  const scoped = scope !== undefined ? { namespace: scope } : {};
  return (
    <nav className="browse-crumbs" aria-label="Path inside the snapshot">
      <ol>
        {crumbs.map((crumb, index) => {
          const last = index === crumbs.length - 1;
          return (
            <li key={crumb.path}>
              {index > 0 ? (
                <ChevronRight
                  className="browse-crumbs__sep"
                  size={13}
                  strokeWidth={2}
                  aria-hidden="true"
                />
              ) : null}
              {last ? (
                <span className="browse-crumbs__here mono" aria-current="page">
                  {crumb.label}
                </span>
              ) : (
                <Link
                  className="mono"
                  to="/snapshots/$namespace/$name/browse"
                  params={{ namespace, name }}
                  search={crumb.path.length > 0 ? { ...scoped, path: crumb.path } : scoped}
                >
                  {crumb.label}
                </Link>
              )}
            </li>
          );
        })}
      </ol>
    </nav>
  );
}
