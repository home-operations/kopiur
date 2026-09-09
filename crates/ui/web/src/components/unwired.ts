/**
 * "not reported" — the honest rendering of a field nothing ever writes.
 *
 * Seven status fields are declared on the CRDs and written by no controller.
 * They are recorded in `crates/xtask/wiring-allowlist.yaml`, section (c),
 * whose note says why they are visible at all: the wiring ratchet asks "does
 * anything READ this?", `crates/ui` reads all seven to build its views, so the
 * ratchet now considers them wired — and the other half, that nothing WRITES
 * them, is still open.
 *
 * The consequence for this bundle is that six of them reach a screen it owns
 * and every one of them is always absent. A blank cell reads as "nothing to
 * say"; a `0` reads as a measurement that came back zero; `-` (the ledger's
 * `EMPTY_CELL`, which is right for a value that legitimately has none) reads
 * as "not applicable". None of those is true. The field was never published,
 * and the only honest cell says so in words.
 *
 * This is not a workaround to remove when the controller catches up: the day a
 * controller writes one, the ratchet fails on the stale entry, the value
 * arrives, and this rendering simply stops being reached.
 */

/** The word itself, so a test asserts one string rather than a spelling. */
export const NOT_REPORTED = "not reported";

/**
 * Every unwired field, keyed by the name this bundle refers to it by and
 * valued by the CRD path a reader can check with `kubectl get -o yaml`.
 *
 * All seven are listed, including `snapshotBytesNew`, which no screen in this
 * task renders — the snapshot detail (Task 6) has the same problem with the
 * same answer, and one list is what keeps the two from drifting.
 */
export const UNWIRED_FIELDS = {
  repositoryLastObserved: "Repository.status.storageStats.lastObservedAt",
  maintenanceQuickNextRun: "Maintenance.status.quick.nextScheduledAt",
  maintenanceFullNextRun: "Maintenance.status.full.nextScheduledAt",
  repositoryReplicationNextRun: "RepositoryReplication.status.nextScheduledAt",
  repositoryReplicationBytes: "RepositoryReplication.status.lastReplicatedBytes",
  repositoryReplicationBlobs: "RepositoryReplication.status.lastReplicatedBlobs",
  snapshotBytesNew: "Snapshot.status.stats.bytesNew",
} as const;

/** One of the seven. */
export type UnwiredField = keyof typeof UNWIRED_FIELDS;

/** Why that field is absent, in the words the operator can act on. */
export function unwiredReason(field: UnwiredField): string {
  return `No controller writes ${UNWIRED_FIELDS[field]} today, so the operator has never published a value. This is an absence, not a zero and not a measurement that failed.`;
}
