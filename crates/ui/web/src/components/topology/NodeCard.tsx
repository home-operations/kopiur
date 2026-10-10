import { LampBadge } from "../HealthBadge";
import { inspectToken, useInspect } from "../inspect";
import { InspectLink } from "../InspectLink";
import { KindChip } from "../KindMark";
import { KIND_META } from "../kind";
import { nodeTarget } from "../drawer/graphFacts";
import type { TopologyNode } from "./model";

/**
 * One plate on the board: what the object is, what it is called, and how it
 * is.
 *
 * A plate that is a resource — a repository, a cluster repository, a policy,
 * a ghost of one included — is a link that opens it in the resource drawer,
 * over the board; the board keeps no drawer of its own. A backend, a
 * namespace or a selector is not a resource and has nothing to open: it is a
 * plain plate, and what is known about it is in the drawer of the resource
 * that points at it. The kind and the name are the label strip the rest of
 * the console uses (silkscreen caps + monospace), so a plate reads like a row.
 *
 * Colour never carries the health on its own — the plate's lamp is an icon
 * and a word, and a `missing` node adds a dashed outline and the words
 * "referenced but not found" under a dashed icon, because a dangling
 * reference is a backup destination that does not exist and must be
 * unmistakable at a glance.
 */
export function NodeCard({ node }: { node: TopologyNode }) {
  const target = nodeTarget(node.node);
  const { target: open } = useInspect();
  const body = (
    <>
      <span className="topo-node__id">
        {target !== null ? <KindChip kind={target.kind} size="sm" /> : null}
        <span className="topo-node__text">
          <span className="topo-node__kind">{node.kindWord}</span>
          <span className="topo-node__name">{node.node.label}</span>
        </span>
      </span>
      <span className="topo-node__lamp">
        <LampBadge lamp={node.lamp} />
      </span>
    </>
  );
  const marks = {
    "data-node-id": node.id,
    "data-health": node.lamp.key,
    "data-missing": node.missing ? "true" : undefined,
  };
  if (target === null) {
    return (
      <div
        className="topo-node"
        role="group"
        aria-label={`${node.kindWord} ${node.node.label}`}
        {...marks}
      >
        {body}
      </div>
    );
  }
  const selected = open !== null && inspectToken(open) === inspectToken(target);
  return (
    <InspectLink
      className="topo-node has-stripe"
      data-kind={KIND_META[target.kind].slug}
      target={target}
      data={{ ...marks, "data-selected": selected ? "true" : undefined }}
    >
      {body}
    </InspectLink>
  );
}
