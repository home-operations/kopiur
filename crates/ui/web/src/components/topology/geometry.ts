/**
 * Turning ELK's polylines into the two things the board draws: a path and a
 * point to hang a label on. Pure, so the board's geometry is a unit test
 * rather than something only a browser can show.
 */

import type { ElkPoint } from "elkjs/lib/elk-api";

/** The `d` of an orthogonal polyline: move to the first point, line to the rest. */
export function polylinePath(points: readonly ElkPoint[]): string {
  const [first, ...rest] = points;
  if (first === undefined) {
    return "";
  }
  const move = `M ${first.x} ${first.y}`;
  return rest.reduce((path, point) => `${path} L ${point.x} ${point.y}`, move);
}

/**
 * Where an edge's label sits: the midpoint of the middle segment.
 *
 * Not the midpoint of the whole run — on an orthogonal route that often lands
 * on a corner, or on the long horizontal that another edge is also using.
 * The middle segment is the one the eye reads as "this line", and its centre
 * is clear of both plates.
 */
export function edgeLabelPoint(points: readonly ElkPoint[]): ElkPoint | null {
  if (points.length < 2) {
    return null;
  }
  const segment = Math.floor((points.length - 1) / 2);
  const from = points[segment];
  const to = points[segment + 1];
  if (from === undefined || to === undefined) {
    return null;
  }
  return { x: (from.x + to.x) / 2, y: (from.y + to.y) / 2 };
}
