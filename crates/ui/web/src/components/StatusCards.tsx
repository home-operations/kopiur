import { Link } from "@tanstack/react-router";

import type { RepositorySummary } from "../api/types";
import { HealthBadge } from "./HealthBadge";
import { HEALTH_ORDER, type HealthKey, countByHealth, healthLamp } from "./health";

/**
 * The repositories, counted by health: a strip of lettered lamps, not a
 * row of stat tiles.
 *
 * Worst first (`HEALTH_ORDER`), so the eye lands on what is lit; every lamp
 * is present even at zero (dimmed), because "0 failed" is a fact worth
 * reading and a strip whose vocabulary changes with the data cannot be read
 * at a glance. Each lamp links to the repositories list filtered to that
 * health, carrying the namespace scope; the lamp the list is already
 * filtered to links back to the whole list, so the filter is cleared by
 * choosing it again rather than by a separate control. The counts come from the server's
 * own `health` (`countByHealth`), never from a phase-to-health table here.
 */
export interface StatusCardsProps {
  repositories: readonly RepositorySummary[];
  namespace: string | undefined;
  /** The health the list is filtered to, if any. */
  active?: HealthKey | undefined;
}

/** The `?health=` filter the repositories route reads; carried beside the namespace. */
export interface RepositoriesFilter {
  namespace?: string;
  health?: HealthKey;
}

export function StatusCards({ repositories, namespace, active }: StatusCardsProps) {
  const counts = countByHealth(repositories);
  return (
    <nav className="health-strip" aria-label="Repositories by health">
      <ul className="health-strip__list">
        {HEALTH_ORDER.map((key) => {
          const count = counts[key];
          const word = healthLamp(key).word.toLowerCase();
          const noun = count === 1 ? "repository" : "repositories";
          const current = key === active;
          const scope: RepositoriesFilter = namespace !== undefined ? { namespace } : {};
          const search: RepositoriesFilter = current ? scope : { ...scope, health: key };
          const label = `${String(count)} ${word} ${noun}`;
          return (
            <li key={key} data-empty={count === 0 ? "true" : undefined}>
              <Link
                to="/repositories"
                search={search}
                className="health-strip__lamp"
                aria-label={current ? `${label}, shown — select again to show all` : label}
                aria-current={current ? "true" : undefined}
                title={current ? "Show all repositories" : undefined}
                activeOptions={{ exact: true, includeSearch: true }}
              >
                <span className="health-strip__count">{count}</span>
                <HealthBadge health={key} />
              </Link>
            </li>
          );
        })}
      </ul>
    </nav>
  );
}
