import { Link } from "@tanstack/react-router";
import { Fragment } from "react";

import { useOverview } from "../../api/hooks";
import { KIND_META } from "../kind";
import { NAV_GROUPS } from "../nav";
import { navTally } from "../navTally";
import { useBrowseStatus } from "../browse/useBrowseStatus";
import { BrowseLine } from "./BrowseLine";
import { NavTallyMarks } from "./NavTally";
import { NamespaceSwitcher } from "./NamespaceSwitcher";
import { ObjectSearch } from "./ObjectSearch";
import { UserChip } from "./UserChip";

function BrandMark() {
  return (
    <svg width="16" height="16" viewBox="0 0 32 32" aria-hidden="true">
      <circle cx="11" cy="16" r="5.5" fill="none" stroke="currentColor" strokeWidth="2.5" />
      <circle cx="21" cy="16" r="5.5" fill="none" stroke="currentColor" strokeWidth="2.5" />
    </svg>
  );
}

/**
 * The whole frame: brand, namespace scope, object search, the sections
 * grouped by family, and who you are. There is no header bar.
 *
 * A section that lists a kind carries the fleet's count of it in scope and a
 * bar of their health (`navTally`), from the same overview read the overview
 * page's verdict uses. The section's name stays its link's name; the count in
 * words is the link's description, so the bar is never the only account.
 * Browse, which lists no kind, has a line of its own: whether a browse
 * session is running on the snapshot it is on (`BrowseLine`).
 */
export function Sidebar({ namespace }: { namespace: string | undefined }) {
  const search = namespace !== undefined ? { namespace } : {};
  const overview = useOverview(namespace);
  const browse = useBrowseStatus();
  return (
    <aside id="sidebar" className="sidebar" aria-label="Sections">
      <Link to="/" className="sidebar__brand" search={search}>
        <span className="sidebar__mark">
          <BrandMark />
        </span>
        <span className="sidebar__wordmark">Kopiur</span>
      </Link>
      <NamespaceSwitcher />
      <ObjectSearch />
      <nav className="sidebar__nav" aria-label="Primary">
        {NAV_GROUPS.map((group, index) => (
          <div className="nav-group" key={group.label ?? `group-${String(index)}`}>
            {group.label !== null ? (
              <span className="nav-group__label">{group.label}</span>
            ) : index > 0 ? (
              <hr className="nav-group__rule" />
            ) : null}
            {group.items.map((item) => {
              const Icon = item.icon;
              const tally = navTally(overview.data, item.to);
              const isBrowse = item.to === "/browse";
              const words = isBrowse ? browse.words : tally?.words;
              const describedBy = words !== undefined ? `nav-tally-${item.to.slice(1)}` : undefined;
              const to =
                item.to === "/repositories" && tally !== null && tally.failing > 0
                  ? { health: "failed", ...search }
                  : search;
              return (
                <Fragment key={item.to}>
                  <Link
                    to={item.to}
                    search={to}
                    aria-describedby={describedBy}
                    className="nav-item"
                    activeOptions={{ exact: item.to === "/", includeSearch: false }}
                    activeProps={{ "aria-current": "page" }}
                  >
                    {item.kind !== undefined ? (
                      <span
                        className="kind-chip kind-chip--nav"
                        data-kind={KIND_META[item.kind].slug}
                        aria-hidden="true"
                      >
                        <Icon strokeWidth={2} />
                      </span>
                    ) : (
                      <span className="nav-item__chip" aria-hidden="true">
                        <Icon strokeWidth={2} />
                      </span>
                    )}
                    <span className="nav-item__label">{item.label}</span>
                    {tally !== null && !tally.refused ? <NavTallyMarks tally={tally} /> : null}
                    {isBrowse ? <BrowseLine status={browse} /> : null}
                  </Link>
                  {describedBy !== undefined ? (
                    <span id={describedBy} hidden>
                      {words}
                    </span>
                  ) : null}
                </Fragment>
              );
            })}
          </div>
        ))}
      </nav>
      <UserChip namespace={namespace} />
    </aside>
  );
}
