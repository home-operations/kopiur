//! The `SnapshotReplication` CRD — copy snapshot **manifests** (and the content
//! they reference) from one repository to another on a schedule, wrapping
//! `kopia snapshot migrate` (issue #368).
//!
//! Where [`RepositoryReplication`](crate::repository_replication) mirrors a
//! repository's **blobs** verbatim (`kopia repository sync-to` — same format,
//! same password, all-or-nothing), `SnapshotReplication` copies at the
//! **snapshot** level: source and destination are BOTH real repository CRs with
//! their own passwords/formats, and a selector chooses which kopia identities
//! (and optionally only their latest snapshots) get copied. It is
//! **namespaced** (it lives alongside its source, like `Maintenance` and
//! `RepositoryReplication`) and references its repositories via
//! [`RepositoryRef`] — a cluster-scoped `ClusterRepository` destination is the
//! flagship consolidation use case.
//!
//! Each copied snapshot is materialized as a `Snapshot` CR
//! (`origin: replicated`) in this CR's namespace by the replication mover, with
//! **no ownerReference back to this CR**: deleting the `SnapshotReplication`
//! never deletes the copies. Only `spec.pruning` (or deleting the copy
//! `Snapshot` CRs themselves) removes replicated data.

use crate::common::{
    CredentialProjection, CronSpec, MoverSpec, ReplicationManualRunStatus, RepositoryRef, Retention,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Copy selected snapshots from a source repository into a destination repository on a schedule (`kopia snapshot migrate`).
///
/// Not `Eq`: `mover` transitively embeds k8s-openapi types.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[kube(
    group = "kopiur.home-operations.com",
    version = "v1alpha1",
    kind = "SnapshotReplication",
    plural = "snapshotreplications",
    namespaced,
    status = "SnapshotReplicationStatus",
    shortname = "kopiasrepl",
    category = "kopiur",
    printcolumn = r#"{"name":"Source","type":"string","jsonPath":".spec.sourceRef.name"}"#,
    printcolumn = r#"{"name":"Destination","type":"string","jsonPath":".spec.destinationRef.name"}"#,
    printcolumn = r#"{"name":"Schedule","type":"string","jsonPath":".spec.schedule.cron"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Last","type":"date","jsonPath":".status.lastReplicated"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotReplicationSpec {
    /// The `Repository` or `ClusterRepository` snapshots are copied FROM. Opened
    /// read-only by the replication mover — a replication never writes to its source.
    pub source_ref: RepositoryRef,
    /// The `Repository` or `ClusterRepository` snapshots are copied INTO. A real
    /// repository CR with its own password and format (unlike a
    /// `RepositoryReplication` destination, which is a bare backend reusing the
    /// source's password). Must not resolve to the same repository as `sourceRef`
    /// (webhook-enforced structurally; the shared validator catches the literal
    /// same-ref case at admission).
    pub destination_ref: RepositoryRef,
    /// Cron and deterministic jitter for the replication runs.
    pub schedule: CronSpec,
    /// Which snapshots to copy. Absent: every identity in the source repository,
    /// full history (`kopia snapshot migrate --all`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<SelectionSpec>,
    /// Tuning for the underlying `kopia snapshot migrate` invocation. Absent:
    /// sequential copy, and NO policy copying (see [`PolicyCopyMode`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migrate: Option<MigrateOptions>,
    /// What happens to already-replicated copies at the destination on later runs.
    /// Absent = `none`: copies accumulate forever and — like every mode here —
    /// **survive deletion of this CR** (copy `Snapshot` CRs carry no
    /// ownerReference to it). Pruning only ever considers snapshots this CR
    /// replicated (labeled at birth); it never touches the destination's own
    /// directly-written snapshots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pruning: Option<Pruning>,
    /// Mover (Job pod) overrides for the replication run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mover: Option<MoverSpec>,
    /// Opt-in projection of BOTH repositories' credential Secrets into the mover
    /// Job's namespace (required when the destination is a `ClusterRepository`
    /// whose Secrets live elsewhere).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_projection: Option<CredentialProjection>,
    /// Pause this replication; a suspended replication runs no copies (default `false`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub suspend: bool,
}

/// Which snapshots a replication copies. Pure scalars, so `Eq` (unlike the parent spec).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SelectionSpec {
    /// Select source identities (`username@hostname:path`) to copy. Absent: all
    /// identities in the source repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identities: Option<IdentitySelection>,
    /// Copy only each selected identity's most recent snapshot instead of its
    /// full history (`kopia snapshot migrate --latest`). Default `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub latest_only: bool,
}

/// Include/exclude lists of identity matchers. An identity is selected when it
/// matches at least one `include` entry (an empty/absent `include` list means
/// "everything") AND matches no `exclude` entry — exclude always wins.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IdentitySelection {
    /// Identities to copy. Empty/absent: every identity in the source.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<IdentityMatcher>,
    /// Identities to skip, applied after `include`. Exclude wins on overlap.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<IdentityMatcher>,
}

/// One glob match against a kopia identity's structured
/// `(username, hostname, sourcePath)` triple. Every set component must match
/// (AND); an unset component matches anything — but at least one component must
/// be set (webhook-refused otherwise: a fully-empty matcher constrains nothing).
///
/// Glob semantics, **per component** (non-crossing — a `*` in `username` can
/// never reach into `hostname`): `*` matches any run of characters (including
/// none), `?` matches exactly one character, everything else matches literally,
/// and the pattern is anchored (it must cover the WHOLE component — `pg` does
/// not match `pg-main`; `pg*` does). Matching is always on the structured
/// triple, never on a joined `username@hostname:path` string, so literal `@`/`:`
/// inside a component cannot confuse it. No character classes (`[...]`) or brace
/// expansion — those are rejected at admission rather than silently matched
/// literally. See [`component_glob_matches`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IdentityMatcher {
    /// Glob for the `username` component; absent matches any username.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Glob for the `hostname` component; absent matches any hostname.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// Glob for the `sourcePath` component; absent matches any path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
}

/// Tuning for `kopia snapshot migrate`. Pure scalars, so `Eq`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MigrateOptions {
    /// `--parallel`: number of snapshots migrated concurrently (kopia default
    /// `1` — sequential). Must be >= 1 when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<u32>,
    /// Whether kopia **policies** attached to the copied sources are also copied
    /// to the destination. Defaults to [`PolicyCopyMode::None`].
    #[serde(default)]
    pub policies: PolicyCopyMode,
}

/// How `kopia snapshot migrate` treats the source's kopia policies.
///
/// kopia's own default is to copy policies, but Kopiur defaults to `none`: in a
/// Kopiur-managed destination, retention is driven by `Snapshot` CRs (GFS over
/// CRs, ADR §4.4) and kopia-side policies are deliberately inert — importing the
/// source's policies could re-introduce kopia-side retention that deletes
/// destination manifests behind the operator's back. `none` maps to an explicit
/// `--no-policies`.
///
/// ```
/// use kopiur_api::snapshot_replication::PolicyCopyMode;
///
/// // Plain camelCase string enum on the wire.
/// assert_eq!(serde_json::to_value(PolicyCopyMode::CopyOverwrite).unwrap(), "copyOverwrite");
/// assert_eq!(PolicyCopyMode::default(), PolicyCopyMode::None);
/// ```
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PolicyCopyMode {
    /// Do not copy kopia policies (`--no-policies`) — the default, so a
    /// Kopiur-managed destination's retention stays CR-driven.
    #[default]
    None,
    /// Copy policies for the migrated sources, keeping any policy that already
    /// exists at the destination (kopia's migrate default).
    Copy,
    /// Copy policies and overwrite same-named policies already at the
    /// destination (`--overwrite-policies`).
    CopyOverwrite,
}

/// Exactly one pruning mode for already-replicated copies (externally tagged:
/// `pruning: { retention: {...} }`). Absent `spec.pruning` = [`Pruning::None`].
///
/// Whatever the mode, pruning only ever considers snapshots THIS replication
/// created (selected by the labels stamped at copy-CR birth); the destination's
/// own directly-written snapshots are structurally out of reach. And because
/// copy `Snapshot` CRs carry no ownerReference, deleting the
/// `SnapshotReplication` CR itself never deletes any copy — the modes below are
/// the only operator-driven way replicated data goes away.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum Pruning {
    /// Never prune: copies accumulate until deleted by hand (the default when
    /// `spec.pruning` is absent).
    None(NoPruning),
    /// Mirror the source: delete a copy when its `(identity, startTime)` has
    /// vanished from the source repository.
    MirrorSource(MirrorSourcePruning),
    /// Keep copies under an independent GFS retention at the destination,
    /// regardless of what the source still holds.
    Retention(Retention),
}

impl Pruning {
    /// Stable lowercase label for status/metrics/log fields. Exhaustive so a new
    /// mode cannot compile without naming itself.
    pub fn kind_str(&self) -> &'static str {
        match self {
            Pruning::None(_) => "none",
            Pruning::MirrorSource(_) => "mirrorSource",
            Pruning::Retention(_) => "retention",
        }
    }
}

/// Marker for [`Pruning::None`] — no fields today; a sub-object so future knobs
/// slot in without API breakage (ADR §4.11).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NoPruning {}

/// Marker for [`Pruning::MirrorSource`] — no fields today (sub-object for
/// forward-compat, ADR §4.11).
///
/// Mirror-source deletes are deliberately NOT stamped as operator prunes, so
/// they classify as EXTERNAL deletions and the destination repository's
/// mass-deletion breaker (`deletionProtection.threshold`) **holds a bulk
/// source-side vanish**: ransomware emptying the source cannot cascade into
/// emptying the off-site copy in one wave. The hold surfaces through the
/// breaker's normal machinery — a `DeletionHeld` condition on each held copy
/// `Snapshot` and `MassDeletionHeld` on the destination repository — and is
/// released with the breaker's normal timestamp acknowledgement.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MirrorSourcePruning {}

/// Anchored per-component glob match: `*` matches any run of characters
/// (including none), `?` matches exactly one, everything else is literal. This
/// is the ONE matcher both admission (via
/// [`validate_component_glob`]) and the replication mover use, so what the
/// webhook admitted and what the mover selects cannot fork. Deliberately
/// dependency-free — no regex, no glob crate.
///
/// Matching is per identity **component** ([`IdentityMatcher`]): there is no
/// separator to cross, so `*` spanning the whole component is exactly the
/// intended "match anything here" semantics.
///
/// ```
/// use kopiur_api::snapshot_replication::component_glob_matches;
///
/// assert!(component_glob_matches("pg-*", "pg-main"));
/// assert!(component_glob_matches("*", ""));
/// assert!(component_glob_matches("data-?", "data-1"));
/// assert!(!component_glob_matches("pg", "pg-main"));   // anchored: whole component
/// assert!(!component_glob_matches("data-?", "data-12"));
/// assert!(component_glob_matches("*.internal", "billing.svc.internal"));
/// ```
pub fn component_glob_matches(pattern: &str, value: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let v: Vec<char> = value.chars().collect();
    let (mut pi, mut vi) = (0usize, 0usize);
    // Last `*` seen and the value position to retry from (classic backtracking
    // wildcard match, linear in practice).
    let mut star: Option<(usize, usize)> = None;
    while vi < v.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == v[vi]) {
            pi += 1;
            vi += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, vi));
            pi += 1;
        } else if let Some((sp, sv)) = star {
            // Let the last `*` swallow one more character and retry.
            pi = sp + 1;
            vi = sv + 1;
            star = Some((sp, sv + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Admission-time check that an [`IdentityMatcher`] component pattern is one
/// [`component_glob_matches`] can honor: non-empty, no control characters, and
/// none of the glob syntax this matcher deliberately does NOT support (`[...]`
/// character classes, `{...}` brace expansion). Rejecting those up front beats
/// silently matching them as literal characters — a `data[0-9]` that never
/// matches anything is the silent-no-op failure mode this repo designs out.
///
/// Returns the human-readable reason on failure (the caller wraps it in
/// [`ValidationError::InvalidFieldValue`](crate::ValidationError::InvalidFieldValue)
/// with the field path).
///
/// ```
/// use kopiur_api::snapshot_replication::validate_component_glob;
///
/// assert!(validate_component_glob("pg-*").is_ok());
/// assert!(validate_component_glob("*").is_ok());
/// assert!(validate_component_glob("").is_err());
/// assert!(validate_component_glob("data[0-9]").is_err());
/// assert!(validate_component_glob("a{b,c}").is_err());
/// ```
pub fn validate_component_glob(pattern: &str) -> Result<(), String> {
    if pattern.is_empty() {
        return Err(
            "must not be empty — omit the component to match anything, or use \"*\"".to_string(),
        );
    }
    for c in pattern.chars() {
        match c {
            '[' | ']' | '{' | '}' => {
                return Err(format!(
                    "contains {c:?}: character classes and brace expansion are not supported; \
                     only \"*\" (any run) and \"?\" (one character) are glob metacharacters"
                ));
            }
            c if c.is_control() => {
                return Err("must not contain control characters".to_string());
            }
            _ => {}
        }
    }
    Ok(())
}

/// Lifecycle phase of a snapshot replication.
///
/// ```
/// use kopiur_api::snapshot_replication::SnapshotReplicationPhase as P;
///
/// assert_eq!(serde_json::to_value(P::Suspended).unwrap(), "Suspended");
/// // An unrecognized phase from a newer operator decodes instead of erroring.
/// let p: P = serde_json::from_value(serde_json::json!("Verifying")).unwrap();
/// assert_eq!(p, P::Unknown("Verifying".into()));
/// assert_eq!(serde_json::to_value(&p).unwrap(), "Verifying");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum SnapshotReplicationPhase {
    /// Admitted, not yet run (also the default).
    #[default]
    Pending,
    /// A replication mover Job is in flight.
    Replicating,
    /// The most recent replication completed successfully (idle until the next slot).
    Succeeded,
    /// The most recent replication run failed; see conditions.
    Failed,
    /// Suspended via `spec.suspend`.
    Suspended,
    /// A phase string this build does not recognize (newer operator, or legacy
    /// stored data). Decode-compat only — hidden from the CRD schema, never
    /// produced by this build, never a success.
    Unknown(String),
}

impl SnapshotReplicationPhase {
    /// Whether this phase is the **decode sentinel** — a value the running build
    /// cannot interpret, kept verbatim by [`Unknown`](Self::Unknown) instead of
    /// failing the whole typed `list()`/watch (#359, defect 3).
    ///
    /// Same narrow contract as
    /// [`RepositoryReplicationPhase::is_unknown`](crate::RepositoryReplicationPhase::is_unknown):
    /// `true` means only "this string is not a phase this binary knows", never
    /// "unusual" or "not one I handle". A canonical variant added to this enum
    /// later is by definition **not** the sentinel, which is why the `match` is
    /// written out exhaustively rather than left as a `matches!` — the compiler,
    /// not a reviewer, is what forces the new variant to answer.
    ///
    /// ```
    /// use kopiur_api::SnapshotReplicationPhase;
    ///
    /// assert!(SnapshotReplicationPhase::Unknown("Verifying".into()).is_unknown());
    /// assert!(!SnapshotReplicationPhase::Pending.is_unknown());
    /// assert!(!SnapshotReplicationPhase::Replicating.is_unknown());
    /// assert!(!SnapshotReplicationPhase::Suspended.is_unknown());
    /// ```
    pub fn is_unknown(&self) -> bool {
        match self {
            Self::Unknown(_) => true,
            Self::Pending
            | Self::Replicating
            | Self::Succeeded
            | Self::Failed
            | Self::Suspended => false,
        }
    }
}

crate::common::phase_serde!(
    SnapshotReplicationPhase,
    "Lifecycle phase of a snapshot replication."
);

impl crate::common::PhaseLabel for SnapshotReplicationPhase {
    const ALL: &'static [Self] = &[
        Self::Pending,
        Self::Replicating,
        Self::Succeeded,
        Self::Failed,
        Self::Suspended,
    ];
    fn label(&self) -> &str {
        match self {
            Self::Pending => "Pending",
            Self::Replicating => "Replicating",
            Self::Succeeded => "Succeeded",
            Self::Failed => "Failed",
            Self::Suspended => "Suspended",
            Self::Unknown(s) => s,
        }
    }
    fn unknown(raw: String) -> Self {
        Self::Unknown(raw)
    }
}

/// Counters from the most recent replication run, written by the mover's
/// terminal status patch. Pure scalars, so `Eq`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotReplicationRunStats {
    /// Source identities the selector matched this run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identities_selected: Option<u32>,
    /// Snapshots newly copied to the destination this run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshots_copied: Option<u32>,
    /// Selected snapshots that were already present at the destination (skipped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub already_present: Option<u32>,
    /// Selected snapshots that failed to copy (kopia migrate exits 0 on
    /// per-source failures; the mover's post-verify counts them here).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<u32>,
    /// Copies pruned this run per `spec.pruning`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pruned: Option<u32>,
}

/// Observed state of a `SnapshotReplication`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotReplicationStatus {
    /// Current lifecycle phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<SnapshotReplicationPhase>,
    /// `metadata.generation` last reconciled, for staleness detection / kstatus.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// RFC3339 timestamp of the most recent successful replication run (also the
    /// scheduling anchor for the next slot).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_replicated: Option<String>,
    /// Counters from the most recent run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<SnapshotReplicationRunStats>,
    /// Standard Kubernetes conditions (`Ready`, `Reconciling`, `Stalled`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    /// State of the most recent annotation-requested out-of-band run
    /// (`kopiur.home-operations.com/run-requested`); absent until one is
    /// requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_run: Option<ReplicationManualRunStatus>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::RepositoryKind;
    use crate::testutil::from_yaml;
    use kube::core::CustomResourceExt;

    #[test]
    fn snapshot_replication_crd_metadata_is_correct() {
        let crd = SnapshotReplication::crd();
        assert_eq!(crd.spec.group, "kopiur.home-operations.com");
        assert_eq!(crd.spec.names.kind, "SnapshotReplication");
        assert_eq!(crd.spec.names.plural, "snapshotreplications");
        assert_eq!(crd.spec.scope, "Namespaced");
        assert_eq!(crd.spec.versions[0].name, "v1alpha1");
        assert_eq!(
            crd.spec.names.short_names,
            Some(vec!["kopiasrepl".to_string()])
        );
        assert_eq!(crd.spec.names.categories, Some(vec!["kopiur".to_string()]));
    }

    #[test]
    fn snapshot_replication_full_roundtrip() {
        let yaml = r#"
sourceRef:
  kind: Repository
  name: nas-primary
destinationRef:
  kind: ClusterRepository
  name: offsite-shared
schedule:
  cron: "0 6 * * *"
  jitter: 30m
  timezone: America/Chicago
selection:
  identities:
    include:
      - username: pg-*
        hostname: billing.talos
    exclude:
      - sourcePath: "/scratch/*"
  latestOnly: true
migrate:
  parallel: 4
  policies: copyOverwrite
pruning:
  retention:
    keepDaily: 7
    keepWeekly: 4
credentialProjection:
  enabled: true
suspend: false
"#;
        let spec: SnapshotReplicationSpec = from_yaml(yaml);
        assert_eq!(spec.source_ref.kind, RepositoryKind::Repository);
        assert_eq!(spec.source_ref.name, "nas-primary");
        assert_eq!(spec.destination_ref.kind, RepositoryKind::ClusterRepository);
        assert_eq!(spec.destination_ref.name, "offsite-shared");
        assert_eq!(spec.schedule.cron, "0 6 * * *");
        assert_eq!(spec.schedule.jitter.as_deref(), Some("30m"));

        let sel = spec.selection.as_ref().expect("selection set");
        assert!(sel.latest_only);
        let ids = sel.identities.as_ref().expect("identities set");
        assert_eq!(ids.include.len(), 1);
        assert_eq!(ids.include[0].username.as_deref(), Some("pg-*"));
        assert_eq!(ids.include[0].hostname.as_deref(), Some("billing.talos"));
        assert!(ids.include[0].source_path.is_none());
        assert_eq!(ids.exclude[0].source_path.as_deref(), Some("/scratch/*"));

        let migrate = spec.migrate.expect("migrate set");
        assert_eq!(migrate.parallel, Some(4));
        assert_eq!(migrate.policies, PolicyCopyMode::CopyOverwrite);

        // Pruning is exactly one externally-tagged variant.
        match spec.pruning.as_ref().expect("pruning set") {
            Pruning::Retention(r) => {
                assert_eq!(r.keep_daily, Some(7));
                assert_eq!(r.keep_weekly, Some(4));
            }
            other => panic!("expected retention pruning, got {}", other.kind_str()),
        }
        assert!(
            spec.credential_projection
                .as_ref()
                .is_some_and(|c| c.enabled)
        );
        assert!(!spec.suspend);

        let json = serde_json::to_value(&spec).expect("serialize");
        // Externally tagged pruning + camelCase policies string.
        assert_eq!(json["pruning"]["retention"]["keepDaily"], 7);
        assert_eq!(json["migrate"]["policies"], "copyOverwrite");
        let reparsed: SnapshotReplicationSpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);
    }

    #[test]
    fn minimal_spec_omits_optionals() {
        let yaml = r#"
sourceRef: { name: nas-primary }
destinationRef: { name: offsite }
schedule: { cron: "0 6 * * 0" }
"#;
        let spec: SnapshotReplicationSpec = from_yaml(yaml);
        assert_eq!(spec.source_ref.kind, RepositoryKind::Repository);
        assert!(spec.selection.is_none());
        assert!(spec.migrate.is_none());
        assert!(spec.pruning.is_none());
        assert!(spec.mover.is_none());
        assert!(spec.credential_projection.is_none());
        assert!(!spec.suspend);
        let json = serde_json::to_value(&spec).unwrap();
        for absent in [
            "selection",
            "migrate",
            "pruning",
            "mover",
            "credentialProjection",
            "suspend",
        ] {
            assert!(json.get(absent).is_none(), "{absent} must not serialize");
        }
    }

    #[test]
    fn all_three_pruning_variants_roundtrip_under_their_external_tags() {
        for (yaml, want_kind, want_key) in [
            ("pruning: { none: {} }", "none", "none"),
            (
                "pruning: { mirrorSource: {} }",
                "mirrorSource",
                "mirrorSource",
            ),
            (
                "pruning: { retention: { keepLatest: 5 } }",
                "retention",
                "retention",
            ),
        ] {
            let full = format!(
                "sourceRef: {{ name: a }}\ndestinationRef: {{ name: b }}\nschedule: {{ cron: \"0 6 * * *\" }}\n{yaml}\n"
            );
            let spec: SnapshotReplicationSpec = from_yaml(&full);
            let pruning = spec.pruning.as_ref().expect("pruning set");
            assert_eq!(pruning.kind_str(), want_kind);
            let json = serde_json::to_value(&spec).unwrap();
            assert!(
                json["pruning"].get(want_key).is_some(),
                "pruning must serialize under the {want_key} tag: {json}"
            );
            let reparsed: SnapshotReplicationSpec = serde_json::from_value(json).expect("reparse");
            assert_eq!(spec, reparsed);
        }
    }

    #[test]
    fn unknown_pruning_variant_is_rejected() {
        let value: serde_json::Value =
            serde_yaml::from_str("keepEverything: {}\n").expect("yaml value");
        assert!(
            serde_json::from_value::<Pruning>(value).is_err(),
            "unknown pruning variant must fail to deserialize"
        );
    }

    #[test]
    fn unknown_policy_copy_mode_is_rejected() {
        assert!(
            serde_json::from_value::<PolicyCopyMode>(serde_json::json!("merge")).is_err(),
            "unknown policies mode must fail to deserialize"
        );
    }

    #[test]
    fn replication_phase_all_covers_every_variant() {
        use crate::common::PhaseLabel;
        let labels: Vec<&str> = SnapshotReplicationPhase::ALL
            .iter()
            .map(|p| p.label())
            .collect();
        assert_eq!(SnapshotReplicationPhase::ALL.len(), 5);
        assert!(labels.iter().all(|l| !l.is_empty()));
    }

    #[test]
    fn status_roundtrips_with_run_stats() {
        let status: SnapshotReplicationStatus = from_yaml(
            "phase: Succeeded\nobservedGeneration: 3\nlastReplicated: 2026-08-01T06:00:00Z\nlastRun:\n  identitiesSelected: 4\n  snapshotsCopied: 12\n  alreadyPresent: 88\n  failed: 0\n  pruned: 2\n",
        );
        assert_eq!(status.phase, Some(SnapshotReplicationPhase::Succeeded));
        let run = status.last_run.expect("lastRun set");
        assert_eq!(run.identities_selected, Some(4));
        assert_eq!(run.snapshots_copied, Some(12));
        assert_eq!(run.already_present, Some(88));
        assert_eq!(run.failed, Some(0));
        assert_eq!(run.pruned, Some(2));
        let json = serde_json::to_value(&status).unwrap();
        let reparsed: SnapshotReplicationStatus = serde_json::from_value(json).unwrap();
        assert_eq!(status, reparsed);
    }

    #[test]
    fn component_glob_matches_covers_the_matrix() {
        // (pattern, value, matches)
        for (p, v, want) in [
            ("*", "", true),
            ("*", "anything", true),
            ("pg-*", "pg-main", true),
            ("pg-*", "pg-", true),
            ("pg-*", "mysql", false),
            ("pg", "pg-main", false), // anchored: must cover the whole component
            ("?", "a", true),
            ("?", "", false),
            ("data-?", "data-1", true),
            ("data-?", "data-12", false),
            ("*-main", "pg-main", true),
            ("*-main", "pg-main-old", false),
            ("a*b*c", "aXXbYYc", true),
            ("a*b*c", "aXXcYYb", false),
            // Backtracking: the first `*` must not greedily eat the only `b`.
            ("*b", "abab", true),
            ("*.internal", "billing.svc.internal", true),
            // Literal dots are literal, not regex.
            ("a.b", "aXb", false),
            ("", "", true),
            ("", "x", false),
        ] {
            assert_eq!(
                component_glob_matches(p, v),
                want,
                "glob {p:?} vs {v:?} must be {want}"
            );
        }
    }

    #[test]
    fn manual_run_status_roundtrips_the_apiserver_way() {
        use crate::common::ReplicationManualRunPhase;
        // Parsed the cluster's way (YAML -> serde_json::Value -> typed).
        let status: SnapshotReplicationStatus = from_yaml(
            "phase: Succeeded\nmanualRun:\n  requestedAt: 2026-06-11T12:00:00Z\n  phase: Running\n",
        );
        let manual = status.manual_run.as_ref().expect("manualRun decodes");
        assert_eq!(manual.requested_at.as_deref(), Some("2026-06-11T12:00:00Z"));
        assert_eq!(manual.phase, Some(ReplicationManualRunPhase::Running));
        assert!(
            !manual.answers("2026-06-11T12:00:00Z"),
            "an in-flight run does not answer its request"
        );
        let reparsed: SnapshotReplicationStatus =
            serde_json::from_value(serde_json::to_value(&status).unwrap()).unwrap();
        assert_eq!(status, reparsed);

        // A suspended replication records the request as Pending — visible,
        // not silently queued.
        let pending: SnapshotReplicationStatus =
            from_yaml("manualRun:\n  requestedAt: 2026-06-11T12:00:00Z\n  phase: Pending\n");
        assert_eq!(
            pending.manual_run.and_then(|m| m.phase),
            Some(ReplicationManualRunPhase::Pending)
        );
    }

    #[test]
    fn manual_run_is_absent_from_a_status_that_never_requested_one() {
        let status: SnapshotReplicationStatus = from_yaml("phase: Succeeded\n");
        assert!(status.manual_run.is_none());
        let json = serde_json::to_value(&status).unwrap();
        assert!(json.get("manualRun").is_none(), "{json}");
    }

    #[test]
    fn validate_component_glob_rejects_unsupported_syntax() {
        assert!(validate_component_glob("pg-*").is_ok());
        assert!(validate_component_glob("?").is_ok());
        assert!(validate_component_glob("/data/pg").is_ok());
        for bad in ["", "data[0-9]", "a]b", "a{b,c}", "a}b", "a\tb", "a\nb"] {
            assert!(
                validate_component_glob(bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
    }
}
