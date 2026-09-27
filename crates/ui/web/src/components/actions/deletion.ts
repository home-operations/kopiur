/**
 * What deleting a `Snapshot` resource does to the kopia snapshot behind it.
 *
 * The CR owns the kopia object through a finalizer, and `spec.deletionPolicy`
 * decides the object's fate — so this sentence is the whole content of the
 * confirmation. Getting it wrong in either direction is a data-loss bug:
 * telling the owner of a `Retain` snapshot that their backup is about to be
 * destroyed makes them abandon a safe operation, and telling the owner of a
 * `Delete` snapshot that the kopia object survives makes them destroy one.
 *
 * # An absent value must not be filled in
 *
 * `SnapshotRow.deletionPolicy` is optional and the CRD carries no `default:`
 * — deliberately, because the effective policy is context-dependent: a
 * *produced* backup behaves as `Delete` and a *discovered* one is forced to
 * `Retain`, and which applies is the operator's decision at delete time, not
 * something this field mirrors. So an absent value is reported as the
 * unanswered question it is (addenda item 19), never as either answer.
 */

/** What the deletion will do, and whether it destroys data. */
export interface DeletionConsequence {
  /** The `deletionPolicy` value this describes, or `"not set"`. */
  policy: string;
  /** One sentence: what happens to the kopia snapshot. */
  text: string;
  /** True only when the value says the kopia snapshot is destroyed. */
  destructive: boolean;
}

/**
 * The consequence for one `spec.deletionPolicy`.
 *
 * The three the CRD defines are spelled out. Anything else — including an
 * absent value — says that this console cannot tell, rather than guessing:
 * a policy string a newer operator adds is the operator's word, and the fact
 * that we do not know what it means is more useful than a wrong sentence.
 */
export function deletionConsequence(policy: string | null | undefined): DeletionConsequence {
  if (policy === null || policy === undefined || policy.length === 0) {
    return {
      policy: "not set",
      destructive: false,
      text: "This snapshot sets no deletionPolicy, so the operator decides at deletion time: a produced backup is deleted, a discovered one is kept. Check status.origin before confirming.",
    };
  }
  switch (policy) {
    case "Delete":
      return {
        policy,
        destructive: true,
        text: "The finalizer runs kopia snapshot delete, then removes the resource. The backup is destroyed for good.",
      };
    case "Retain":
      return {
        policy,
        destructive: false,
        text: "The resource is removed and the kopia snapshot stays in the repository. The data is not at risk.",
      };
    case "Orphan":
      return {
        policy,
        destructive: false,
        text: "The resource is removed without contacting the repository. The kopia snapshot stays, but kopiur stops tracking it.",
      };
    default:
      return {
        policy,
        destructive: false,
        text: `This console does not recognise deletionPolicy ${policy}, so it cannot say what happens to the kopia snapshot. Check the operator's documentation before confirming.`,
      };
  }
}
