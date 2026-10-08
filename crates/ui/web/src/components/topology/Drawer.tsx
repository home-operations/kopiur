import { Link } from "@tanstack/react-router";

import { Finding } from "../Finding";
import { LampBadge } from "../HealthBadge";
import { gateSeverityLamp } from "../gates";
import { healthLamp } from "../health";
import { inspectToken } from "../inspect";
import { SidePanel } from "../SidePanel";
import { nodeMeaning, nodeSection } from "./drawer";
import {
  type TopologyEdge,
  type TopologyModel,
  type TopologyNode,
  nodeObjectKind,
  relationships,
} from "./model";

/**
 * The drawer: everything the board could not fit on a 232×64 plate.
 *
 * What it is (in words, not in CRD shorthand), how it is, what is holding it
 * back, every relationship at either end of it, and the section its object is
 * listed in. A dangling reference and a repository nothing copies are the two
 * findings this screen exists to surface, so both are stated as what / why /
 * fix on the same plate the problem banner uses — not as an adjective on a
 * plate.
 *
 * There is no link to a repository *detail* path here. The URL segment for a
 * repository is the server's `kindPath` (addenda item 16), the graph does not
 * carry it, and inventing one from the display kind is exactly the mapping
 * table that field exists to prevent. So the link goes to the section that
 * lists the object; `drawer.ts` is the one place that changes when the detail
 * routes land. A plate that is a resource also offers "Inspect", which swaps
 * this drawer for the resource drawer — never both at once.
 *
 * It floats in the side panel like every other drawer. The plate that opened
 * it is behind the scrim while it is open, and gets focus back when it closes.
 */
export interface DrawerProps {
  model: TopologyModel;
  node: TopologyNode;
  /** The console's current scope, carried into every link. */
  namespace: string | undefined;
  onClose: () => void;
}

export function Drawer({ model, node, namespace, onClose }: DrawerProps) {
  const { inbound, outbound } = relationships(model, node.id);
  const section = nodeSection(node.node.kind);
  const search = namespace !== undefined ? { namespace } : {};
  const kind = nodeObjectKind(node.node.kind);
  const nodeNamespace = node.node.namespace ?? undefined;
  const inspectable = kind !== null && !node.missing;

  return (
    <SidePanel
      kind={kind ?? undefined}
      kindWord={node.kindWord}
      label={node.node.label}
      onClose={onClose}
      footer={
        section !== null || inspectable ? (
          <>
            {section !== null ? (
              <Link className="button button--quiet" to={section.to} search={search}>
                Find {node.node.name} in {section.section}
              </Link>
            ) : null}
            {inspectable ? (
              <Link
                className="button"
                to="/topology"
                search={{
                  ...search,
                  inspect: inspectToken({ kind, name: node.node.name, namespace: nodeNamespace }),
                }}
                replace
              >
                Inspect
              </Link>
            ) : null}
          </>
        ) : undefined
      }
    >
      <div className="topo-drawer__body">
        {/* What the kind *is* — but not for a ghost: this node is a reference,
            not a repository, and describing what a Repository does would be
            describing something that is not there. The finding below is the
            whole story. */}
        {node.missing ? null : (
          <p className="topo-drawer__meaning">{nodeMeaning(node.node.kind)}</p>
        )}

        <dl className="topo-drawer__facts">
          <dt>State</dt>
          <dd>
            <LampBadge lamp={node.lamp} />
          </dd>
          {node.node.namespace !== undefined && node.node.namespace !== null ? (
            <>
              <dt>Namespace</dt>
              <dd className="mono">{node.node.namespace}</dd>
            </>
          ) : null}
          {node.node.backendKind !== undefined && node.node.backendKind !== null ? (
            <>
              <dt>Backend</dt>
              <dd className="mono">{node.node.backendKind}</dd>
            </>
          ) : null}
          {node.node.allowsAllNamespaces ? (
            <>
              <dt>Admits</dt>
              <dd>every namespace, not a listed set</dd>
            </>
          ) : null}
        </dl>

        {node.missing ? (
          <Finding
            lamp={node.lamp}
            what={`${node.node.name} is referenced here but does not exist.`}
            why="No object with that name is in the cluster, so anything aimed at it has nowhere to land."
            fix="create the repository, or point the objects below at one that exists"
          />
        ) : null}

        {node.replicatesNowhere ? (
          <Finding
            lamp={healthLamp("degraded")}
            what={`Nothing copies ${node.node.name} anywhere.`}
            why="No replication reads from it, so this is the only copy."
            fix="add a SnapshotReplication or RepositoryReplication if it needs a second copy"
          />
        ) : null}

        {node.node.gates.length > 0 ? (
          <ul className="finding-list">
            {node.node.gates.map((gate) => (
              <li key={`${gate.condition}/${gate.reason}`}>
                <Finding
                  title={gate.condition}
                  lamp={gateSeverityLamp(gate.severity)}
                  what={gate.message}
                  meta={<span className="mono">{gate.reason}</span>}
                />
              </li>
            ))}
          </ul>
        ) : null}

        <Relationships
          model={model}
          heading="Points at"
          empty="Nothing on this board leads out of here."
          edges={outbound}
          other={(edge) => edge.to}
        />
        <Relationships
          model={model}
          heading="Pointed at by"
          empty="Nothing on this board leads here."
          edges={inbound}
          other={(edge) => edge.from}
        />
      </div>
    </SidePanel>
  );
}

interface RelationshipsProps {
  model: TopologyModel;
  heading: string;
  empty: string;
  edges: readonly TopologyEdge[];
  /** Which end of the edge is the *other* node. */
  other: (edge: TopologyEdge) => string;
}

function Relationships({ model, heading, empty, edges, other }: RelationshipsProps) {
  return (
    <section className="topo-drawer__rel" aria-label={heading}>
      <h4>{heading}</h4>
      {edges.length === 0 ? (
        <p className="topo-drawer__empty">{empty}</p>
      ) : (
        <ul>
          {edges.map((edge) => {
            const id = other(edge);
            const end = model.byId.get(id);
            return (
              <li key={edge.id} data-edge={edge.style.key}>
                <span className="topo-drawer__rel-kind">{edge.style.word}</span>
                <span className="mono">{end?.node.label ?? id}</span>
                <LampBadge lamp={edge.lamp} />
                {edge.edge.label !== undefined && edge.edge.label !== null ? (
                  <span className="topo-drawer__rel-note mono">{edge.edge.label}</span>
                ) : null}
                {end === undefined ? (
                  <span className="topo-drawer__rel-note">
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
