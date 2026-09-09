/**
 * Narrowing for `StatusOverview.report`, the one wire field that is `unknown`
 * by design (addenda item 15).
 *
 * The report is `kopiur_ops::status::StatusReport` serialized verbatim so the
 * CLI and the console can never disagree, which means its shape is the CLI's
 * to change. Nothing here may assume a field exists: every read is a guarded
 * lookup, a row this bundle cannot read is dropped (never patched with a
 * default that looks like a fact), and `complete` says whether anything was
 * dropped so the route can say the report was partial rather than quietly
 * rendering less than the server sent.
 *
 * Only what the overview renders is narrowed — repositories, in-flight counts,
 * stalled rows, and section sizes. Add a field here when a route needs it.
 */

/** One repository line of the report, as the overview reads it. */
export interface RepoRowView {
  kind: string;
  name: string;
  /** Absent for a `ClusterRepository`. */
  namespace: string | null;
  /** `status.phase` as the operator wrote it; `null` when unreported. */
  phase: string | null;
  suspended: boolean | null;
  /** The `Ready` condition's message when the repository is not Ready. */
  problem: string | null;
}

/** One object carrying the kstatus `Stalled=True` condition. */
export interface StalledRowView {
  kind: string;
  /** `namespace/name`. */
  object: string;
  message: string;
}

/** Counts of non-terminal work; `null` when the report did not say. */
export interface InFlightView {
  snapshots: number | null;
  restores: number | null;
}

/** The narrowed report. */
export interface StatusReportView {
  repositories: RepoRowView[];
  /** How many policies the report lists; `null` when the section is absent. */
  policies: number | null;
  schedules: number | null;
  snapshotReplications: number | null;
  inFlight: InFlightView;
  stalled: StalledRowView[];
  /**
   * `false` when any section or row was absent or unreadable. The route
   * renders what it has and says the report was partial.
   */
  complete: boolean;
}

function asObject(value: unknown): Record<string, unknown> | null {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

function optionalString(value: unknown): string | null {
  return typeof value === "string" ? value : null;
}

function optionalBoolean(value: unknown): boolean | null {
  return typeof value === "boolean" ? value : null;
}

function optionalCount(value: unknown): number | null {
  return typeof value === "number" && Number.isInteger(value) && value >= 0 ? value : null;
}

function repoRow(value: unknown): RepoRowView | null {
  const row = asObject(value);
  if (row === null) {
    return null;
  }
  const kind = optionalString(row.kind);
  const name = optionalString(row.name);
  if (kind === null || name === null) {
    return null;
  }
  return {
    kind,
    name,
    namespace: optionalString(row.namespace),
    phase: optionalString(row.phase),
    suspended: optionalBoolean(row.suspended),
    problem: optionalString(row.problem),
  };
}

function stalledRow(value: unknown): StalledRowView | null {
  const row = asObject(value);
  if (row === null) {
    return null;
  }
  const kind = optionalString(row.kind);
  const object = optionalString(row.object);
  if (kind === null || object === null) {
    return null;
  }
  return { kind, object, message: optionalString(row.message) ?? "" };
}

/**
 * Read an array section: the rows that parse, and whether every row did. An
 * absent or non-array section is an empty, incomplete section.
 */
function rows<T>(value: unknown, parse: (item: unknown) => T | null): [T[], boolean] {
  if (!Array.isArray(value)) {
    return [[], false];
  }
  const parsed: T[] = [];
  let complete = true;
  for (const item of value) {
    const row = parse(item);
    if (row === null) {
      complete = false;
    } else {
      parsed.push(row);
    }
  }
  return [parsed, complete];
}

/** Narrow the opaque report. Never throws; see the module doc. */
export function narrowStatusReport(report: unknown): StatusReportView {
  const root = asObject(report);
  if (root === null) {
    return {
      repositories: [],
      policies: null,
      schedules: null,
      snapshotReplications: null,
      inFlight: { snapshots: null, restores: null },
      stalled: [],
      complete: false,
    };
  }
  const [repositories, reposComplete] = rows(root.repositories, repoRow);
  const [stalled, stalledComplete] = rows(root.stalled, stalledRow);
  const inFlightObject = asObject(root.inFlight);
  const inFlight: InFlightView = {
    snapshots: optionalCount(inFlightObject?.snapshots),
    restores: optionalCount(inFlightObject?.restores),
  };
  const policies = Array.isArray(root.policies) ? root.policies.length : null;
  const schedules = Array.isArray(root.schedules) ? root.schedules.length : null;
  const snapshotReplications = Array.isArray(root.snapshotReplications)
    ? root.snapshotReplications.length
    : null;
  const complete =
    reposComplete &&
    stalledComplete &&
    inFlight.snapshots !== null &&
    inFlight.restores !== null &&
    policies !== null &&
    schedules !== null &&
    snapshotReplications !== null;
  return { repositories, policies, schedules, snapshotReplications, inFlight, stalled, complete };
}
