import { Network } from "lucide-react";

import { Finding } from "../Finding";
import { LampBadge } from "../HealthBadge";
import { healthLamp } from "../health";
import { ObjectRef } from "../ObjectRef";
import type { TopologyEdge, TopologyModel, TopologyNode } from "../topology/model";
import { DrawerSection } from "./DrawerSection";
import { type GraphContext, nodeTarget } from "./graphFacts";

/**
 * Every relationship the topology graph draws for a resource, both ways: what
 * kind of link it is, what is at the other end, the link's own state (a
 * failing replication is a failed link between two healthy repositories) and
 * its label. A resource at the other end opens in the drawer; a backend, a
 * namespace or a selector is named as what it is.
 */
export function GraphRelationships({ context }: { context: GraphContext }) {
  return (
    <DrawerSection title="Relationships" icon={Network}>
      <EdgeList
        heading="Points at"
        empty="Points at nothing on the board."
        edges={context.outbound}
        other={(edge) => edge.to}
        model={context.model}
        namespace={context.node.node.namespace ?? undefined}
      />
      <EdgeList
        heading="Pointed at by"
        empty="Nothing on the board points here."
        edges={context.inbound}
        other={(edge) => edge.from}
        model={context.model}
        namespace={context.node.node.namespace ?? undefined}
      />
    </DrawerSection>
  );
}

interface EdgeListProps {
  heading: string;
  empty: string;
  edges: readonly TopologyEdge[];
  other: (edge: TopologyEdge) => string;
  model: TopologyModel;
  namespace: string | undefined;
}

export function EdgeList({ heading, empty, edges, other, model, namespace }: EdgeListProps) {
  return (
    <section className="drawer__edges" aria-label={heading}>
      <h4 className="drawer__edges-title">{heading}</h4>
      {edges.length === 0 ? (
        <p className="page__section-note">{empty}</p>
      ) : (
        <ul>
          {edges.map((edge) => {
            const id = other(edge);
            const end = model.byId.get(id);
            return (
              <li key={edge.id} data-edge={edge.style.key}>
                <span className="drawer__edge-kind">{edge.style.word}</span>
                <EdgeEnd end={end} id={id} namespace={namespace} />
                <LampBadge lamp={edge.lamp} />
                {/* A selector's label is the selector itself: said once, as the end. */}
                {edge.edge.label !== undefined &&
                edge.edge.label !== null &&
                edge.edge.label !== end?.node.label ? (
                  <span className="drawer__edge-note mono">{edge.edge.label}</span>
                ) : null}
                {end === undefined ? (
                  <span className="drawer__edge-note">
                    this end is not on the board, so the line is not drawn
                  </span>
                ) : null}
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}

function EdgeEnd({
  end,
  id,
  namespace,
}: {
  end: TopologyNode | undefined;
  id: string;
  namespace: string | undefined;
}) {
  if (end === undefined) return <span className="mono">{id}</span>;
  const target = nodeTarget(end.node);
  if (target === null) {
    return (
      <span className="drawer__edge-end">
        <span className="drawer__edge-endkind">{end.kindWord}</span>{" "}
        <span className="mono">{end.node.label}</span>
      </span>
    );
  }
  return (
    <ObjectRef
      kind={target.kind}
      name={target.name}
      namespace={target.namespace}
      contextNamespace={namespace}
      health={end.lamp}
    />
  );
}

/** A reference to something that does not exist, as what / why / fix. */
export function GhostFinding({ name, fix }: { name: string; fix: string }) {
  return (
    <Finding
      lamp={healthLamp("failed")}
      what={`${name} is referenced here but does not exist.`}
      why="No object with that name is in the cluster, so anything aimed at it has nowhere to land."
      fix={fix}
    />
  );
}

/**
 * What the graph knows that needs a human: a repository nothing copies, and
 * every reference this resource makes to something that does not exist.
 */
export function GraphFindings({ context }: { context: GraphContext }) {
  return (
    <>
      {context.node.replicatesNowhere ? (
        <Finding
          lamp={healthLamp("degraded")}
          what={`Nothing copies ${context.node.node.name} anywhere.`}
          why="No replication reads from it, so this is the only copy."
          fix="add a SnapshotReplication or RepositoryReplication if it needs a second copy"
        />
      ) : null}
      {context.missingRefs.map((ghost) => (
        <GhostFinding
          key={ghost.id}
          name={ghost.node.name}
          fix="create it, or point this at one that exists"
        />
      ))}
    </>
  );
}
