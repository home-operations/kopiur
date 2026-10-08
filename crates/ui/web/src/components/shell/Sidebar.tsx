import { Link } from "@tanstack/react-router";

import { KIND_META } from "../kind";
import { NAV_GROUPS } from "../nav";
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
 */
export function Sidebar({ namespace }: { namespace: string | undefined }) {
  const search = namespace !== undefined ? { namespace } : {};
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
              return (
                <Link
                  key={item.to}
                  to={item.to}
                  search={search}
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
                  {item.label}
                </Link>
              );
            })}
          </div>
        ))}
      </nav>
      <UserChip namespace={namespace} />
    </aside>
  );
}
