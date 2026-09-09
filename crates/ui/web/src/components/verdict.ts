import type { DoctorSummary } from "./doctor";
import type { HealthKey } from "./health";

/**
 * The one sentence at the top of the overview: is the data safe?
 *
 * Pure, so the rule can be read and tested in one place. The order of the
 * lamps is the order of concern: a failed repository, a stalled object or a
 * failing doctor check is `failed`; a degraded or unknown repository or a
 * doctor warning is `degraded`; a source that did not load makes the whole
 * verdict `unknown` unless what did load is already worse — a green verdict
 * over a report that never arrived is exactly the lie this screen exists to
 * prevent. An empty scope is `unknown` too: nothing is not healthy.
 *
 * A check the viewer's own RBAC would not let doctor run (`doctor.rbac`) is
 * that same missing data at a finer grain, and it is treated the same way: it
 * never lights the cluster's lamp, it is named in its own words rather than
 * counted as a warning, and on its own it makes the verdict `unknown` — the
 * console genuinely did not verify those checks, and saying "healthy" would
 * claim knowledge it does not have.
 */
export interface VerdictInputs {
  repositories: Record<HealthKey, number>;
  stalled: number;
  doctor: DoctorSummary;
  /** Sources that failed to load, named as the sentence should name them. */
  unavailable: readonly string[];
}

export interface Verdict {
  health: HealthKey;
  text: string;
}

function plural(count: number, one: string, many = `${one}s`): string {
  return `${count} ${count === 1 ? one : many}`;
}

function joinList(parts: readonly string[]): string {
  if (parts.length <= 1) {
    return parts.join("");
  }
  return `${parts.slice(0, -1).join(", ")} and ${parts[parts.length - 1] ?? ""}`;
}

function capitalize(text: string): string {
  return text.charAt(0).toUpperCase() + text.slice(1);
}

export function overviewVerdict({
  repositories,
  stalled,
  doctor,
  unavailable,
}: VerdictInputs): Verdict {
  const lit: string[] = [];
  if (repositories.failed > 0) {
    lit.push(`${plural(repositories.failed, "repository", "repositories")} failed`);
  }
  if (repositories.degraded > 0) {
    lit.push(
      lit.length > 0
        ? `${repositories.degraded} degraded`
        : `${plural(repositories.degraded, "repository", "repositories")} degraded`,
    );
  }
  if (repositories.unknown > 0) {
    lit.push(
      lit.length > 0
        ? `${repositories.unknown} unknown`
        : `${plural(repositories.unknown, "repository", "repositories")} unknown`,
    );
  }
  if (stalled > 0) {
    lit.push(`${plural(stalled, "object")} stalled`);
  }
  if (doctor.fail > 0) {
    lit.push(`${plural(doctor.fail, "doctor check")} failing`);
  }
  if (doctor.warn > 0) {
    lit.push(
      doctor.fail > 0 ? `${doctor.warn} warning` : `${plural(doctor.warn, "doctor check")} warning`,
    );
  }

  const missing =
    unavailable.length > 0 ? ` ${capitalize(joinList(unavailable))} did not load.` : "";
  // Named, never counted: these checks say nothing about the cluster, and
  // the overview keeps warning detail off the screen — so a count with no
  // words beside it would leave the operator no way to learn what it meant.
  const blockedSentence = `${plural(doctor.rbac, "doctor check")} could not run with your permissions.`;
  const blocked = doctor.rbac > 0 ? ` ${blockedSentence}` : "";

  const failed = repositories.failed > 0 || stalled > 0 || doctor.fail > 0;
  if (failed) {
    return { health: "failed", text: `Needs attention: ${lit.join(", ")}.${missing}${blocked}` };
  }
  if (unavailable.length > 0) {
    return {
      health: "unknown",
      text: `Cannot tell: ${joinList(unavailable)} did not load.${blocked}`,
    };
  }
  if (lit.length > 0) {
    return { health: "degraded", text: `Mostly healthy: ${lit.join(", ")}.${blocked}` };
  }
  if (doctor.rbac > 0) {
    return { health: "unknown", text: `Cannot fully check: ${blockedSentence}` };
  }

  const total = Object.values(repositories).reduce((sum, count) => sum + count, 0);
  const tail = "nothing stalled, doctor passes.";
  if (total === 0) {
    return { health: "unknown", text: `No repositories in scope, ${tail}` };
  }
  if (repositories.healthy === total) {
    return {
      health: "healthy",
      text: `All ${plural(total, "repository", "repositories")} healthy, ${tail}`,
    };
  }
  const rest: string[] = [];
  if (repositories.pending > 0) {
    rest.push(`${repositories.pending} pending`);
  }
  if (repositories.suspended > 0) {
    rest.push(`${repositories.suspended} suspended`);
  }
  return {
    health: "healthy",
    text: `${repositories.healthy} of ${plural(total, "repository", "repositories")} healthy (${rest.join(", ")}), ${tail}`,
  };
}
