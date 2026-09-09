// Barrel for the generated wire types.
//
// HAND-MAINTAINED, and deliberately OUTSIDE `./types/`: `cargo xtask
// gen-ui-types` sweeps stale files from that directory, so a barrel living
// inside it would be deleted on the next regeneration. (ts-rs emits one file
// per type and no `index.ts` of its own.)
//
// Add a wire type to `kopiur-ui-model::export_all` and a line here; the xtask
// test `the_barrel_reexports_every_generated_type` fails if the two sets ever
// disagree, so this file cannot silently fall behind the generator.
//
// Import from here, never from `./types/Foo` directly:
//     import type { SnapshotRow, Page } from "@/api/types";

export type { ActionReceipt } from "./types/ActionReceipt";
export type { Capabilities } from "./types/Capabilities";
export type { CatalogView } from "./types/CatalogView";
export type { ConditionView } from "./types/ConditionView";
export type { DirEntryView } from "./types/DirEntryView";
export type { DirListing } from "./types/DirListing";
export type { DoctorCheckView } from "./types/DoctorCheckView";
export type { DoctorReportView } from "./types/DoctorReportView";
export type { EdgeKind } from "./types/EdgeKind";
export type { EntryKind } from "./types/EntryKind";
export type { EventRow } from "./types/EventRow";
export type { FailureView } from "./types/FailureView";
export type { GateDescriptor } from "./types/GateDescriptor";
export type { GateHit } from "./types/GateHit";
export type { GateSeverityView } from "./types/GateSeverityView";
export type { GraphEdge } from "./types/GraphEdge";
export type { GraphNode } from "./types/GraphNode";
export type { Health } from "./types/Health";
export type { HealthProbeView } from "./types/HealthProbeView";
export type { IdentitySource } from "./types/IdentitySource";
export type { Lineage } from "./types/Lineage";
export type { MaintenanceRow } from "./types/MaintenanceRow";
export type { MaintenanceRunBody } from "./types/MaintenanceRunBody";
export type { ManualRunView } from "./types/ManualRunView";
export type { Me } from "./types/Me";
export type { NodeKind } from "./types/NodeKind";
export type { OriginView } from "./types/OriginView";
export type { Page } from "./types/Page";
export type { PolicyDetail } from "./types/PolicyDetail";
export type { PolicyRef } from "./types/PolicyRef";
export type { PolicyRow } from "./types/PolicyRow";
export type { Problem } from "./types/Problem";
export type { ReplicationPhaseView } from "./types/ReplicationPhaseView";
export type { ReplicationRunBody } from "./types/ReplicationRunBody";
export type { ReplicationsView } from "./types/ReplicationsView";
export type { RepoVerificationView } from "./types/RepoVerificationView";
export type { RepositoryDetail } from "./types/RepositoryDetail";
export type { RepositoryGraph } from "./types/RepositoryGraph";
export type { RepositoryPhaseView } from "./types/RepositoryPhaseView";
export type { RepositoryRefBody } from "./types/RepositoryRefBody";
export type { RepositoryReplicationRow } from "./types/RepositoryReplicationRow";
export type { RepositorySummary } from "./types/RepositorySummary";
export type { RestoreBody } from "./types/RestoreBody";
export type { RestoreClaimView } from "./types/RestoreClaimView";
export type { RestoreDetail } from "./types/RestoreDetail";
export type { RestorePhaseView } from "./types/RestorePhaseView";
export type { RestoreRow } from "./types/RestoreRow";
export type { RestoreSourceBody } from "./types/RestoreSourceBody";
export type { RestoreSourceView } from "./types/RestoreSourceView";
export type { RestoreTargetBody } from "./types/RestoreTargetBody";
export type { RestoreTargetView } from "./types/RestoreTargetView";
export type { RetentionBucket } from "./types/RetentionBucket";
export type { RetentionCandidate } from "./types/RetentionCandidate";
export type { RetentionPlan } from "./types/RetentionPlan";
export type { RetentionPreview } from "./types/RetentionPreview";
export type { RetentionView } from "./types/RetentionView";
export type { RunStatusView } from "./types/RunStatusView";
export type { ScanCatalogBody } from "./types/ScanCatalogBody";
export type { ScheduleRow } from "./types/ScheduleRow";
export type { SeedView } from "./types/SeedView";
export type { ServerView } from "./types/ServerView";
export type { SessionCreateBody } from "./types/SessionCreateBody";
export type { SessionInfo } from "./types/SessionInfo";
export type { SnapshotDetail } from "./types/SnapshotDetail";
export type { SnapshotNowBody } from "./types/SnapshotNowBody";
export type { SnapshotPhaseView } from "./types/SnapshotPhaseView";
export type { SnapshotRefView } from "./types/SnapshotRefView";
export type { SnapshotReplicationRow } from "./types/SnapshotReplicationRow";
export type { SnapshotRow } from "./types/SnapshotRow";
export type { SnapshotStatsView } from "./types/SnapshotStatsView";
export type { StatusOverview } from "./types/StatusOverview";
export type { SuspendBody } from "./types/SuspendBody";
