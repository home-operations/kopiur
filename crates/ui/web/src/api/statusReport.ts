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
 * Only what the overview renders is narrowed — in-flight counts, stalled
 * rows, and the two section sizes. Add a field here when a route needs it,
 * and not before: `repositories` and `snapshotReplications` were narrowed
 * and never read, so an unreadable *repository* row flipped `complete` and
 * raised "report incomplete" on a page that displays no repository section.
 * The overview's fleet strip comes from `/repositories`, which carries the
 * typed `Health` this report has no notion of.
 */

/** One object carrying the kstatus `Stalled=True` condition. */
export interface StalledRowView {
  kind: string;
  /** `namespace/name`. */
  object: string;
  /**
   * The condition's message, or `null` when the report did not carry one.
   *
   * `null`, never `""`: an empty string renders as an empty cell and reads
   * as "the operator said nothing", which is a fact this bundle cannot
   * establish. The row itself is kept — a stalled object is real whether or
   * not its message could be read — and `complete` records the gap.
   */
  message: string | null;
}

/** Counts of non-terminal work; `null` when the report did not say. */
export interface InFlightView {
  snapshots: number | null;
  restores: number | null;
}

/** The narrowed report. */
export interface StatusReportView {
  /** How many policies the report lists; `null` when the section is absent. */
  policies: number | null;
  schedules: number | null;
  inFlight: InFlightView;
  stalled: StalledRowView[];
  /**
   * `false` when any section or row the overview renders was absent or
   * unreadable. The route renders what it has and says the report was
   * partial — so this must not follow a section the route does not show.
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

function optionalCount(value: unknown): number | null {
  return typeof value === "number" && Number.isInteger(value) && value >= 0 ? value : null;
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
  return { kind, object, message: optionalString(row.message) };
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
      policies: null,
      schedules: null,
      inFlight: { snapshots: null, restores: null },
      stalled: [],
      complete: false,
    };
  }
  const [stalled, stalledComplete] = rows(root.stalled, stalledRow);
  const inFlightObject = asObject(root.inFlight);
  const inFlight: InFlightView = {
    snapshots: optionalCount(inFlightObject?.snapshots),
    restores: optionalCount(inFlightObject?.restores),
  };
  const policies = Array.isArray(root.policies) ? root.policies.length : null;
  const schedules = Array.isArray(root.schedules) ? root.schedules.length : null;
  const complete =
    stalledComplete &&
    stalled.every((row) => row.message !== null) &&
    inFlight.snapshots !== null &&
    inFlight.restores !== null &&
    policies !== null &&
    schedules !== null;
  return { policies, schedules, inFlight, stalled, complete };
}
