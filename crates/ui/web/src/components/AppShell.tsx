import { useRouterState } from "@tanstack/react-router";
import { Menu } from "lucide-react";
import { type ReactNode, useEffect, useRef, useState } from "react";

import { useCurrentNamespace } from "../util/namespace";
import { sectionFor } from "./nav";
import { GlobalProblemBanner } from "./ProblemBanner";
import { Sidebar } from "./shell/Sidebar";

export { ThemeSwitch } from "./shell/ThemeSwitch";

export interface AppShellProps {
  children: ReactNode;
}

/**
 * The console's frame: a sidebar and the page. There is no header bar — the
 * page title (the section and its scope, the one `h1`) opens the content.
 * Below 900px the sidebar is a drawer behind a compact bar.
 */
export function AppShell({ children }: AppShellProps) {
  const pathname = useRouterState({ select: (state) => state.location.pathname });
  const namespace = useCurrentNamespace();
  const section = sectionFor(pathname);
  const href = useRouterState({ select: (state) => state.location.href });
  // The drawer is open on the page it was opened on; navigating closes it.
  const [openOn, setOpenOn] = useState<string | null>(null);
  const navOpen = openOn === href;
  const setNavOpen = (open: boolean) => {
    setOpenOn(open ? href : null);
  };
  const menu = useRef<HTMLButtonElement>(null);
  // Escape closes an open drawer and puts focus back on the button that
  // opened it, so a keyboard user is never left inside a hidden panel.
  useEffect(() => {
    if (!navOpen) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      setOpenOn(null);
      menu.current?.focus();
    };
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("keydown", onKey);
    };
  }, [navOpen]);
  return (
    <div className="shell" data-nav-open={navOpen ? "true" : undefined}>
      <SkipLink />
      <div className="mobile-bar">
        <button
          type="button"
          ref={menu}
          className="mobile-bar__menu"
          aria-controls="sidebar"
          aria-expanded={navOpen}
          onClick={() => {
            setNavOpen(!navOpen);
          }}
        >
          <Menu size={18} strokeWidth={2} aria-hidden="true" />
          <span className="visually-hidden">Navigation</span>
        </button>
        <span className="mobile-bar__brand">kopiur</span>
      </div>
      <Sidebar namespace={namespace} />
      <main className="main" id="main" tabIndex={-1}>
        <GlobalProblemBanner />
        <div className="page-head">
          <h1 className="page-title">
            <span>{section.label}</span>
            <span className="page-title__scope">{namespace ?? "all namespaces"}</span>
          </h1>
        </div>
        <div className="content">{children}</div>
      </main>
    </div>
  );
}

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
