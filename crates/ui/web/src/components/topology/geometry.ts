/**
 * Turning ELK's polylines into the two things the board draws: a path and a
 * point to hang a label on. Pure, so the board's geometry is a unit test
 * rather than something only a browser can show.
 */

import type { ElkPoint } from "elkjs/lib/elk-api";

import type { NodeBox } from "./elk";

/** The `d` of an orthogonal polyline: move to the first point, line to the rest. */
export function polylinePath(points: readonly ElkPoint[]): string {
  const [first, ...rest] = points;
  if (first === undefined) {
    return "";
  }
  const move = `M ${first.x} ${first.y}`;
  return rest.reduce((path, point) => `${path} L ${point.x} ${point.y}`, move);
}

/** Room a centred label needs around its anchor before it starts hitting a plate. */
const LABEL_REACH_X = 24;
const LABEL_REACH_Y = 10;

/**
 * Where an edge's label sits: the midpoint of the *longest* segment that is
 * clear of every plate — or nowhere, when no segment is.
 *
 * Two things this is not. It is not the midpoint of the whole run: on an
 * orthogonal route that often lands on a corner. And it is not "wherever the
 * middle happens to be": ELK routes lines behind plates, and a cron
 * expression printed half-under a plate is worse than no cron expression,
 * because the reader cannot tell what they are missing. A label that cannot
 * be read is not drawn, and the same fact is in the drawer and in the spoken
 * edge list either way.
 *
 * The longest clear segment is chosen because on a layered orthogonal route
 * that is the free run between two layers — the piece the eye reads as "this
 * line".
 */
export function edgeLabelPoint(
  points: readonly ElkPoint[],
  plates: Iterable<NodeBox>,
): ElkPoint | null {
  const obstacles = [...plates];
  let best: { at: ElkPoint; length: number } | null = null;
  for (let i = 0; i + 1 < points.length; i += 1) {
    const from = points[i];
    const to = points[i + 1];
    if (from === undefined || to === undefined) {
      continue;
    }
    const at = { x: (from.x + to.x) / 2, y: (from.y + to.y) / 2 };
    if (obstacles.some((plate) => covers(plate, at))) {
      continue;
    }
    const length = Math.abs(to.x - from.x) + Math.abs(to.y - from.y);
    if (best === null || length > best.length) {
      best = { at, length };
    }
  }
  return best?.at ?? null;
}

/** Whether a plate, plus the room a centred label needs, contains the anchor. */
function covers(plate: NodeBox, at: ElkPoint): boolean {
  return (
    at.x >= plate.x - LABEL_REACH_X &&
    at.x <= plate.x + plate.width + LABEL_REACH_X &&
    at.y >= plate.y - LABEL_REACH_Y &&
    at.y <= plate.y + plate.height + LABEL_REACH_Y
  );
}
