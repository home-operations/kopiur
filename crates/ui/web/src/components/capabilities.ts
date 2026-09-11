import type { Capabilities, IdentitySource, Me } from "../api/types";
import { unknownVariant } from "../util/assertNever";

/**
 * Every capability flag with a human label, in the order the identity panel
 * lists them. Exhaustive: adding a flag to `Capabilities` fails to compile
 * here until it is named.
 */
export const CAPABILITY_LABELS: { readonly [K in keyof Capabilities]: string } = {
  createSnapshots: "Snapshot now",
  deleteSnapshots: "Delete snapshots",
  createRestores: "Restore",
  patchPolicies: "Suspend / resume policies",
  patchSchedules: "Suspend / resume schedules",
  patchRepositories: "Suspend, scan repositories",
  patchClusterRepositories: "Suspend, scan cluster repositories",
  patchMaintenances: "Run maintenance",
  patchRepositoryReplications: "Run, suspend repository replications",
  patchSnapshotReplications: "Run, suspend snapshot replications",
  createSessionJobs: "Start browse sessions",
  deleteSessionJobs: "Stop browse sessions",
  execSessions: "Read through browse sessions",
};

/** The flags in display order. */
export const CAPABILITY_KEYS = Object.keys(CAPABILITY_LABELS) as readonly (keyof Capabilities)[];

/** The word for how the backend established the identity. */
export function identitySourceLabel(source: IdentitySource): string {
  switch (source) {
    case "trustedHeaders":
      return "via proxy headers";
    case "anonymous":
      return "anonymous";
    default:
      return unknownVariant(source, "IdentitySource");
  }
}

/**
 * The reason a capability-gated control is disabled, or `undefined` when it
 * is allowed. Dialogs pass this straight to `ActionButton.disabledReason`,
 * so an action the user cannot perform is visible, disabled, and explained.
 *
 * Starting a browse session takes two grants (`createSessionJobs` and
 * `execSessions`); pass both and the first refusal is the reason.
 */
export function capabilityReason(
  me: Me | undefined,
  capability: keyof Capabilities | readonly (keyof Capabilities)[],
  namespace: string | undefined,
): string | undefined {
  if (me === undefined) {
    return "Your permissions have not loaded yet.";
  }
  const needed = typeof capability === "string" ? [capability] : capability;
  const missing = needed.find((flag) => !me.can[flag]);
  if (missing === undefined) {
    return undefined;
  }
  const scope = namespace !== undefined ? `in namespace ${namespace}` : "cluster-wide";
  return `${CAPABILITY_LABELS[missing]} is not permitted for ${me.user} ${scope}. Ask a cluster admin to bind kopiur-ui-user or kopiur-ui-editor to your user or group.`;
}
