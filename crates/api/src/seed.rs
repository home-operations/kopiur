//! `spec.seed` — initialize a brand-new repository from an existing replica
//! (issue #380).
//!
//! Disaster recovery starts with an empty cluster and a full off-site mirror.
//! Without this block the only way to get the mirror's history back under
//! kopiur's management is to point a `Repository` at the mirror itself (which
//! then becomes the live repository the new cluster writes into) or to copy the
//! blobs by hand. `spec.seed` makes the first bootstrap of an **uninitialized**
//! backend pull the data across first, in one mover Job, before the repository
//! is ever reported `Ready`.
//!
//! Two source shapes, one field:
//!
//! * [`SeedSource::Backend`] — **blob mode**: a bare storage backend holding a
//!   byte-for-byte mirror of a kopia repository (what a
//!   [`RepositoryReplication`](crate::repository_replication) writes). The copy
//!   is `kopia repository sync-to`, so the new repository inherits the mirror's
//!   format and password verbatim — this repository's own
//!   `encryption.passwordSecretRef` must therefore already carry the mirror's
//!   password.
//! * [`SeedSource::Repository`] — **migrate mode**: another `Repository` or
//!   `ClusterRepository` CR, opened read-only. The copy is
//!   `kopia snapshot migrate`, which preserves each snapshot's
//!   `username@hostname:path` identity and times, so seeded history stays
//!   restorable by `identity`/`fromPolicy`. Source and destination are two real
//!   repositories with their own passwords and formats.
//!
//! Not to be confused with the "seed job" fixtures in `deploy/examples` — those
//! are one-shot Jobs that write test data into a volume. This block seeds a
//! **repository**, from another repository.
//!
//! Seeding is armed only while the repository has never been initialized
//! (`status.uniqueId` unset) AND the mover's first connect reports the backend
//! uninitialized. On an already-initialized repository the block is a documented
//! no-op (`Seeded=True`, reason `AlreadyInitialized`), so it is safe to leave
//! standing in a GitOps manifest forever.

use crate::backend::Backend;
use crate::common::{CredentialProjection, FailurePolicy, RepositoryRef};
use crate::snapshot_replication::PolicyCopyMode;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Default `activeDeadlineSeconds` for a bootstrap Job that is **seeding** — 24
/// hours, versus the 120s a routine connect gets.
///
/// A seed copies a whole repository over the network exactly once; the ordinary
/// bootstrap deadline exists to fail a wedged *connect* fast and is orders of
/// magnitude too short for it. Overridable per repository via
/// `spec.seed.failurePolicy.activeDeadlineSeconds` (see
/// [`seed_active_deadline_seconds`]). Part of the documented API contract, so it
/// lives beside the field rather than in the controller.
pub const DEFAULT_SEED_BOOTSTRAP_DEADLINE_SECS: i64 = 86_400;

/// Initialize this repository from an existing replica on its first bootstrap.
///
/// Every knob below is a sub-object so future tuning slots in without an API
/// break. The `sync`/`migrate` blocks are **mode-specific** and admission
/// rejects the mismatched pairing (`sync` with a repository source, `migrate`
/// with a backend source) rather than silently ignoring one — no field in
/// kopiur is inert.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SeedSpec {
    /// Where the seed data comes from: exactly one of a bare storage backend
    /// (blob mode) or another repository CR (migrate mode).
    pub from: SeedSource,
    /// Tuning for the `kopia repository sync-to` blob copy. **Blob mode only**
    /// (`from.backend`); rejected at admission alongside `from.repository`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<SeedSyncOptions>,
    /// Tuning for the `kopia snapshot migrate` copy. **Migrate mode only**
    /// (`from.repository`); rejected at admission alongside `from.backend`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migrate: Option<SeedMigrateOptions>,
    /// Accept a source that holds zero snapshots (default `false`).
    ///
    /// A mirror that answers but is empty is almost always a mis-pointed
    /// bucket/prefix, and silently seeding nothing would hand you a `Ready`
    /// repository with no history — the failure mode #380 is about. By default
    /// the bootstrap fails loudly and retries; set this when an empty source is
    /// genuinely expected (e.g. re-homing a repository whose history was
    /// deliberately pruned to nothing).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_empty_source: bool,
    /// Deadline/backoff for the **seeding** bootstrap Job. An absent
    /// `activeDeadlineSeconds` means **24h** here, rather than the 120s a
    /// routine connect gets — a seed copies a whole repository, once. Only
    /// applied while the seed is armed; later connects to the now-initialized
    /// repository use `spec.bootstrap.failurePolicy` as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_policy: Option<FailurePolicy>,
    /// Opt-in projection of the SOURCE repository's credential Secrets into the
    /// seeding mover Job's namespace. **Migrate mode only in practice** — a
    /// blob-mode source's credentials must already be in the namespace the
    /// bootstrap Job runs in (this CR's own namespace for a `Repository`; for a
    /// `ClusterRepository` the operator's namespace, unless
    /// `encryption.passwordSecretRef.namespace` pins another, in which case the
    /// Job runs there). Requires the operator's `features.credentialProjection`
    /// install flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_projection: Option<CredentialProjection>,
}

/// Where a seed reads from — exactly one of, externally tagged
/// (`from: { backend: { s3: {...} } }` or `from: { repository: {...} }`).
///
/// The two variants are genuinely different operations, not two spellings of
/// one: blob mode copies raw storage and inherits the source's format and
/// password; migrate mode copies snapshot manifests between two independently
/// encrypted repositories. Making them one enum is what forces every handler to
/// answer for both.
///
/// ```
/// use kopiur_api::seed::SeedSource;
///
/// // Blob mode: the wire form is `{ "backend": { "s3": { ... } } }`.
/// let blob: SeedSource = serde_json::from_value(serde_json::json!({
///     "backend": { "s3": { "bucket": "offsite-mirror" } }
/// }))
/// .unwrap();
/// assert_eq!(blob.mode(), kopiur_api::seed::SeedMode::Blob);
/// assert_eq!(blob.describe(), "S3");
///
/// // Migrate mode: another repository CR.
/// let migrate: SeedSource = serde_json::from_value(serde_json::json!({
///     "repository": { "kind": "ClusterRepository", "name": "offsite" }
/// }))
/// .unwrap();
/// assert_eq!(migrate.mode(), kopiur_api::seed::SeedMode::Migrate);
/// assert_eq!(migrate.describe(), "ClusterRepository/offsite");
/// ```
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum SeedSource {
    /// A bare storage backend holding a byte-for-byte mirror of a kopia
    /// repository (blob mode, `kopia repository sync-to`). The mirror's format
    /// and encryption password are inherited verbatim, so this repository's
    /// `encryption.passwordSecretRef` must already hold the MIRROR's password
    /// and `spec.create`'s format knobs are refused as inert.
    // Boxed because a `Backend` is far larger than a `RepositoryRef`; an
    // unboxed variant would inflate every `SeedSource` — and every `SeedSpec`
    // and repository spec embedding one — to the larger size.
    Backend(Box<Backend>),
    /// Another `Repository`/`ClusterRepository`, opened read-only (migrate mode,
    /// `kopia snapshot migrate`). Snapshot identities and times are preserved,
    /// so seeded history is restorable by `identity`/`fromPolicy`.
    ///
    /// A `kind: Repository` reference with no `namespace` resolves in the
    /// referrer's own namespace — and, on a cluster-scoped `ClusterRepository`
    /// (which has none), in the operator's namespace, the same rule its
    /// credential `secretRef`s follow. Set `namespace` explicitly whenever the
    /// source lives anywhere else.
    Repository(RepositoryRef),
}

impl SeedSource {
    /// Which copy mechanism this source selects. Exhaustive, so a new variant
    /// cannot compile until its mode is decided.
    pub fn mode(&self) -> SeedMode {
        match self {
            SeedSource::Backend(_) => SeedMode::Blob,
            SeedSource::Repository(_) => SeedMode::Migrate,
        }
    }

    /// The rendering pinned into `status.seed.source`: the
    /// [`Backend::kind_str`] discriminant (`S3`, `Filesystem`, …) for blob
    /// mode, `Kind/name` (with `namespace/` when the reference sets one) for
    /// migrate mode.
    ///
    /// Lives here so the controller, the CLI and any diagnostic describe a seed
    /// source identically instead of each re-deriving a string.
    ///
    /// ```
    /// use kopiur_api::seed::SeedSource;
    ///
    /// let cross_ns: SeedSource = serde_json::from_value(serde_json::json!({
    ///     "repository": { "name": "nas", "namespace": "backups" }
    /// }))
    /// .unwrap();
    /// assert_eq!(cross_ns.describe(), "Repository/backups/nas");
    /// ```
    pub fn describe(&self) -> String {
        match self {
            SeedSource::Backend(b) => b.kind_str().to_string(),
            SeedSource::Repository(r) => match r.namespace.as_deref() {
                Some(ns) if !ns.is_empty() => {
                    format!("{}/{ns}/{}", r.kind.kind_str(), r.name)
                }
                _ => format!("{}/{}", r.kind.kind_str(), r.name),
            },
        }
    }
}

/// Which copy mechanism a seed ran. Mirrors the `SeedSource` variant, named
/// after the operation rather than the input so status, metrics
/// (`kopiur_repository_seed_total{mode}`) and docs share one vocabulary.
// The wire strings are pinned by `tests::seed_mode_labels_are_stable` rather
// than a doctest: schemars lifts a referenced enum's doc comment into the field
// description, so a code block here would land in `docs/field-reference.md`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum SeedMode {
    /// `kopia repository sync-to` from a bare mirror backend.
    Blob,
    /// `kopia snapshot migrate` from another repository CR.
    Migrate,
}

impl SeedMode {
    /// Stable lowercase label for metrics/log fields. Exhaustive.
    pub fn as_str(self) -> &'static str {
        match self {
            SeedMode::Blob => "blob",
            SeedMode::Migrate => "migrate",
        }
    }
}

/// Blob-mode tuning for `kopia repository sync-to`.
///
/// Deliberately a strict subset of
/// [`SyncOptions`](crate::repository_replication::SyncOptions): a seed writes
/// into a repository that does not exist yet, so `deleteExtra` has nothing to
/// prune, `mustExist` must be `false` (initializing the destination layout is
/// the point), and `times`/`update` have no prior copy to compare against.
/// Offering them would be offering inert fields.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SeedSyncOptions {
    /// `--parallel`: concurrent blob-copy workers (kopia default `1` —
    /// sequential). Raise it: a first-time seed of a large repository over a
    /// WAN is exactly the workload sequential copying is worst at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<u32>,
    /// `--max-download-speed`: cap read throughput from the seed source, in
    /// bytes/sec (kopia default: unlimited).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_download_speed_bytes_per_second: Option<i64>,
    /// `--max-upload-speed`: cap write throughput into this repository, in
    /// bytes/sec (kopia default: unlimited).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_upload_speed_bytes_per_second: Option<i64>,
}

/// Migrate-mode tuning for `kopia snapshot migrate`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SeedMigrateOptions {
    /// `--parallel`: snapshots migrated concurrently (kopia default `1` —
    /// sequential). Must be >= 1 when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<u32>,
    /// Copy only each source identity's most recent snapshot instead of its full
    /// history (`kopia snapshot migrate --latest`). Default `false` — a seed
    /// exists to recover history, so the full copy is the sane default; set this
    /// when you only need the latest restore point back quickly.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub latest_only: bool,
    /// Whether the source's kopia **policies** are copied along with the
    /// snapshots. Defaults to `PolicyCopyMode::None` (an explicit
    /// `--no-policies`), not kopia's own copy-by-default: retention in a
    /// kopiur-managed repository is driven by `Snapshot` CRs, and importing the
    /// source's kopia-side policies could delete manifests behind the
    /// operator's back.
    #[serde(default)]
    pub policies: PolicyCopyMode,
}

/// What the last seed attempt did, pinned on `Repository`/`ClusterRepository`
/// `status.seed`. Absent on a repository that was never seeded.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SeedStatus {
    /// RFC 3339 timestamp the operator LAUNCHED a seeding bootstrap Job for
    /// this repository — the durable **seed-attempt marker**.
    ///
    /// Stamped before the Job is created, and never cleared. Its whole job is
    /// to distinguish "a seed this operator started did not finish" from "this
    /// backend was initialized by somebody else": the first must resume the
    /// copy, the second must keep the no-clobber `AlreadyInitialized` path.
    /// See `seed_resume` — the marker is the ONLY input the resume decision is
    /// allowed to take, because a resuming migrate writes into whatever
    /// repository is at the backend and then re-stamps its maintenance owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// RFC 3339 timestamp the seed completed. Set once: a repository is seeded
    /// exactly once, at its first bootstrap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seeded_at: Option<String>,
    /// Which copy mechanism ran: `blob` (a `kopia repository sync-to` from a
    /// mirror backend) or `migrate` (a `kopia snapshot migrate` from another
    /// repository CR).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<SeedMode>,
    /// The source the data came from, rendered by `SeedSource::describe` —
    /// the backend discriminant (`S3`, `Filesystem`, …) for blob mode,
    /// `Kind/name` for migrate mode. Never a credential or a bucket path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Snapshots observed at the SOURCE when the seed ran. Zero is only ever
    /// recorded when `allowEmptySource` permitted it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_count: Option<i64>,
    /// Snapshots actually copied into this repository. Migrate mode only — a
    /// blob copy moves storage, not manifests, so there is no per-snapshot copy
    /// count to report and it leaves this unset; its `snapshotCount` is the
    /// listing taken at the SOURCE before the copy, which for a byte-for-byte
    /// mirror is also what this repository ends up holding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshots_copied: Option<i64>,
}

/// The `activeDeadlineSeconds` a **seeding** bootstrap Job gets: the
/// repository's own `spec.seed.failurePolicy.activeDeadlineSeconds`, else
/// [`DEFAULT_SEED_BOOTSTRAP_DEADLINE_SECS`].
///
/// Pure + shared so the controller (which builds the Job) and any diagnostic
/// explaining a long-running seed agree on the number.
///
/// ```
/// use kopiur_api::seed::{DEFAULT_SEED_BOOTSTRAP_DEADLINE_SECS, seed_active_deadline_seconds};
/// # use kopiur_api::common::FailurePolicy;
/// # use kopiur_api::seed::{SeedSpec, SeedSource};
/// # let from: SeedSource = serde_json::from_value(serde_json::json!({
/// #     "repository": { "name": "offsite" }
/// # })).unwrap();
/// let mut seed = SeedSpec {
///     from,
///     sync: None,
///     migrate: None,
///     allow_empty_source: false,
///     failure_policy: None,
///     credential_projection: None,
/// };
/// assert_eq!(seed_active_deadline_seconds(&seed), DEFAULT_SEED_BOOTSTRAP_DEADLINE_SECS);
///
/// seed.failure_policy = Some(FailurePolicy {
///     backoff_limit: None,
///     active_deadline_seconds: Some(3600),
///     pod_startup_deadline_seconds: None,
/// });
/// assert_eq!(seed_active_deadline_seconds(&seed), 3600);
/// ```
pub fn seed_active_deadline_seconds(seed: &SeedSpec) -> i64 {
    seed.failure_policy
        .as_ref()
        .and_then(|fp| fp.active_deadline_seconds)
        .unwrap_or(DEFAULT_SEED_BOOTSTRAP_DEADLINE_SECS)
}

/// The `RepositoryRef` a migrate-mode seed reads from, or `None` for blob mode.
/// Exhaustive helper so callers that only care about the CR-reference case
/// (referent watches, tenancy gates, credential resolution) do not each write
/// their own `match`.
pub fn seed_repository_ref(seed: &SeedSpec) -> Option<&RepositoryRef> {
    match &seed.from {
        SeedSource::Repository(r) => Some(r),
        SeedSource::Backend(_) => None,
    }
}

/// The `Backend` a blob-mode seed reads from, or `None` for migrate mode.
/// Exhaustive counterpart to [`seed_repository_ref`].
pub fn seed_backend(seed: &SeedSpec) -> Option<&Backend> {
    match &seed.from {
        SeedSource::Backend(b) => Some(b),
        SeedSource::Repository(_) => None,
    }
}

/// Whether a repository whose `status.uniqueId` is `unique_id` should arm its
/// seed (D4): a seed runs only on a repository that has never been initialized.
/// Once the bootstrap pins a unique ID the block is a standing no-op.
pub fn seed_armed(seed: Option<&SeedSpec>, unique_id: Option<&str>) -> bool {
    seed.is_some() && unique_id.is_none_or(str::is_empty)
}

/// Whether an armed seed must **RESUME** an attempt a previous bootstrap
/// started but did not finish, rather than run as a first seed.
///
/// `armed` is [`seed_armed`]; `status` is the repository's `status.seed`. The
/// answer is `true` exactly when the seed is armed, the durable seed-attempt
/// marker ([`SeedStatus::started_at`]) is present, and
/// [`SeedStatus::seeded_at`] is not — i.e. this operator recorded that it began
/// seeding THIS repository and never recorded finishing.
///
/// **The marker is the sole guard**, deliberately. A resuming migrate re-runs
/// `kopia snapshot migrate` into whatever repository is at the backend and then
/// re-stamps its maintenance owner, with no kopia-side backstop (blob mode gets
/// one for free — `sync-to` refuses a destination whose format blob differs
/// from the source's). So a repository this operator never began seeding — an
/// ordinary ADOPTION of a backend somebody else initialized, `spec.seed` left
/// standing in a GitOps manifest — must never resume: it has no marker, and it
/// keeps the no-clobber `AlreadyInitialized` no-op. Never derive `resume` from
/// anything weaker (a Job's existence, an unset `status.uniqueId`, a
/// condition).
///
/// ```
/// use kopiur_api::seed::{SeedStatus, seed_resume};
///
/// let none = SeedStatus::default();
/// // Fresh seed: armed, but no attempt was ever recorded.
/// assert!(!seed_resume(true, Some(&none)));
/// assert!(!seed_resume(true, None));
///
/// // A previous attempt started and never finished ⇒ resume.
/// let attempted = SeedStatus { started_at: Some("2026-01-01T00:00:00Z".into()), ..none.clone() };
/// assert!(seed_resume(true, Some(&attempted)));
///
/// // Finished ⇒ nothing to resume (and the seed is no longer armed anyway).
/// let done = SeedStatus { seeded_at: Some("2026-01-01T01:00:00Z".into()), ..attempted.clone() };
/// assert!(!seed_resume(true, Some(&done)));
/// assert!(!seed_resume(false, Some(&attempted)));
/// ```
pub fn seed_resume(armed: bool, status: Option<&SeedStatus>) -> bool {
    let Some(status) = status else {
        return false;
    };
    armed
        && status.started_at.as_deref().is_some_and(|s| !s.is_empty())
        && !status.seeded_at.as_deref().is_some_and(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::RepositoryKind;
    use crate::testutil::from_yaml;

    #[test]
    fn blob_source_parses_under_its_wire_key() {
        // Externally tagged: `from.backend` selects blob mode. Parsed the way
        // the cluster does (YAML -> JSON value -> typed).
        let seed: SeedSpec = from_yaml(
            r#"
from:
  backend:
    s3:
      bucket: offsite-mirror
      prefix: kopiur/
sync:
  parallel: 8
  maxDownloadSpeedBytesPerSecond: 20000000
allowEmptySource: false
failurePolicy:
  activeDeadlineSeconds: 43200
"#,
        );
        assert_eq!(seed.from.mode(), SeedMode::Blob);
        assert_eq!(seed.from.describe(), "S3");
        match seed_backend(&seed) {
            Some(Backend::S3(s3)) => assert_eq!(s3.bucket, "offsite-mirror"),
            other => panic!("expected an s3 seed backend, got {other:?}"),
        }
        assert!(seed_repository_ref(&seed).is_none());
        assert_eq!(seed.sync.and_then(|s| s.parallel), Some(8));
        // An explicit failurePolicy wins over the 24h seed default.
        assert_eq!(seed_active_deadline_seconds(&seed), 43_200);
        // Round-trip through the wire shape.
        let json = serde_json::to_value(&seed.from).expect("serialize");
        assert!(
            json.get("backend").is_some(),
            "wire key must be `backend`: {json}"
        );
    }

    #[test]
    fn migrate_source_parses_under_its_wire_key() {
        let seed: SeedSpec = from_yaml(
            r#"
from:
  repository:
    kind: ClusterRepository
    name: offsite
migrate:
  parallel: 4
  latestOnly: true
  policies: copy
credentialProjection:
  enabled: true
"#,
        );
        assert_eq!(seed.from.mode(), SeedMode::Migrate);
        assert_eq!(seed.from.describe(), "ClusterRepository/offsite");
        let r = seed_repository_ref(&seed).expect("migrate mode carries a repository ref");
        assert_eq!(r.kind, RepositoryKind::ClusterRepository);
        assert_eq!(r.name, "offsite");
        assert!(seed_backend(&seed).is_none());
        let m = seed.migrate.expect("migrate options");
        assert_eq!(m.parallel, Some(4));
        assert!(m.latest_only);
        assert_eq!(m.policies, PolicyCopyMode::Copy);
        assert!(seed.credential_projection.expect("projection").enabled);
        let json = serde_json::to_value(&seed.from).expect("serialize");
        assert!(
            json.get("repository").is_some(),
            "wire key must be `repository`: {json}"
        );
    }

    #[test]
    fn unknown_source_variant_is_rejected() {
        // An externally-tagged enum must refuse a key it does not know rather
        // than silently defaulting to one of the two real modes.
        let v: serde_json::Value = serde_yaml::from_str("mirror:\n  name: offsite\n").unwrap();
        let err = serde_json::from_value::<SeedSource>(v).expect_err("unknown variant must fail");
        assert!(
            err.to_string().contains("unknown variant"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn migrate_policies_default_to_none() {
        // kopia's own migrate default IMPORTS the source policies; kopiur's must
        // not, or a seeded repository inherits kopia-side retention that fights
        // the CR-driven timeline.
        let seed: SeedSpec = from_yaml(
            r#"
from:
  repository:
    name: offsite
migrate:
  parallel: 2
"#,
        );
        assert_eq!(
            seed.migrate.expect("migrate options").policies,
            PolicyCopyMode::None
        );
    }

    #[test]
    fn optional_blocks_are_elided_when_absent() {
        // `#[serde(skip_serializing_if)]` keeps a minimal spec minimal on the
        // wire (and out of GitOps diffs).
        let seed = seed_of(from_yaml::<SeedSource>("repository:\n  name: offsite\n"));
        let json = serde_json::to_value(&seed).expect("serialize");
        let obj = json.as_object().expect("object");
        assert_eq!(obj.keys().collect::<Vec<_>>(), vec!["from"], "{json}");
    }

    #[test]
    fn seed_is_armed_only_before_the_first_unique_id() {
        let seed = seed_of(from_yaml::<SeedSource>("repository:\n  name: offsite\n"));
        assert!(seed_armed(Some(&seed), None));
        assert!(
            seed_armed(Some(&seed), Some("")),
            "an empty pin is not a pin"
        );
        assert!(!seed_armed(Some(&seed), Some("abc123")));
        assert!(!seed_armed(None, None));
    }

    #[test]
    fn seed_mode_labels_are_stable() {
        // Metrics label values and the status enum share one vocabulary.
        assert_eq!(SeedMode::Blob.as_str(), "blob");
        assert_eq!(SeedMode::Migrate.as_str(), "migrate");
        assert_eq!(
            serde_json::to_value(SeedMode::Migrate).unwrap(),
            serde_json::json!("migrate")
        );
    }

    #[test]
    fn seed_status_round_trips() {
        let status: SeedStatus = from_yaml(
            r#"
seededAt: "2026-08-17T04:05:06Z"
mode: migrate
source: ClusterRepository/offsite
snapshotCount: 412
snapshotsCopied: 412
"#,
        );
        assert_eq!(status.mode, Some(SeedMode::Migrate));
        assert_eq!(status.snapshots_copied, Some(412));
        let reparsed: SeedStatus =
            serde_json::from_value(serde_json::to_value(&status).unwrap()).unwrap();
        assert_eq!(status, reparsed);
        // The marker is optional on the wire and elided when unset, so an
        // upgrade over a status written before #380 decodes cleanly.
        assert_eq!(status.started_at, None);
        assert!(
            !serde_json::to_value(&status)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("startedAt")
        );
    }

    #[test]
    fn resume_is_decided_by_the_attempt_marker_and_nothing_else() {
        // The full matrix the controller depends on. `armed` alone never
        // resumes — that is the ADOPTION case (a backend somebody else
        // initialized, with `spec.seed` standing in the manifest), and it must
        // keep the no-clobber AlreadyInitialized path.
        let marker = |started: Option<&str>, seeded: Option<&str>| SeedStatus {
            started_at: started.map(str::to_string),
            seeded_at: seeded.map(str::to_string),
            ..SeedStatus::default()
        };
        // (armed, status) -> resume
        let cases: &[(bool, Option<SeedStatus>, bool, &str)] = &[
            (true, None, false, "fresh seed: no status at all"),
            (
                true,
                Some(marker(None, None)),
                false,
                "fresh seed: status exists but no attempt was recorded",
            ),
            (
                true,
                Some(marker(Some(""), None)),
                false,
                "an empty marker is not a marker",
            ),
            (
                true,
                Some(marker(Some("2026-01-01T00:00:00Z"), None)),
                true,
                "retry after a recorded attempt: RESUME",
            ),
            (
                true,
                Some(marker(
                    Some("2026-01-01T00:00:00Z"),
                    Some("2026-01-01T01:00:00Z"),
                )),
                false,
                "already seeded: nothing to resume",
            ),
            (
                false,
                Some(marker(Some("2026-01-01T00:00:00Z"), None)),
                false,
                "not armed (uniqueId pinned): never resume",
            ),
            (
                true,
                Some(marker(None, Some("2026-01-01T01:00:00Z"))),
                false,
                "seeded without a marker (impossible, but must not resume)",
            ),
        ];
        for (armed, status, expected, why) in cases {
            assert_eq!(
                seed_resume(*armed, status.as_ref()),
                *expected,
                "seed_resume({armed}, {status:?}): {why}"
            );
        }
    }

    fn seed_of(from: SeedSource) -> SeedSpec {
        SeedSpec {
            from,
            sync: None,
            migrate: None,
            allow_empty_source: false,
            failure_policy: None,
            credential_projection: None,
        }
    }
}
