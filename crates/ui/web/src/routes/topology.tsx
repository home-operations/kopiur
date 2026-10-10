import { createFileRoute, redirect } from "@tanstack/react-router";
import { Waypoints } from "lucide-react";
import { useMemo } from "react";

import { useGraph } from "../api/hooks";
import { EmptyState } from "../components/EmptyState";
import { ErrorState } from "../components/ErrorState";
import { LoadingState } from "../components/LoadingState";
import { healthLamp } from "../components/health";
import { EdgeLegend } from "../components/topology/EdgeLegend";
import { Graph } from "../components/topology/Graph";
import { useTopologyLayout } from "../components/topology/layout";
import { topologyModel, topologyVerdict } from "../components/topology/model";
import { targetFromGraphId } from "../components/drawer/graphFacts";
import { inspectToken } from "../components/inspect";
import { relativeTime } from "../util/format";
import { namespaceFromSearch, useCurrentNamespace } from "../util/namespace";

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
 * route), and a resource plate opens the resource drawer (`?inspect=`), so a
 * dangling reference can be sent to someone as a link. The board once had a
 * drawer of its own on `?node=`; such a link is sent to the resource drawer
 * when it names a resource, and dropped when it does not.
 */
export const Route = createFileRoute("/topology")({
  beforeLoad: ({ search }) => {
    const raw = search as Record<string, unknown>;
    if (typeof raw.node !== "string") return;
    const target = targetFromGraphId(raw.node);
    const scope = namespaceFromSearch(raw);
    redirect({
      throw: true,
      to: "/topology",
      search: {
        ...(scope !== undefined ? { namespace: scope } : {}),
        ...(target !== null ? { inspect: inspectToken(target) } : {}),
      },
      replace: true,
    });
  },
  component: Topology,
});

function Topology() {
  const namespace = useCurrentNamespace();
  const graph = useGraph(namespace);

  const model = useMemo(
    () => (graph.data === undefined ? null : topologyModel(graph.data)),
    [graph.data],
  );
  // An empty graph is never handed to the engine: there is nothing to lay
  // out, and the empty state says so better than an empty canvas would.
  const toLayOut = model !== null && model.nodes.length > 0 ? model : null;
  const { layout, problem: layoutFailure, pending: laying } = useTopologyLayout(toLayOut);

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
            The board shows repositories, their backends, and the policies that write to them.
            Create a Repository or ClusterRepository and it appears here.
          </EmptyState>
        ) : layoutFailure !== null ? (
          // No retry: the layout is deterministic, so the same graph would
          // fail the same way. The problem's fix names what to do instead.
          <ErrorState problem={layoutFailure} what="the topology" />
        ) : laying || layout === null || model === null ? (
          <LoadingState what="the topology layout" rows={6} />
        ) : (
          <div className="topo-layout">
            <Graph model={model} layout={layout} />
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
