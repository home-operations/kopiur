import type { Health } from "../api/types";
import { healthLamp } from "./health";

/**
 * A health lamp: colour, icon and word, always together.
 *
 * The colour alone is never the signal — a red/green board is useless to a
 * colour-blind operator at 3am — so every lamp carries an icon and its word.
 * The mapping lives in `health.ts` (`healthLamp`), exhaustive over the
 * generated `Health` union.
 */
export interface HealthBadgeProps {
  health: Health;
  /** Replace the word, e.g. a count: "3 failed". The icon still says which lamp. */
  label?: string | undefined;
}

export function HealthBadge({ health, label }: HealthBadgeProps) {
  const lamp = healthLamp(health);
  const Icon = lamp.icon;
  return (
    <span className="health" data-health={lamp.key}>
      <Icon size={14} strokeWidth={2} aria-hidden="true" />
      <span>{label ?? lamp.word}</span>
    </span>
  );
}
