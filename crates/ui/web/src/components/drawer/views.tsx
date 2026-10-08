import { Link } from "@tanstack/react-router";
import type { ReactNode } from "react";

import type { Lamp } from "../health";
import { KIND_META } from "../kind";
import { type CardRow, cardFacts } from "../objectCard";
import type { TabSpec } from "../Tabs";
import { Chain, type ChainStep } from "./Chain";
import { type DrawerData, cardOf } from "./drawerData";
import { type DrawerRelation, drawerFacts } from "./drawerFacts";
import { DrawerHead } from "./DrawerHead";
import { FactsList } from "./FactsList";
import { ListActions } from "./ListActions";
import { policyView } from "./policy/policyView";
import { repositoryView } from "./repository/repositoryView";

/**
 * Everything the drawer shows for one resource, in its three bands: the main
 * information at the top, the tabs in the middle, the actions at the bottom.
 */
export interface DrawerView {
  /** The panel title's pill. */
  lamp: Lamp;
  head: ReactNode;
  /** Names the tab list: "About this policy". */
  tabsLabel: string;
  tabs: readonly TabSpec[];
  actions?: ReactNode;
}

/** The view for whatever the drawer read. Exhaustive: a new kind does not compile until it has one. */
export function drawerView(data: DrawerData, now: Date): DrawerView {
  switch (data.kind) {
    case "repository":
    case "clusterRepository":
      return repositoryView(data.kind, data.detail, data, now);
    case "snapshotPolicy":
      return policyView(data.detail, data.repositories, now);
    case "snapshot":
    case "restore": {
      const card = cardOf(data);
      const to = cardFacts(card, now).to;
      return cardView(
        card,
        now,
        to !== undefined ? (
          <Link className="button" to={to}>
            Open full page
          </Link>
        ) : undefined,
      );
    }
    case "snapshotSchedule":
    case "maintenance":
    case "repositoryReplication":
    case "snapshotReplication":
      return cardView(data, now, <ListActions card={data} />);
  }
}

/** Relations on one side of the resource as chain steps, one step per label. */
function steps(related: readonly DrawerRelation[], side: DrawerRelation["side"]): ChainStep[] {
  const out = new Map<string, ChainStep["items"][number][]>();
  for (const rel of related) {
    if (rel.side !== side) continue;
    const items = out.get(rel.label) ?? [];
    items.push({ ref: rel.target });
    out.set(rel.label, items);
  }
  return [...out].map(([label, items]) => ({ label, items }));
}

/** "this policy" without the "this": what the tab list is about. */
export function aboutLabel(kind: CardRow["kind"]): string {
  return `About this ${KIND_META[kind].label.replace(/^Snapshot(?=Policy|Schedule)/, "").toLowerCase()}`;
}

/**
 * A resource told from its card: where it sits, its headline stats, and its
 * facts in one panel.
 */
function cardView(card: CardRow, now: Date, actions: ReactNode): DrawerView {
  const facts = cardFacts(card, now);
  const { facts: list, related } = drawerFacts(card);
  const label = KIND_META[card.kind].label;
  return {
    lamp: facts.lamp,
    head: (
      <DrawerHead
        chain={
          <Chain
            kind={card.kind}
            namespace={facts.namespace}
            before={steps(related, "before")}
            after={steps(related, "after")}
          />
        }
        stats={facts.stats}
        statsLabel={`${label} ${facts.name} at a glance`}
      />
    ),
    tabsLabel: aboutLabel(card.kind),
    tabs: [{ id: "details", label: "Details", render: () => <FactsList facts={list} /> }],
    actions,
  };
}
