import {
  CircleCheck,
  CircleHelp,
  CirclePause,
  Clock,
  OctagonX,
  TriangleAlert,
  type LucideIcon,
} from "lucide-react";

import type { Health } from "../api/types";
import { unknownVariant } from "../util/assertNever";

/** The `data-health` keys the stylesheet colours; every lamp maps onto one. */
export type HealthKey = "healthy" | "degraded" | "failed" | "suspended" | "pending" | "unknown";

/** A health lamp: colour key, word and icon — always the three together. */
export interface Lamp {
  key: HealthKey;
  word: string;
  icon: LucideIcon;
}

/**
 * The lamp for a health value — exhaustive over the generated union.
 *
 * `Health` is unit-only with a plain `"unknown"` string as its fallback (no
 * `{ unknown: { raw } }` object; see `crates/ui-model/src/lib.rs`), and a newer
 * server may send a string this bundle has never seen: that renders as an
 * unknown lamp carrying the raw word, never as healthy.
 */
export function healthLamp(health: Health): Lamp {
  switch (health) {
    case "healthy":
      return { key: "healthy", word: "Healthy", icon: CircleCheck };
    case "degraded":
      return { key: "degraded", word: "Degraded", icon: TriangleAlert };
    case "failed":
      return { key: "failed", word: "Failed", icon: OctagonX };
    case "suspended":
      return { key: "suspended", word: "Suspended", icon: CirclePause };
    case "pending":
      return { key: "pending", word: "Pending", icon: Clock };
    case "unknown":
      return { key: "unknown", word: "Unknown", icon: CircleHelp };
    default:
      return { key: "unknown", word: unknownVariant(health, "Health"), icon: CircleHelp };
  }
}
