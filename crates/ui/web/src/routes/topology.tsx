import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { Waypoints } from "lucide-react";
import { useMemo } from "react";

import { useGraph } from "../api/hooks";
import { EmptyState } from "../components/EmptyState";
import { ErrorState } from "../components/ErrorState";
import { LoadingState } from "../components/LoadingState";
import { healthLamp } from "../components/health";
import { Drawer } from "../components/topology/Drawer";
import { EdgeLegend } from "../components/topology/EdgeLegend";
import { Graph } from "../components/topology/Graph";
import { useTopologyLayout } from "../components/topology/layout";
import { topologyModel, topologyVerdict } from "../components/topology/model";
import { relativeTime } from "../util/format";
import { useCurrentNamespace } from "../util/namespace";

/**
 * Topology — where the copies of your data are, and where a copy was
 * promised and is not.
 *
 * The board draws repositories, the bare backends behind them, the policies
 * that write into them and the namespaces a cluster repository admits, with
 * one line per relationship: what replicates where, what was seeded from
 * what, who may write, who is admitted. It answers the question the
 * repositories list cannot — *is anything actually copying this?* — and the
 * one that costs the most to learn late: *does this backup destination
 * exist?*
 *
 * Three rules this screen is built on:
 *
 * - A node's colour is never its only signal. Every plate carries the lamp's
 *   icon and its word, and a referenced-but-missing node adds a dashed
 *   outline, a dashed icon and the words "referenced but not found".
 * - The edge legend is always on the page. A dash vocabulary the reader has
 *   to uncover is not a vocabulary.
 * - The layout runs in a worker, and the worker is the only importer of the
 *   engine — so neither the layout nor `elkjs` is on the main thread or in
 *   the chunk every other page pays for.
 *
 * The URL is the state: `?namespace=` scopes the graph (shared with every
 * route) and `?node=` is the open drawer, so a dangling reference can be sent
 * to someone as a link.
 */
export interface TopologySearch {
  node?: string;
}

export const Route = createFileRoute("/topology")({
  validateSearch: (search: Record<string, unknown>): TopologySearch => {
    const node = search.node;
    return typeof node === "string" && node.length > 0 ? { node } : {};
  },
  component: Topology,
});

function Topology() {
  const namespace = useCurrentNamespace();
  const { node: openNode } = Route.useSearch();
  const navigate = useNavigate();
  const graph = useGraph(namespace);

  const model = useMemo(
    () => (graph.data === undefined ? null : topologyModel(graph.data)),
    [graph.data],
  );
  // An empty graph is never handed to the engine: there is nothing to lay
  // out, and the empty state says so better than an empty canvas would.
  const toLayOut = model !== null && model.nodes.length > 0 ? model : null;
  const { layout, problem: layoutFailure, pending: laying } = useTopologyLayout(toLayOut);

  const selected =
    model !== null && openNode !== undefined ? (model.byId.get(openNode) ?? null) : null;
  const open = (node: string | undefined) => {
    const search: TopologySearch & { namespace?: string } = {};
    if (namespace !== undefined) {
      search.namespace = namespace;
    }
    if (node !== undefined) {
      search.node = node;
    }
    // `replace`: inspecting six nodes should not be six steps of history.
    void navigate({ to: "/topology", search, replace: true });
  };

  const board = graph.data !== undefined && graph.data.nodes.length > 0;

  return (
    <div className="page">
      {model !== null ? <Verdict model={model} /> : null}

      <section className="page__section" aria-label="Topology">
        {graph.isPending ? (
          <LoadingState what="the topology" rows={6} />
        ) : graph.isError ? (
          <ErrorState
            problem={graph.error.problem}
            what="the topology"
            onRetry={() => void graph.refetch()}
          />
        ) : graph.data.nodes.length === 0 ? (
          <EmptyState title={`Nothing to draw in ${namespace ?? "any namespace"}`} icon={Waypoints}>
            The board draws repositories, the backends behind them, the policies that write into
            them and the namespaces a cluster repository admits. Create a Repository or
            ClusterRepository and it appears here; a policy naming it draws the first line.
          </EmptyState>
        ) : layoutFailure !== null ? (
          // No retry: the layout is deterministic, so the same graph would
          // fail the same way. The problem's fix names what to do instead.
          <ErrorState problem={layoutFailure} what="the topology" />
        ) : laying || layout === null || model === null ? (
          <LoadingState what="the topology layout" rows={6} />
        ) : (
          <div className="topo-layout" data-drawer={selected !== null ? "open" : undefined}>
            <Graph model={model} layout={layout} selected={selected?.id ?? null} onSelect={open} />
            {selected !== null ? (
              <Drawer
                model={model}
                node={selected}
                namespace={namespace}
                onClose={() => {
                  open(undefined);
                }}
              />
            ) : null}
          </div>
        )}
      </section>

      {board ? (
        <section className="page__section" aria-label="What the lines mean">
          <div className="page__section-head">
            <h2>What the lines mean</h2>
          </div>
          <EdgeLegend />
        </section>
      ) : null}

      <p className="page__prose">
        Every line is directed: it runs from the object that acts to the object it acts on. A plate
        is selectable — its drawer names every relationship at either end of it, the gates holding
        it back, and the section its object is listed in. A plate drawn with a dashed outline is a
        reference to something that is not in the cluster: whatever points at it has been promising
        a copy to nowhere.
      </p>
    </div>
  );
}

/** The one sentence over the board, with the counts it is drawn from. */
function Verdict({ model }: { model: ReturnType<typeof topologyModel> }) {
  const verdict = topologyVerdict(model);
  const lamp = healthLamp(verdict.health);
  const Icon = lamp.icon;
  const facts = [
    `${verdict.facts.repositories} repositories`,
    `${verdict.facts.policies} policies`,
    `${verdict.facts.replications} replications`,
  ];
  return (
    <h2 className="verdict" aria-live="polite">
      <span className="verdict__lamp" data-health={lamp.key}>
        <Icon size={18} strokeWidth={2} aria-hidden="true" />
        <span>{lamp.word}</span>
      </span>
      <span className="verdict__text">{verdict.text}</span>
      <span className="verdict__meta">
        {facts.join(" · ")} · drawn {relativeTime(model.generatedAt)}
      </span>
    </h2>
  );
}
