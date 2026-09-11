/**
 * Reading a `RepositorySummary`: the narrowings, labels and one-sentence
 * verdict the list and the detail both need.
 *
 * Two rules this module exists to keep.
 *
 * **The URL segment is the server's, never ours.** Every link and every
 * action body takes `summary.kindPath`, the field the backend added precisely
 * so the SPA holds no CRD-kind-to-segment table (addenda item 16). `kind` is
 * the CRD spelling for display; the two differ in case and punctuation, and
 * the read route and the session-delete route have disagreed about it before.
 *
 * **The lamp is the server's too.** `RepositorySummary.health` is a typed
 * `Health` the backend computed from phase, suspension and gates
 * (`crates/ui/src/api/graph.rs::repository_health`). Nothing here recomputes
 * it; `filterByHealth` only files rows under the lamp `healthLamp` already
 * gives them, so a health string this bundle has never seen lands under
 * unknown rather than under healthy.
 */

import type { RepositoryPhaseView, RepositorySummary } from "../api/types";
import { unknownVariant } from "../util/assertNever";
import { EMPTY_CELL } from "../util/format";
import { HEALTH_ORDER, type HealthKey, type Lamp, healthLamp } from "./health";

/**
 * Whether a value is one of the six lamps — the guard the repositories route
 * validates `?health=` with.
 *
 * The overview's health strip links here with a typed key, so a value that
 * fails this came from a hand-edited or stale URL. The route keeps it and
 * says so rather than filtering to nothing.
 */
export function isHealthKey(value: unknown): value is HealthKey {
  return typeof value === "string" && (HEALTH_ORDER as readonly string[]).includes(value);
}

/**
 * `status.phase` as a word.
 *
 * Narrowed in the shape `crates/ui-model/src/lib.rs` prescribes (addenda item
 * 11): `typeof === "string"` first, then the single-key object. The object arm
 * is `{ unknown: { raw } }`, so a phase a newer operator writes renders as the
 * operator's own word. An absent phase is the ledger's empty cell: the
 * repository has genuinely not been reconciled, which the health lamp already
 * reports as unknown.
 */
export function repositoryPhaseLabel(phase: RepositoryPhaseView | null | undefined): string {
  if (phase === null || phase === undefined) {
    return EMPTY_CELL;
  }
  if (typeof phase === "string") {
    switch (phase) {
      case "pending":
        return "Pending";
      case "initializing":
        return "Initializing";
      case "ready":
        return "Ready";
      case "degraded":
        return "Degraded";
      case "failed":
        return "Failed";
      default:
        return unknownVariant(phase, "RepositoryPhaseView");
    }
  }
  return phase.unknown.raw;
}

/**
 * How clients reach the repository — deliberately not `mode`.
 *
 * `serverBacked` and `mode` are independent (the summary's own doc says so):
 * a read-only repository can be fronted by a kopia repository server or
 * talked to directly, and a UI that conflated them could not say which.
 */
export function accessLabel(summary: RepositorySummary): string {
  return summary.serverBacked ? "Repository server" : "Direct to backend";
}

/**
 * The token `SuspendBody.kind` and `ScanCatalogBody.kind` take.
 *
 * Both action handlers parse with `RepositoryKindPath::parse`
 * (`crates/ui/src/actions/mod.rs`), the same parser the `{kind}` path routes
 * use — so the row's own `kindPath` is accepted verbatim and no spelling is
 * invented here.
 */
export function suspendKindToken(summary: RepositorySummary): string {
  return summary.kindPath;
}

/**
 * The search a link into this repository's detail carries.
 *
 * A namespaced `Repository` needs its own namespace: the handler answers a
 * 400 `namespace-required` without one, and `/me`'s capability review is
 * namespace-scoped (addenda item 17), so the namespace here is what decides
 * whether the suspend and scan buttons are honestly enabled.
 *
 * A `ClusterRepository` carries none — the object is not in a namespace, and
 * a namespaced review of `patchClusterRepositories` would report a
 * RoleBinding grant that cannot authorize the write. The current page's
 * `?namespace=` scope is deliberately dropped for it rather than passed on.
 */
export function detailSearch(summary: RepositorySummary): { namespace?: string } {
  const namespace = summary.namespace;
  return namespace !== null && namespace !== undefined && namespace.length > 0 ? { namespace } : {};
}

/** The rows whose lamp is `health`; every row when no lamp is asked for. */
export function filterByHealth(
  rows: readonly RepositorySummary[],
  health: HealthKey | undefined,
): RepositorySummary[] {
  if (health === undefined) {
    return [...rows];
  }
  return rows.filter((row) => healthLamp(row.health).key === health);
}

/**
 * Whether this row addresses the cluster-scoped CRD.
 *
 * Keyed on `kindPath` — the server's own canonical routing token, and the
 * exact string the action bodies send as `kind` — rather than on the display
 * `kind` or on the absence of a namespace. One field decides the segment, the
 * body and the capability, so the three cannot disagree.
 */
export function isClusterScoped(summary: RepositorySummary): boolean {
  return summary.kindPath === "cluster-repository";
}

/**
 * The `/me` flag that decides whether suspend and scan-catalog are offered.
 *
 * `patchClusterRepositories` is reviewed cluster-scoped whatever namespace is
 * asked about, because `ClusterRepository` is a cluster-scoped resource — so a
 * `ClusterRepository`'s controls must be judged with no namespace at all
 * (addenda item 17).
 */
export function repositoryPatchCapability(
  summary: RepositorySummary,
): "patchRepositories" | "patchClusterRepositories" {
  return isClusterScoped(summary) ? "patchClusterRepositories" : "patchRepositories";
}

/**
 * The namespace an action body carries for this repository: its own, or none
 * for a `ClusterRepository` — which the handler would drop anyway
 * (`action_namespace` in `crates/ui/src/actions/mod.rs`), and which an empty
 * string would turn into a 400.
 */
export function actionNamespace(summary: RepositorySummary): string | undefined {
  if (isClusterScoped(summary)) {
    return undefined;
  }
  const namespace = summary.namespace;
  return namespace !== null && namespace !== undefined && namespace.length > 0
    ? namespace
    : undefined;
}

/** A lamp and the one sentence beside it. */
export interface RepositoryVerdict {
  lamp: Lamp;
  text: string;
}

/**
 * One sentence answering "what is this repository doing?", for the detail
 * screen's verdict line.
 *
 * The lamp is the server's `health` — never re-derived — and the sentence
 * never asserts more than the row carries: with no phase it says the operator
 * has written none rather than naming one, and a `mode` string this bundle
 * does not know is printed as the server's own word rather than guessed at.
 */
export function repositoryVerdict(summary: RepositorySummary): RepositoryVerdict {
  const lamp = healthLamp(summary.health);
  const phase =
    summary.phase === null || summary.phase === undefined
      ? "the operator has written no phase yet"
      : `phase ${repositoryPhaseLabel(summary.phase)}`;
  const access = summary.serverBacked
    ? "reached through a repository server"
    : "reached directly by every mover";
  const clause = summary.suspended
    ? `it is suspended, so it is taking no new backups (${phase})`
    : `${phase}, ${modeClause(summary.mode)}, ${access}`;
  return { lamp, text: `${lamp.word}: ${clause}.` };
}

/**
 * `spec.mode` in words. The two the CRD has are spelled out; anything else is
 * the server's own string, because a mode this bundle cannot interpret must
 * not be described as either of the two it knows.
 */
function modeClause(mode: string): string {
  if (mode === "ReadWrite") {
    return "read-write";
  }
  if (mode === "ReadOnly") {
    return "read-only, so nothing may be written to it";
  }
  return `mode ${mode}`;
}
