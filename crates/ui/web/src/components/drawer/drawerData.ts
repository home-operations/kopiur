import type {
  PolicyDetail,
  PolicyRow,
  ReplicationsView,
  RepositoryDetail,
  RepositorySummary,
  RestoreDetail,
  SnapshotDetail,
} from "../../api/types";
import type { CardRow } from "../objectCard";

/** A kind with no read of its own: the drawer picks its row out of the namespace's list. */
export type ListOnlyCard = Extract<
  CardRow,
  { kind: "snapshotSchedule" | "maintenance" | "repositoryReplication" | "snapshotReplication" }
>;

/**
 * What the drawer has read about one resource. The four kinds with a detail
 * read keep the whole payload — the drawer is their only view — and the rest
 * are the row their list sent.
 */
export type DrawerData =
  | {
      kind: "repository" | "clusterRepository";
      detail: RepositoryDetail;
      /** Rows that give the chain live pills; absent until (or unless) they load. */
      policies?: readonly PolicyRow[] | undefined;
      replications?: ReplicationsView | undefined;
    }
  | {
      kind: "snapshotPolicy";
      detail: PolicyDetail;
      /** Repository rows that give the chain live pills. */
      repositories?: readonly RepositorySummary[] | undefined;
    }
  | { kind: "snapshot"; detail: SnapshotDetail }
  | { kind: "restore"; detail: RestoreDetail }
  | ListOnlyCard;
