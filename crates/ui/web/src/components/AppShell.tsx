import { Link, useRouterState } from "@tanstack/react-router";
import {
  Check,
  CircleHelp,
  KeyRound,
  Monitor,
  Moon,
  Sun,
  UserRound,
  X,
  type LucideIcon,
} from "lucide-react";
import { type ReactNode, useEffect } from "react";

import { useMe } from "../api/hooks";
import { problemBanner } from "../api/problem";
import { useCurrentNamespace } from "../util/namespace";
import { type ThemePreference, useThemePreference } from "../util/theme";
import { CAPABILITY_KEYS, CAPABILITY_LABELS, identitySourceLabel } from "./capabilities";
import { NAV_ITEMS, sectionFor } from "./nav";
import { GlobalProblemBanner } from "./ProblemBanner";

/**
 * The application shell: the rail, the header, the global problem banner,
 * and the content area every route renders into.
 *
 * The rail is the vault's frame — a cool second neutral holding ten nav rows
 * (icon + label) with a single accent bar marking the current one. The
 * header names the page and the scope, then the theme switch and the
 * identity strip: who the backend impersonates, how it learned that, which
 * namespace the answer is for, and a disclosure listing every capability
 * with a check or a cross and a word. Actions the user cannot perform are
 * disabled with the reason (`capabilities.ts`), never hidden, so the
 * interface teaches the RBAC model.
 */
export interface AppShellProps {
  children: ReactNode;
}

export function AppShell({ children }: AppShellProps) {
  const pathname = useRouterState({ select: (state) => state.location.pathname });
  const namespace = useCurrentNamespace();
  const section = sectionFor(pathname);
  return (
    <div className="shell">
      <SkipLink />
      <aside className="rail" aria-label="Sections">
        <Link to="/" className="rail__brand" search={namespace !== undefined ? { namespace } : {}}>
          <VaultMark />
          <span className="rail__wordmark">Kopiur</span>
          <span className="rail__product">console</span>
        </Link>
        <nav className="rail__nav" aria-label="Primary">
          {NAV_ITEMS.map((item) => {
            const Icon = item.icon;
            return (
              <Link
                key={item.to}
                to={item.to}
                search={namespace !== undefined ? { namespace } : {}}
                className="nav-item"
                activeOptions={{ exact: item.to === "/", includeSearch: false }}
                activeProps={{ "aria-current": "page" }}
              >
                <Icon size={16} strokeWidth={1.75} aria-hidden="true" />
                <span>{item.label}</span>
              </Link>
            );
          })}
        </nav>
        <div className="rail__foot">
          <span className="mono">kopiur-ui</span>
        </div>
      </aside>
      <header className="header">
        <h1 className="header__title">
          <span>{section.label}</span>
          <span className="header__scope">{namespace ?? "all namespaces"}</span>
        </h1>
        <div className="header__tools">
          <ThemeSwitch />
          <IdentityStrip namespace={namespace} />
        </div>
      </header>
      <main className="main" id="main" tabIndex={-1}>
        <GlobalProblemBanner />
        <div className="content">{children}</div>
      </main>
    </div>
  );
}

/**
 * The first focusable element on every page. Eleven controls sit ahead of
 * the content (the brand link and ten rail rows), so a keyboard or switch
 * user would otherwise tab through all of them on every navigation
 * (WCAG 2.4.1). Focus is moved explicitly rather than trusting the fragment
 * navigation: browsers differ on whether `#main` receives focus.
 */
function SkipLink() {
  return (
    <a
      href="#main"
      className="skip-link"
      onClick={(event) => {
        const main = document.getElementById("main");
        if (main === null) {
          return;
        }
        event.preventDefault();
        // focus() scrolls the target into view on its own.
        main.focus();
      }}
    >
      Skip to content
    </a>
  );
}

/** The favicon's two reels, inline so it shares the rail's colour. */
function VaultMark() {
  return (
    <svg width="20" height="20" viewBox="0 0 32 32" aria-hidden="true">
      <rect x="2" y="6" width="28" height="20" rx="3" fill="currentColor" opacity="0.14" />
      <circle cx="11" cy="16" r="5.5" fill="none" stroke="currentColor" strokeWidth="2" />
      <circle cx="21" cy="16" r="5.5" fill="none" stroke="currentColor" strokeWidth="2" />
      <circle cx="11" cy="16" r="1.5" fill="currentColor" />
      <circle cx="21" cy="16" r="1.5" fill="currentColor" />
    </svg>
  );
}

const THEME_OPTIONS: readonly { value: ThemePreference; label: string; icon: LucideIcon }[] = [
  { value: "system", label: "System", icon: Monitor },
  { value: "light", label: "Light", icon: Sun },
  { value: "dark", label: "Dark", icon: Moon },
];

/** Follow the OS, or override it — three pressed-state buttons, icon + word. */
export function ThemeSwitch() {
  const [preference, setPreference] = useThemePreference();
  return (
    <div className="theme-switch" role="group" aria-label="Theme">
      {THEME_OPTIONS.map((option) => {
        const Icon = option.icon;
        return (
          <button
            key={option.value}
            type="button"
            className="theme-switch__option"
            // The visible label is hidden below 560px and the icon is
            // aria-hidden, so the name must not depend on either.
            aria-label={option.label}
            aria-pressed={preference === option.value}
            onClick={() => {
              setPreference(option.value);
            }}
          >
            <Icon size={14} strokeWidth={1.75} aria-hidden="true" />
            <span>{option.label}</span>
          </button>
        );
      })}
    </div>
  );
}

interface IdentityStripProps {
  namespace: string | undefined;
}

/**
 * Who the backend impersonates, and what that identity may do in the current
 * namespace. Re-fetched per namespace (addenda item 17). A failed `/me` is
 * reported to the global banner — the page is still usable for reads that
 * succeed, but the operator must know that every control is disabled for a
 * reason that may not be true.
 */
function IdentityStrip({ namespace }: IdentityStripProps) {
  const me = useMe(namespace);
  const failure = me.isError ? me.error : null;
  useEffect(() => {
    if (failure !== null) {
      problemBanner.report(failure.problem, { source: "Identity (/api/v1/me)" });
    }
  }, [failure]);
  if (me.isPending) {
    return (
      <div className="identity" aria-busy="true">
        <span className="identity__summary">
          <span className="skeleton" style={{ width: "7rem" }} aria-hidden="true" />
          <span className="visually-hidden">Loading identity…</span>
        </span>
      </div>
    );
  }
  if (me.isError) {
    return (
      <div className="identity">
        <span className="identity__summary" data-state="error">
          <UserRound size={14} strokeWidth={1.75} aria-hidden="true" />
          <span className="identity__user">identity unavailable</span>
        </span>
      </div>
    );
  }
  const data = me.data;
  const allowed = CAPABILITY_KEYS.filter((key) => data.can[key]).length;
  const scopeWord = namespace !== undefined ? `in ${namespace}` : "cluster-wide";
  return (
    <details className="identity">
      <summary className="identity__summary" aria-label={`Signed in as ${data.user}`}>
        <UserRound size={14} strokeWidth={1.75} aria-hidden="true" />
        <span className="identity__user">{data.user}</span>
        <span className="identity__source">
          <KeyRound size={12} strokeWidth={1.75} aria-hidden="true" />
          <span>{identitySourceLabel(data.source)}</span>
        </span>
      </summary>
      <div className="identity__panel">
        <dl className="identity__grid">
          <dt>User</dt>
          <dd>{data.user}</dd>
          <dt>Groups</dt>
          <dd>{data.groups.length > 0 ? data.groups.join(", ") : "—"}</dd>
          {data.email ? (
            <>
              <dt>Email</dt>
              <dd>{data.email}</dd>
            </>
          ) : null}
          <dt>Source</dt>
          <dd>{identitySourceLabel(data.source)}</dd>
          <dt>Scope</dt>
          <dd>{namespace ?? data.namespace ?? "cluster-wide"}</dd>
        </dl>
        <p className="state__body" style={{ marginTop: "var(--space-3)" }}>
          {allowed} of {CAPABILITY_KEYS.length} actions permitted {scopeWord}. Cluster-repository
          actions are always answered cluster-wide.
        </p>
        <ul className="cap-list" aria-label="Capabilities">
          {CAPABILITY_KEYS.map((key) => {
            const ok = data.can[key];
            return (
              <li key={key} data-allowed={ok ? "true" : "false"}>
                {ok ? (
                  <Check size={14} strokeWidth={2} aria-hidden="true" />
                ) : (
                  <X size={14} strokeWidth={2} aria-hidden="true" />
                )}
                <span>
                  {CAPABILITY_LABELS[key]}
                  <span className="visually-hidden">{ok ? ": permitted" : ": not permitted"}</span>
                </span>
              </li>
            );
          })}
        </ul>
        {data.source === "anonymous" ? (
          <p className="state__body" style={{ marginTop: "var(--space-3)" }}>
            <CircleHelp size={14} strokeWidth={1.75} aria-hidden="true" /> No identity headers
            reached kopiur-ui; every request is made as the anonymous user.
          </p>
        ) : null}
      </div>
    </details>
  );
}
