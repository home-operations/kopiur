#![warn(missing_docs)]
//! `kopiur-ui-model` — the wire contract between the kopiur web UI backend
//! (`kopiur-ui`) and its TypeScript SPA.
//!
//! Nothing in here knows about Kubernetes. The types are plain serde structs and
//! enums that mirror the CRD surface in a shape a browser can consume: timestamps
//! are RFC3339 `String`s, not `k8s_openapi::apimachinery::pkg::apis::meta::v1::Time`,
//! and every phase is a *view* enum with an `Unknown { raw }` fallback so a newer
//! operator writing a phase this build has never heard of degrades to a labelled
//! chip instead of a deserialization error.
//!
//! TypeScript definitions are generated from these very types with [`ts_rs`] — see
//! [`export_all`] — so the SPA's types cannot drift from what the server serializes.
//!
//! # The shape every exported enum takes in TypeScript
//!
//! This section is the contract the SPA's exhaustiveness strategy is built on.
//! It is a **fact about the generated output**, verified against it, not a
//! convention — nothing in this crate enforces it, so read it before writing a
//! `switch`.
//!
//! ts-rs renders an externally-tagged Rust enum as a union of *string literals*
//! (for unit variants) and *single-key objects* (for struct variants), and the
//! two mix freely in one type. A `SnapshotPhaseView` is therefore
//! `string | object`, and a plain `switch (phase)` neither narrows nor covers
//! it. The shape to write is:
//!
//! ```ts
//! if (typeof phase === "string") {
//!   switch (phase) { /* … */ default: assertNever(phase); }
//! } else if ("unknown" in phase) {
//!   render(phase.unknown.raw);
//! }
//! ```
//!
//! **The fallback variant is not uniformly named, and three enums have none.**
//! Writing `case "unknown"` for [`views::EntryKind`], or assuming
//! [`graph::Health`]'s `unknown` is an object, is the mistake this table exists
//! to prevent — and it is the *silent* kind, because an unmatched
//! `{ unknown: { raw } }` falls through a colour map's `default`, which is
//! usually the healthy-green arm.
//!
//! | type | unit variants (string literals) | struct variants (objects) | fallback |
//! |---|---|---|---|
//! | [`identity::IdentitySource`] | `trustedHeaders`, `anonymous` | — | **none** |
//! | [`graph::NodeKind`] | `repository`, `clusterRepository`, `backend`, `policy`, `namespace`, `namespaceSelector` | — | **none** |
//! | [`graph::EdgeKind`] | `snapshotReplication`, `repositoryReplication`, `seed`, `policyMembership`, `allowedNamespace` | — | **none** |
//! | [`graph::Health`] | `healthy`, `degraded`, `failed`, `suspended`, `pending`, `unknown` | — | `"unknown"` — a **plain string**, not an object, and it carries no `raw` |
//! | [`graph::GateSeverityView`] | `warning`, `error` | — | **none** |
//! | [`views::RepositoryPhaseView`] | `pending`, `initializing`, `ready`, `degraded`, `failed` | `{ unknown: { raw: string } }` | `unknown` |
//! | [`views::SnapshotPhaseView`] | `pending`, `running`, `succeeded`, `failed`, `deleting`, `discovered`, `unchanged` | `{ unknown: { raw: string } }` | `unknown` |
//! | [`views::RestorePhaseView`] | `pending`, `resolving`, `restoring`, `completed`, `failed` | `{ unknown: { raw: string } }` | `unknown` |
//! | [`views::ReplicationPhaseView`] | `pending`, `replicating`, `succeeded`, `failed`, `suspended` | `{ unknown: { raw: string } }` | `unknown` |
//! | [`views::OriginView`] | `scheduled`, `manual`, `discovered`, `adopted`, `replicated` | — | **none** — `Origin` is parsed strictly and an unrecognized marker never decodes |
//! | [`views::EntryKind`] | `file`, `dir`, `symlink` | `{ other: { raw: string } }` | **`other`**, not `unknown` |
//! | [`requests::RestoreSourceBody`] | — | `{ snapshotRef: … }`, `{ fromPolicy: … }`, `{ identity: … }` | none (a request body: the client picks the variant) |
//! | [`requests::RestoreTargetBody`] | — | `{ pvcRef: … }`, `{ pvc: … }` | none (as above) |
//!
//! An enum with **no fallback** can still receive a value this build has never
//! heard of, because the server may be newer than the bundle. `assertNever` on
//! such a union is correct as a *compile-time* check and wrong as a runtime
//! assumption: keep the default arm rendering the raw string rather than
//! throwing.
//!
//! # Fields that are stringly-typed on purpose
//!
//! Not every state is an enum here. `DoctorCheckView.outcome`
//! (`Pass`/`Warn`/`Fail`), `ManualRunView.phase`, `RestoreClaimView.phase`,
//! `SeedView.mode`, `ServerView.authMode`, `RepositorySummary.kind` and
//! `RepositorySummary.mode` are `String`, so no exhaustive switch is possible
//! over them and a `default` arm must not assume "OK". `StatusOverview.report`
//! is `unknown`: it is `kopiur_ops`'s own report shape, passed through verbatim,
//! and the consumer narrows it at the point of use.

pub mod graph;
pub mod identity;
pub mod problem;
pub mod requests;
pub mod views;

use std::path::Path;

use ts_rs::{Config, ExportError, TS};

/// Export the TypeScript definition of every wire type into `dir`.
///
/// One `.ts` file per type, named after the type (`Problem.ts`, `SnapshotRow.ts`, …).
/// Each root type is exported together with its transitive dependencies, so listing
/// the roots below is enough to cover the whole contract.
///
/// 64-bit integers are emitted as `number`, not ts-rs's default `bigint`: these types
/// travel as JSON and `JSON.parse` yields a `number`, so a `bigint` annotation would
/// describe a value the SPA never actually receives.
pub fn export_all(dir: &Path) -> Result<(), ExportError> {
    let cfg = Config::new().with_out_dir(dir).with_large_int("number");

    problem::Problem::export_all(&cfg)?;
    identity::Me::export_all(&cfg)?;
    graph::RepositoryGraph::export_all(&cfg)?;
    views::StatusOverview::export_all(&cfg)?;
    views::RepositorySummary::export_all(&cfg)?;
    views::RepositoryDetail::export_all(&cfg)?;
    <views::Page<views::SnapshotRow>>::export_all(&cfg)?;
    views::SnapshotDetail::export_all(&cfg)?;
    views::RetentionPlan::export_all(&cfg)?;
    views::PolicyRow::export_all(&cfg)?;
    views::PolicyDetail::export_all(&cfg)?;
    views::ScheduleRow::export_all(&cfg)?;
    views::RestoreRow::export_all(&cfg)?;
    views::RestoreDetail::export_all(&cfg)?;
    views::MaintenanceRow::export_all(&cfg)?;
    views::RepositoryReplicationRow::export_all(&cfg)?;
    views::SnapshotReplicationRow::export_all(&cfg)?;
    views::ReplicationsView::export_all(&cfg)?;
    views::DoctorReportView::export_all(&cfg)?;
    views::GateDescriptor::export_all(&cfg)?;
    views::EventRow::export_all(&cfg)?;
    views::DirListing::export_all(&cfg)?;
    views::SessionInfo::export_all(&cfg)?;
    views::ActionReceipt::export_all(&cfg)?;

    requests::SnapshotNowBody::export_all(&cfg)?;
    requests::RestoreBody::export_all(&cfg)?;
    requests::SuspendBody::export_all(&cfg)?;
    requests::MaintenanceRunBody::export_all(&cfg)?;
    requests::ReplicationRunBody::export_all(&cfg)?;
    requests::ScanCatalogBody::export_all(&cfg)?;
    requests::SessionCreateBody::export_all(&cfg)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory that removes itself when the test ends.
    struct TempDir(std::path::PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn exports_every_wire_type_as_typescript() {
        let dir =
            TempDir(std::env::temp_dir().join(format!("kopiur-ui-model-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(&dir.0);

        export_all(&dir.0).expect("export_all should write TypeScript definitions");

        for file in ["Problem.ts", "RepositoryGraph.ts", "SnapshotPhaseView.ts"] {
            assert!(
                dir.0.join(file).is_file(),
                "expected {file} in {}",
                dir.0.display()
            );
        }

        let phase = std::fs::read_to_string(dir.0.join("SnapshotPhaseView.ts")).unwrap();
        assert!(
            phase.contains("\"unknown\""),
            "SnapshotPhaseView must expose the camelCase `unknown` fallback variant, got:\n{phase}"
        );

        let source = std::fs::read_to_string(dir.0.join("RestoreSourceBody.ts")).unwrap();
        assert!(
            source.contains("snapshotRef"),
            "RestoreSourceBody must be externally tagged with camelCase variants, got:\n{source}"
        );

        // 64-bit counters must land as `number`: they arrive through
        // `JSON.parse`, which never produces a `bigint`.
        let repo = std::fs::read_to_string(dir.0.join("RepositorySummary.ts")).unwrap();
        assert!(
            repo.contains("snapshotCount?: number | null"),
            "i64 fields must be exported as `number`, got:\n{repo}"
        );
        assert!(
            !repo.contains("bigint"),
            "no wire field may be typed `bigint`, got:\n{repo}"
        );

        // Request bodies carry `#[ts(optional_fields = nullable)]`, so a
        // `#[serde(default)]` option is omittable in TypeScript too.
        let session = std::fs::read_to_string(dir.0.join("SessionCreateBody.ts")).unwrap();
        assert!(
            session.contains("ttlSeconds?:"),
            "request-body options must export as optional keys, got:\n{session}"
        );

        // Exactly one file per wire type. This is the assertion that fails when a
        // root is dropped from `export_all` (or a type stops being reachable from
        // one), which the per-file checks above cannot catch.
        let exported = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".ts"))
            .count();
        assert_eq!(
            exported, 72,
            "expected one .ts file per wire type; add the new type's root to \
             `export_all` and bump this count deliberately. 64 -> 65 when \
             `GateSeverityView` replaced `GateHit.severity`/`GateDescriptor.severity`'s \
             `String`, so one gate registry can no longer be described by two \
             vocabularies; 65 -> 66 when `ReplicationsView` moved here from \
             `kopiur-ui/src/api/replications.rs`, where it was a wire shape the \
             SPA would have had to hand-write; 66 -> 69 for `RestoreDetail` and \
             its two new sub-objects (`RestoreSourceView`, `RestoreTargetView`), \
             so the restore detail route stops answering with the list's row; \
             69 -> 72 for `RetentionPlan` and its `RetentionBucket` / \
             `RetentionCandidate`, so the retention screen can show the whole \
             bucket instead of one snapshot's verdict"
        );
    }
}
