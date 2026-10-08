import { LampBadge } from "../HealthBadge";
import { KindChip } from "../KindMark";
import { KIND_META } from "../kind";
import { type TopologyNode, nodeObjectKind } from "./model";

/**
 * One plate on the board: what the object is, what it is called, and how it
 * is.
 *
 * The plate is a `button`, not a decorated `div`: selecting a node is the
 * board's only interaction and it must be reachable by tab and by Enter, and
 * the drawer it opens is announced through `aria-expanded`. The kind and the
 * name are the label strip the rest of the console uses (silkscreen caps +
 * monospace), so a plate reads like a row.
 *
 * Colour never carries the health on its own — the plate's lamp is an icon
 * and a word, and a `missing` node adds a dashed outline and the words
 * "referenced but not found" under a dashed icon, because a dangling
 * reference is a backup destination that does not exist and must be
 * unmistakable at a glance.
 */
export interface NodeCardProps {
  node: TopologyNode;
  selected: boolean;
  onSelect: () => void;
}

export function NodeCard({ node, selected, onSelect }: NodeCardProps) {
  const kind = nodeObjectKind(node.node.kind);
  return (
    <button
      type="button"
      className={kind !== null ? "topo-node has-stripe" : "topo-node"}
      data-node-id={node.id}
      data-kind={kind !== null ? KIND_META[kind].slug : undefined}
      data-health={node.lamp.key}
      data-missing={node.missing ? "true" : undefined}
      data-selected={selected ? "true" : undefined}
      aria-expanded={selected}
      onClick={onSelect}
    >
      <span className="topo-node__id">
        {kind !== null ? <KindChip kind={kind} size="sm" /> : null}
        <span className="topo-node__text">
          <span className="topo-node__kind">{node.kindWord}</span>
          <span className="topo-node__name">{node.node.label}</span>
        </span>
      </span>
      <span className="topo-node__lamp">
        <LampBadge lamp={node.lamp} />
      </span>
    </button>
  );
}
