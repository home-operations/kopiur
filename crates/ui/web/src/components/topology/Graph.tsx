import { useId } from "react";

import { HEALTH_ORDER } from "../health";
import { NodeCard } from "./NodeCard";
import type { LaidOut } from "./elk";
import { edgeLabelPoint, polylinePath } from "./geometry";
import type { TopologyModel } from "./model";

/**
 * The board: plates where the layout put them, lines between them.
 *
 * Two layers, and the split is deliberate. The lines are one SVG, because
 * only SVG draws a routed orthogonal polyline with a dash and an end marker.
 * The plates are ordinary HTML buttons in a list on top of it, because a
 * `foreignObject`-free SVG cannot be tabbed to, described, or read out — and
 * this console's whole premise is that an operator can work it without a
 * mouse and without colour vision.
 *
 * The SVG is therefore `aria-hidden`: it is a picture of relationships that
 * are also stated in words. Each plate is a button; each relationship is in
 * the drawer for either of its ends; and the whole edge set is listed for
 * assistive technology beneath the board, so the graph can be read straight
 * through without ever selecting a node.
 *
 * A dangling edge — an endpoint the server did not send a node for — is not
 * drawn, because there is nothing to draw it to. It is in the spoken list and
 * in the drawer, saying exactly that, rather than being quietly dropped.
 */
export interface GraphProps {
  model: TopologyModel;
  layout: LaidOut;
  /** The node whose drawer is open, or `null`. */
  selected: string | null;
  onSelect: (id: string) => void;
}

export function Graph({ model, layout, selected, onSelect }: GraphProps) {
  // Marker ids are document-global; scope them to this instance so two boards
  // on one page cannot steal each other's arrowheads.
  const markers = useId().replaceAll(":", "");

  return (
    <div className="topo-board">
      <div
        className="topo-board__canvas"
        style={{ width: `${layout.width}px`, height: `${layout.height}px` }}
      >
        <svg
          className="topo-board__edges"
          width={layout.width}
          height={layout.height}
          aria-hidden="true"
          focusable="false"
        >
          <defs>
            {HEALTH_ORDER.map((key) => (
              <marker
                id={`${markers}-arrow-${key}`}
                key={`arrow-${key}`}
                viewBox="0 0 10 10"
                refX="9"
                refY="5"
                markerWidth="6"
                markerHeight="6"
                orient="auto-start-reverse"
                style={{ fill: `var(--health-${key}-fg)` }}
              >
                <path d="M0,0 L10,5 L0,10 z" />
              </marker>
            ))}
            {HEALTH_ORDER.map((key) => (
              <marker
                id={`${markers}-square-${key}`}
                key={`square-${key}`}
                viewBox="0 0 10 10"
                refX="8"
                refY="5"
                markerWidth="5"
                markerHeight="5"
                orient="auto"
                style={{ fill: `var(--health-${key}-fg)` }}
              >
                <rect x="1" y="1" width="8" height="8" />
              </marker>
            ))}
          </defs>
          {model.edges.map((edge) => {
            const path = layout.edges.get(edge.id);
            if (path === undefined) {
              return null;
            }
            const marker =
              edge.style.marker === "none"
                ? undefined
                : `url(#${markers}-${edge.style.marker}-${edge.lamp.key})`;
            const at = edgeLabelPoint(path.points, layout.nodes.values());
            const words = [
              edge.edge.label ?? undefined,
              edge.lamp.key === "healthy" ? undefined : edge.lamp.word,
            ].filter((word): word is string => word !== undefined && word.length > 0);
            return (
              <g key={edge.id} data-edge-id={edge.id} data-edge={edge.style.key}>
                <path
                  className="topo-edge"
                  d={polylinePath(path.points)}
                  data-health={edge.lamp.key}
                  fill="none"
                  strokeWidth={edge.style.width}
                  strokeDasharray={edge.style.dash}
                  markerEnd={marker}
                  style={{ stroke: `var(--health-${edge.lamp.key}-fg)` }}
                />
                {at !== null && words.length > 0 ? (
                  <text className="topo-edge__label" x={at.x} y={at.y - 6} textAnchor="middle">
                    {words.join(" · ")}
                  </text>
                ) : null}
              </g>
            );
          })}
        </svg>

        <ul className="topo-board__nodes">
          {model.nodes.map((node) => {
            const box = layout.nodes.get(node.id);
            if (box === undefined) {
              return null;
            }
            return (
              <li
                className="topo-board__node"
                key={node.id}
                style={{ left: `${box.x}px`, top: `${box.y}px`, width: `${box.width}px` }}
              >
                <NodeCard
                  node={node}
                  selected={selected === node.id}
                  onSelect={() => {
                    onSelect(node.id);
                  }}
                />
              </li>
            );
          })}
        </ul>
      </div>

      <div className="visually-hidden">
        <h3>Every relationship on this board</h3>
        <ul>
          {model.edges.map((edge) => (
            <li key={edge.id}>{spokenEdge(model, edge.id)}</li>
          ))}
        </ul>
      </div>
    </div>
  );
}

/** One relationship as a sentence, for the list under the board. */
function spokenEdge(model: TopologyModel, id: string): string {
  const edge = model.edges.find((candidate) => candidate.id === id);
  if (edge === undefined) {
    return "";
  }
  const from = model.byId.get(edge.from)?.node.label ?? edge.from;
  const to = model.byId.get(edge.to)?.node.label ?? edge.to;
  const parts = [`${from} — ${edge.style.word} → ${to}.`, `${edge.lamp.word}.`];
  if (edge.edge.label !== undefined && edge.edge.label !== null && edge.edge.label.length > 0) {
    parts.push(`${edge.edge.label}.`);
  }
  if (edge.dangling) {
    parts.push("One end of this relationship is not on the board, so it is not drawn.");
  }
  return parts.join(" ");
}
