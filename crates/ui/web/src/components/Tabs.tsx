import { type KeyboardEvent, type ReactNode, useId, useRef } from "react";

/** One tab: its id, the word on it, an optional count, and what it shows. */
export interface TabSpec {
  id: string;
  label: string;
  count?: number | undefined;
  render: () => ReactNode;
}

export interface TabsProps {
  /** Names the tab list: "About this repository". */
  label: string;
  tabs: readonly TabSpec[];
  /** The selected tab's id; one this list does not have selects the first. */
  selected: string;
  onSelect: (id: string) => void;
}

/**
 * A row of tabs over one panel.
 *
 * The tab list is a single tab stop: the selected tab is in the tab order,
 * the others are reached with ← and → (wrapping) and Home / End, which select
 * as they move. Only the selected panel is mounted, so a tab whose content
 * reads something reads it when it is opened and not before.
 */
export function Tabs({ label, tabs, selected, onSelect }: TabsProps) {
  const base = useId();
  const buttons = useRef(new Map<string, HTMLButtonElement>());
  const current = tabs.find((tab) => tab.id === selected) ?? tabs[0];
  if (current === undefined) return null;

  const tabId = (id: string) => `${base}-tab-${id}`;
  const panelId = (id: string) => `${base}-panel-${id}`;

  const move = (event: KeyboardEvent<HTMLButtonElement>) => {
    const index = tabs.findIndex((tab) => tab.id === current.id);
    const target =
      event.key === "ArrowRight"
        ? tabs[(index + 1) % tabs.length]
        : event.key === "ArrowLeft"
          ? tabs[(index - 1 + tabs.length) % tabs.length]
          : event.key === "Home"
            ? tabs[0]
            : event.key === "End"
              ? tabs[tabs.length - 1]
              : undefined;
    if (target === undefined) return;
    event.preventDefault();
    onSelect(target.id);
    buttons.current.get(target.id)?.focus();
  };

  return (
    <div className="tabs">
      <div className="tabs__list" role="tablist" aria-label={label}>
        {tabs.map((tab) => {
          const on = tab.id === current.id;
          return (
            <button
              key={tab.id}
              ref={(node) => {
                if (node === null) buttons.current.delete(tab.id);
                else buttons.current.set(tab.id, node);
              }}
              type="button"
              role="tab"
              id={tabId(tab.id)}
              className="tabs__tab"
              aria-selected={on}
              aria-controls={panelId(tab.id)}
              tabIndex={on ? 0 : -1}
              onKeyDown={move}
              onClick={() => {
                onSelect(tab.id);
              }}
            >
              {tab.label}
              {tab.count !== undefined ? (
                <>
                  {" "}
                  <span className="tabs__count">{tab.count}</span>
                </>
              ) : null}
            </button>
          );
        })}
      </div>
      <div
        className="tabs__panel"
        role="tabpanel"
        id={panelId(current.id)}
        aria-labelledby={tabId(current.id)}
        tabIndex={0}
      >
        {current.render()}
      </div>
    </div>
  );
}
