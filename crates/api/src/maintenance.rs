//! The `Maintenance` CRD — schedules `kopia maintenance run` quick + full and
//! manages the ownership lease. At most one per repository. ADR-0001 §3.7.

use crate::common::{CredentialProjection, CronSpec, FailurePolicy, MoverSpec, RepositoryRef};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The schedule an operator-managed `Maintenance` uses when the owning
/// `Repository`/`ClusterRepository` does not override it: quick every 6h (30m
/// jitter), full daily at 03:00 (1h jitter). Shared by the webhook (defaulting),
/// the controller (projection), and tests, so the default lives in exactly one
/// place. ADR §3.7.
///
/// ```
/// use kopiur_api::default_maintenance_schedule;
///
/// let s = default_maintenance_schedule();
/// assert_eq!(s.quick.cron, "0 */6 * * *");
/// assert_eq!(s.quick.jitter.as_deref(), Some("30m"));
/// assert_eq!(s.full.cron, "0 3 * * *");
/// assert_eq!(s.full.jitter.as_deref(), Some("1h"));
/// assert!(s.timezone.is_none());
/// ```
pub fn default_maintenance_schedule() -> MaintenanceSchedule {
    MaintenanceSchedule {
        quick: CronSpec {
            cron: "0 */6 * * *".to_string(),
            jitter: Some("30m".to_string()),
            timezone: None,
        },
        full: CronSpec {
            cron: "0 3 * * *".to_string(),
            jitter: Some("1h".to_string()),
            timezone: None,
        },
        timezone: None,
    }
}

/// Maintenance schedule and ownership lease for one `Repository`/`ClusterRepository`.
///
/// Not `Eq`: `mover` transitively embeds k8s-openapi types.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[kube(
    group = "kopiur.home-operations.com",
    version = "v1alpha1",
    kind = "Maintenance",
    namespaced,
    status = "MaintenanceStatus",
    shortname = "kopiamaint",
    category = "kopiur",
    printcolumn = r#"{"name":"Repository","type":"string","jsonPath":".spec.repository.name"}"#,
    printcolumn = r#"{"name":"Owner","type":"string","jsonPath":".status.ownership.owner"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceSpec {
    /// Reference to the `Repository` or `ClusterRepository` to maintain.
    pub repository: RepositoryRef,
    /// quick (cheap) and full (`--full`, reclamation) maintenance crons.
    pub schedule: MaintenanceSchedule,
    /// Maintenance ownership lease holder and takeover policy.
    pub ownership: Ownership,
    /// Mover (Job pod) overrides for the maintenance run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mover: Option<MoverSpec>,
    /// How a failed maintenance run is retried and bounded (backoff, deadline).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_policy: Option<FailurePolicy>,
    /// Opt-in projection of the repository's credential Secret(s) into this run's namespace (default off).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_projection: Option<CredentialProjection>,
}

/// quick (cheap) and full (`--full`, reclamation) maintenance crons and a shared timezone.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceSchedule {
    /// Cron and jitter for `kopia maintenance run` (quick, cheap index/log work).
    pub quick: CronSpec,
    /// Cron and jitter for `kopia maintenance run --full` (content reclamation).
    pub full: CronSpec,
    /// IANA timezone both crons are evaluated in; absent means the controller default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

/// Maintenance ownership lease holder and takeover policy.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Ownership {
    /// Stable lease holder identity (e.g. `kopia-operator/nas-primary`).
    pub owner: String,
    /// Previous lease strings still recognized as SELF (a migration path):
    /// when kopia's currently-recorded maintenance owner matches the owner
    /// derived from one of these aliases, a run treats the lease as its own —
    /// it claims it and re-stamps `owner`, upgrading the recorded owner to the
    /// current format. The operator populates this when a repository's managed
    /// Maintenance moves to a cluster-qualified lease (identityDefaults.cluster),
    /// so the transition never yields the lease to what merely looks like a
    /// foreign owner.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owner_aliases: Vec<String>,
    /// What to do when the lease is already held by a different `owner`.
    #[serde(default)]
    pub takeover_policy: TakeoverPolicy,
}

/// What to do when another owner already holds the lease. Defaults to `Never`
/// (the safest: never seize a lease another owner holds).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
pub enum TakeoverPolicy {
    /// Never take over a lease another owner holds (default, safest).
    #[default]
    Never,
    /// Surface a condition prompting an operator to decide.
    PromptCondition,
    /// Forcibly claim the lease.
    Force,
}

/// What to do about the ownership lease, decided from the takeover policy and
/// whether another owner currently holds it (ADR §3.7). Exhaustive over
/// [`TakeoverPolicy`].
///
/// Lives in `kopiur-api` (not the controller) because the lease decision is made
/// in the mover for object-store repositories — only something with repo access
/// can read `kopia maintenance info` to learn the current holder. Keeping the
/// pure decision here gives the controller (filesystem) and the mover
/// (object-store) one shared, exhaustively-matched source of truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseAction {
    /// Claim the lease (we hold it or it is free).
    Claim,
    /// Forcibly take the lease from the current holder.
    Takeover,
    /// Surface a condition prompting a human to decide; do not claim.
    Prompt,
    /// Another owner holds it and policy is `Never`: do nothing, requeue.
    Yield,
}

/// Decide the lease action. `held_by_other` is true when a *different* owner
/// currently holds the maintenance lease for this repository.
///
/// ```
/// use kopiur_api::{lease_action, LeaseAction, TakeoverPolicy};
///
/// // Free (or already ours) → always claim, regardless of policy.
/// assert_eq!(lease_action(TakeoverPolicy::Never, false), LeaseAction::Claim);
/// // Held by another → dispatch on policy.
/// assert_eq!(lease_action(TakeoverPolicy::Never, true), LeaseAction::Yield);
/// assert_eq!(lease_action(TakeoverPolicy::Force, true), LeaseAction::Takeover);
/// ```
pub fn lease_action(policy: TakeoverPolicy, held_by_other: bool) -> LeaseAction {
    if !held_by_other {
        // Free or already ours → just (re)claim.
        return LeaseAction::Claim;
    }
    match policy {
        TakeoverPolicy::Never => LeaseAction::Yield,
        TakeoverPolicy::PromptCondition => LeaseAction::Prompt,
        TakeoverPolicy::Force => LeaseAction::Takeover,
    }
}

/// The logical maintenance-lease string the operator uses for a repository's
/// DEFAULT-MANAGED `Maintenance` (ADR §3.7). Single derivation, shared by the
/// managed-Maintenance projection and the bootstrap mover's initial kopia
/// owner stamp, so they cannot drift.
///
/// `cluster` is `Repository`/`ClusterRepository.spec.identityDefaults.cluster`
/// (M1/M5): `None` keeps the original, pre-multi-cluster format (a same-named
/// `ClusterRepository`/`Repository` in two clusters would otherwise derive the
/// SAME lease and race each other's maintenance on a shared repo — kopia has no
/// cross-host maintenance lock of its own, only this owner lease); `Some(c)`
/// inserts the cluster as the second path segment so each cluster claims a
/// distinct lease (and, via [`kopia_lease_identity`], a distinct kopia owner)
/// for what is otherwise the same repository name.
///
/// ```
/// use kopiur_api::common::RepositoryKind;
/// use kopiur_api::maintenance::managed_lease;
///
/// assert_eq!(
///     managed_lease(RepositoryKind::Repository, "media", "nas", None),
///     "kopiur/media/nas"
/// );
/// assert_eq!(
///     managed_lease(RepositoryKind::Repository, "media", "nas", Some("east")),
///     "kopiur/east/media/nas"
/// );
/// assert_eq!(
///     managed_lease(RepositoryKind::ClusterRepository, "ignored", "shared", None),
///     "kopiur/clusterrepository/shared"
/// );
/// assert_eq!(
///     managed_lease(RepositoryKind::ClusterRepository, "ignored", "shared", Some("east")),
///     "kopiur/east/clusterrepository/shared"
/// );
/// ```
pub fn managed_lease(
    kind: crate::common::RepositoryKind,
    namespace: &str,
    name: &str,
    cluster: Option<&str>,
) -> String {
    use crate::common::RepositoryKind;
    match (kind, cluster) {
        (RepositoryKind::Repository, None) => format!("kopiur/{namespace}/{name}"),
        (RepositoryKind::Repository, Some(c)) => format!("kopiur/{c}/{namespace}/{name}"),
        (RepositoryKind::ClusterRepository, None) => {
            format!("kopiur/clusterrepository/{name}")
        }
        (RepositoryKind::ClusterRepository, Some(c)) => {
            format!("kopiur/{c}/clusterrepository/{name}")
        }
    }
}

/// The mover-owned condition recording lease state on `Maintenance.status`:
/// `True` (lease claimed, run proceeded) or `False` with one of the reasons
/// below. Written by the mover, matched by the controller (Ready degradation)
/// and the kubectl plugin — one definition so the producers and readers cannot
/// drift.
pub const LEASE_OWNED_CONDITION: &str = "LeaseOwned";
/// `LeaseOwned=False` reason: a foreign owner holds the lease and
/// `takeoverPolicy: Never` — the run yielded.
pub const LEASE_HELD_BY_OTHER_REASON: &str = "LeaseHeldByOther";
/// `LeaseOwned=False` reason: a foreign owner holds the lease and
/// `takeoverPolicy: PromptCondition` — the run yielded, prompting the operator
/// to set `Force`.
pub const LEASE_TAKEOVER_PROMPT_REASON: &str = "LeaseTakeoverPrompt";

/// Hostname-unsafe-character rule shared by every `kopia_lease_identity` path:
/// lowercase, `[a-z0-9-]` kept, everything else collapses to `-`, repeated `-`
/// collapsed, then trimmed from both ends. Pulled out so the character class
/// itself can never drift between the legacy (whole-string) and
/// cluster-qualified (per-segment) sanitizers below.
fn sanitize_lease_fragment(s: &str) -> String {
    let mut out: String = s
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out.trim_matches('-').to_string()
}

/// A single DNS label's worth of cap for the legacy (pre-multi-cluster)
/// hostname derivation. Unchanged from the original implementation — every
/// existing doctest/behavior for a non-cluster-qualified lease must stay
/// byte-identical.
const LEGACY_HOSTNAME_MAX: usize = 63;

/// Cap for the cluster-qualified (dot-joined) hostname derivation below —
/// kopia's identity hostname has no real limit, but this mirrors DNS's overall
/// name ceiling as a defensive backstop (see [`kopia_lease_identity`] doc).
const CLUSTER_HOSTNAME_MAX: usize = 253;

/// The STABLE kopia client identity a maintenance mover assumes for `lease`
/// (`(username, hostname)`); the mover sets it with `kopia repository
/// set-client` so kopia's designated-owner check compares something stable —
/// the pod's own identity is ephemeral (a new hostname every run), which is
/// why comparing kopia's recorded owner against it can never work.
///
/// The derivation forks on the lease's own shape (the mover only ever sees the
/// lease STRING, never the CR it came from, so the rule must be derivable from
/// the string alone):
///
/// * **Exactly 4 `/`-separated segments, AND the first is literally `"kopiur"`**
///   — [`managed_lease`]'s two cluster-qualified formats
///   (`kopiur/{cluster}/{namespace}/{name}`,
///   `kopiur/{cluster}/clusterrepository/{name}`) — sanitize each segment
///   INDEPENDENTLY (the same character rule, but with no per-segment cap and
///   without ever crossing a segment boundary) and join with `.`. Every
///   generated segment is already a lowercase, dot-free RFC 1123 label (the
///   cluster name is validated as such; Kubernetes namespace/resource names
///   are too), so for a generated lease this is verbatim
///   `kopiur.{cluster}.{namespace}.{name}` — and CRITICALLY, injective: two
///   leases can only produce the same hostname if they were `/`-split
///   identically, because a `-` inside one segment can no longer be forged
///   into a fake `.` boundary the way collapsing everything to `-` allowed
///   (`kopiur/east-prod/db/x` and `kopiur/east/prod-db/x` used to collide).
///   No per-segment cap also means a long cluster/namespace/name no longer
///   collides via truncation — capped defensively only on the TOTAL, at
///   [`CLUSTER_HOSTNAME_MAX`] (253, the identity-hostname byte cap enforced by
///   [`crate::validate::validate_identity_component`]), which cluster (≤32) +
///   two Kubernetes names + `"kopiur"` cannot reach in practice.
///
///   The `"kopiur"`-first-segment check matters because [`managed_lease`] is
///   NOT the only source of lease strings: `Ownership.owner` is a free-form
///   field a user can hand-author, and a hand-authored value that HAPPENS to
///   have 4 `/`-separated segments (e.g. `a/b/c/d`) is not one of our
///   generated formats at all. Gating the dot-join on the reserved `"kopiur"`
///   prefix — which [`managed_lease`] always emits and a user has no reason to
///   — guarantees a hand-authored owner's derivation can never change across
///   an operator upgrade merely because it happens to split into 4 segments;
///   it always falls to the legacy whole-string sanitizer below, exactly as it
///   did pre-M6. Without this gate, such an owner would silently switch from
///   its `a-b-c-d` identity to `a.b.c.d`, and with `takeoverPolicy: Never` the
///   repository's maintenance would then yield forever.
/// * **Any other shape** (the legacy 3-segment formats, a 4-segment lease NOT
///   `"kopiur"`-prefixed, or any other hand-authored `Ownership.owner`/alias)
///   — the ORIGINAL whole-string sanitizer: collapse the entire lease through
///   the same character rule and cap at [`LEGACY_HOSTNAME_MAX`] (63, a DNS
///   label). Byte-identical to every pre-M6 lease this function has ever
///   produced.
///
/// ```
/// use kopiur_api::maintenance::kopia_lease_identity;
///
/// // Legacy (3-segment / hand-authored): unchanged.
/// assert_eq!(
///     kopia_lease_identity("kopiur/media/nas"),
///     ("kopiur".to_string(), "kopiur-media-nas".to_string())
/// );
///
/// // Cluster-qualified (4-segment, "kopiur"-prefixed): dot-joined, segment-preserving.
/// assert_eq!(
///     kopia_lease_identity("kopiur/east/media/nas"),
///     ("kopiur".to_string(), "kopiur.east.media.nas".to_string())
/// );
///
/// // A hand-authored 4-segment owner that is NOT "kopiur"-prefixed: legacy
/// // sanitization, byte-identical to pre-M6 — never dot-joined.
/// assert_eq!(
///     kopia_lease_identity("a/b/c/d"),
///     ("kopiur".to_string(), "a-b-c-d".to_string())
/// );
///
/// // Injective: a '-' inside a segment can no longer masquerade as a boundary.
/// assert_ne!(
///     kopia_lease_identity("kopiur/east-prod/db/x").1,
///     kopia_lease_identity("kopiur/east/prod-db/x").1
/// );
/// ```
pub fn kopia_lease_identity(lease: &str) -> (String, String) {
    let segments: Vec<&str> = lease.split('/').collect();
    let host = match segments.as_slice() {
        [a, b, c, d] if *a == "kopiur" => {
            let joined = [a, b, c, d]
                .iter()
                .map(|s| sanitize_lease_fragment(s))
                .collect::<Vec<_>>()
                .join(".");
            let capped: String = joined.chars().take(CLUSTER_HOSTNAME_MAX).collect();
            capped.trim_end_matches(['.', '-']).to_string()
        }
        _ => {
            let host = sanitize_lease_fragment(lease);
            let capped: String = host.chars().take(LEGACY_HOSTNAME_MAX).collect();
            capped.trim_end_matches('-').to_string()
        }
    };
    ("kopiur".to_string(), host)
}

/// The full `user@hostname` owner string kopia records for `lease` — what the
/// mover compares `maintenance info`'s owner against, and what the bootstrap
/// stamps on a repository it CREATES.
///
/// ```
/// use kopiur_api::maintenance::kopia_owner_for_lease;
///
/// assert_eq!(kopia_owner_for_lease("kopiur/media/nas"), "kopiur@kopiur-media-nas");
/// ```
pub fn kopia_owner_for_lease(lease: &str) -> String {
    let (user, host) = kopia_lease_identity(lease);
    format!("{user}@{host}")
}

/// Whether kopia's currently-recorded maintenance owner is a DIFFERENT owner
/// than us — i.e. neither our own lease's owner nor one of our recognized
/// [`Ownership::owner_aliases`] (the migration path: a repo whose managed
/// `Maintenance` moved to a new lease format still recognizes the owner it
/// used to stamp as itself, so the transition claims and re-stamps rather than
/// yielding to what would otherwise look like a foreign owner).
///
/// `current` is empty for a never-run repository (kopia's own "no owner set"
/// state) — never "held by another".
///
/// ```
/// use kopiur_api::maintenance::{kopia_owner_for_lease, lease_held_by_other};
///
/// let lease = "kopiur/east/media/nas";
/// let alias = "kopiur/media/nas"; // the pre-cluster lease this repo used to use
/// let mine = kopia_owner_for_lease(lease);
/// let legacy = kopia_owner_for_lease(alias);
///
/// // Never-run repository: empty owner is never "held by another".
/// assert!(!lease_held_by_other("", lease, &[]));
/// // Already ours: not held by another.
/// assert!(!lease_held_by_other(&mine, lease, &[]));
/// // The recognized alias's owner: treated as self (migration path).
/// assert!(!lease_held_by_other(&legacy, lease, &[alias.to_string()]));
/// // The SAME string but the alias isn't registered: a genuine foreign owner.
/// assert!(lease_held_by_other(&legacy, lease, &[]));
/// // Any other owner: foreign.
/// assert!(lease_held_by_other("someone@else", lease, &[alias.to_string()]));
/// ```
pub fn lease_held_by_other(current: &str, lease: &str, aliases: &[String]) -> bool {
    if current.is_empty() {
        return false;
    }
    if current == kopia_owner_for_lease(lease) {
        return false;
    }
    !aliases
        .iter()
        .any(|alias| current == kopia_owner_for_lease(alias))
}

/// The `kubectl kopiur` invocation that stamps a `Maintenance` run request,
/// quoted in the fix hint when the annotation does not parse.
const MAINTENANCE_RUN_COMMAND: &str = "kubectl kopiur maintenance run";

/// Parse the `run-requested`/`run-mode` annotations into a manual-run request.
/// `Ok(None)` = no request; `Err` = the annotations are present but malformed
/// (the messages say how to fix). Shared by the admission webhook and the
/// controller so validation cannot fork (SKILL "one validator, two callers").
///
/// The timestamp half is [`crate::common::parse_run_requested_at`] — the one
/// parser every "run it now" surface shares (both replication kinds call it
/// directly); only the `run-mode` companion is maintenance-specific.
pub fn parse_run_annotations(
    annotations: Option<&std::collections::BTreeMap<String, String>>,
) -> Result<Option<(chrono::DateTime<chrono::Utc>, ManualRunMode)>, String> {
    let Some(at) = crate::common::parse_run_requested_at(annotations, MAINTENANCE_RUN_COMMAND)?
    else {
        return Ok(None);
    };
    let mode = match annotations.and_then(|a| a.get(crate::consts::RUN_MODE_ANNOTATION)) {
        None => ManualRunMode::Quick,
        Some(raw_mode) => ManualRunMode::parse(raw_mode).ok_or_else(|| {
            format!(
                "annotation {} must be `quick` or `full` (got {raw_mode:?}). \
                 Fix: re-annotate with a valid mode",
                crate::consts::RUN_MODE_ANNOTATION
            )
        })?,
    };
    Ok(Some((at, mode)))
}

/// Inline maintenance control on a `Repository`/`ClusterRepository` (`spec.maintenance`).
///
/// Not `Eq`: `mover` transitively embeds k8s-openapi types.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryMaintenanceSpec {
    /// Whether the operator manages a `Maintenance` CR for this repository (default `true`).
    #[serde(default = "crate::common::default_true")]
    pub enabled: bool,
    /// Schedule override; absent uses the default quick-6h / full-daily schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<MaintenanceSchedule>,
    /// Mover overrides for the managed `Maintenance`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mover: Option<MoverSpec>,
    /// Failure handling (backoff/deadline) for the managed `Maintenance` run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_policy: Option<FailurePolicy>,
    /// Lease takeover policy for the managed `Maintenance` (default `Never`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub takeover_policy: Option<TakeoverPolicy>,
    /// ClusterRepository only: namespace the managed `Maintenance` CR is created in (default the operator's namespace).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

impl Default for RepositoryMaintenanceSpec {
    /// Default-on with no overrides. `enabled` is `true` here to match the serde
    /// `default_true` so a constructed default and a deserialized `{}` agree.
    fn default() -> Self {
        Self {
            enabled: true,
            schedule: None,
            mover: None,
            failure_policy: None,
            takeover_policy: None,
            namespace: None,
        }
    }
}

/// Observed maintenance state: lease holder and per-kind run results.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceStatus {
    /// The `metadata.generation` this status reflects, for staleness detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// Current lease holder, if the lease has been claimed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ownership: Option<OwnershipStatus>,
    /// Last/next-run state for the quick maintenance schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quick: Option<RunStatus>,
    /// Last/next-run state for the full maintenance schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full: Option<RunStatus>,
    /// Standard Kubernetes conditions surfacing maintenance health.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    /// State of the most recent annotation-requested out-of-band run; absent until one is requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_run: Option<ManualRunStatus>,
}

/// Which maintenance kind a manual (annotation-requested) run performs; the wire
/// values are the `run-mode` annotation values. Defaults to `quick`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ManualRunMode {
    /// `kopia maintenance run` (quick); the default when `run-mode` is absent.
    #[default]
    Quick,
    /// `kopia maintenance run --full`.
    Full,
}

impl ManualRunMode {
    /// Parse a `run-mode` annotation value. Exact-match, lowercase — the same
    /// strings serde uses on the wire.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "quick" => Some(Self::Quick),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    /// The stable wire/annotation string.
    pub fn label(self) -> &'static str {
        match self {
            Self::Quick => "quick",
            Self::Full => "full",
        }
    }
}

/// Lifecycle of a manual run.
///
/// **Closed on the wire, open on decode.** The CRD schema still admits exactly
/// `Running`/`Succeeded`/`Failed` — the apiserver rejects anything else on
/// every write — but [`Self::Unknown`] exists so a value written by a NEWER
/// kopiur decodes instead of failing the typed watch for the whole Kind. The
/// schema `description` this type publishes deliberately still reads "Closed
/// enum." and is frozen there; see `phase_serde!` on why the rustdoc and the
/// schema text diverge.
///
/// ```
/// use kopiur_api::maintenance::ManualRunPhase;
///
/// assert_eq!(serde_json::to_value(ManualRunPhase::Running).unwrap(), "Running");
/// // An unrecognized phase from a newer operator decodes instead of erroring.
/// let p: ManualRunPhase = serde_json::from_value(serde_json::json!("Queued")).unwrap();
/// assert_eq!(p, ManualRunPhase::Unknown("Queued".into()));
/// assert_eq!(serde_json::to_value(&p).unwrap(), "Queued");
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManualRunPhase {
    /// The mover Job for this request is in flight.
    Running,
    /// The run finished successfully or yielded the lease cleanly (see the `LeaseOwned` condition).
    Succeeded,
    /// The run's Job failed; conditions carry the detail.
    Failed,
    /// A phase string this build does not recognize (newer operator, or legacy
    /// stored data). Decode-compat only — hidden from the CRD schema, never
    /// produced by this build. Never counted as a finished run, so a
    /// re-run request is never deduped against it.
    Unknown(String),
}

crate::common::phase_serde!(ManualRunPhase, "Lifecycle of a manual run. Closed enum.");

impl crate::common::PhaseLabel for ManualRunPhase {
    const ALL: &'static [Self] = &[Self::Running, Self::Succeeded, Self::Failed];
    fn label(&self) -> &str {
        match self {
            Self::Running => "Running",
            Self::Succeeded => "Succeeded",
            Self::Failed => "Failed",
            Self::Unknown(s) => s,
        }
    }
    fn unknown(raw: String) -> Self {
        Self::Unknown(raw)
    }
}

/// Bookkeeping for the most recent annotation-requested run.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ManualRunStatus {
    /// The `run-requested` annotation value this status reflects (RFC3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_at: Option<String>,
    /// The run kind that was performed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<ManualRunMode>,
    /// Where the run is in its lifecycle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<ManualRunPhase>,
    /// RFC3339 instant the run reached a terminal phase.
    // Deliberately serialized EVEN WHEN `None` (no `skip_serializing_if`), for
    // the same reason as `common::ReplicationManualRunStatus::completed_at`: a
    // non-terminal phase emits `"completedAt": null` so the RFC-7386 merge-patch
    // DELETES the previous run's stamp instead of leaving it standing over a
    // fresh `Running` (#394). Maintenance patches `manualRun` unconditionally
    // (no noop guard), so here the stake is a truthful status rather than a
    // non-converging write loop.
    //
    // This depends on `patch_status` sending `kube::api::Patch::Merge`
    // (`crates/controller/src/io/apply.rs`). Under `Patch::Apply` an explicit
    // null does NOT delete, and this contract silently breaks.
    //
    // Kept a plain comment rather than rustdoc on purpose: doc comments become
    // the CRD `description` (`kubectl explain`, docs/field-reference.md).
    #[serde(default)]
    pub completed_at: Option<String>,
}

/// Observed ownership-lease state: who holds it and since when.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OwnershipStatus {
    /// The current lease holder's identity (matches `Ownership.owner`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// RFC3339 instant the lease was claimed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_at: Option<String>,
}

/// Per-kind (quick/full) run status.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunStatus {
    /// RFC3339 instant of the most recent run of this kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<String>,
    /// RFC3339 instant of the next scheduled run of this kind (cron + jitter, pinned).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_scheduled_at: Option<String>,
    /// RFC3339 instant the controller last observed this kind's per-slot Job reach terminal success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_handled_at: Option<String>,
    /// Count of back-to-back failed runs of this kind; resets on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consecutive_failures: Option<i64>,
    /// Bytes of storage reclaimed by the most recent run of this kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_content_reclaimed_bytes: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{PhaseLabel, RepositoryKind};
    use crate::testutil::from_yaml;
    use kube::core::CustomResourceExt;

    #[test]
    fn manual_run_phase_all_covers_every_variant_uniquely() {
        // `ManualRunPhase` was the one phase enum with no `PhaseLabel` impl, so
        // nothing could enumerate it. Same tripwire as the other phases: every
        // variant in ALL, unique non-empty labels, and the label string equal to
        // the serde encoding (this phase is written to `status.manualRun.phase`,
        // so a label that drifts from the wire value would mislabel metrics and
        // any CLI rendering).
        let labels: Vec<&str> = ManualRunPhase::ALL.iter().map(|p| p.label()).collect();
        assert_eq!(ManualRunPhase::ALL.len(), 3);
        assert!(labels.iter().all(|l| !l.is_empty()));
        let mut sorted = labels.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len(), "phase labels must be unique");
        for p in ManualRunPhase::ALL {
            assert_eq!(
                serde_json::to_value(p).expect("serialize"),
                p.label(),
                "{p:?}"
            );
        }
    }

    #[test]
    fn manual_run_status_roundtrips_camel_case_and_nulls_a_missing_completion() {
        // Parsed the cluster's way (YAML -> serde_json::Value -> typed), which
        // is the only path that proves the camelCase wire names land.
        let status: MaintenanceStatus = from_yaml(
            "manualRun:\n  requestedAt: 2026-06-11T12:00:00Z\n  mode: full\n  phase: Succeeded\n  completedAt: 2026-06-11T12:01:42Z\n",
        );
        let manual = status.manual_run.expect("manualRun decodes");
        assert_eq!(manual.requested_at.as_deref(), Some("2026-06-11T12:00:00Z"));
        assert_eq!(manual.mode, Some(ManualRunMode::Full));
        assert_eq!(manual.phase, Some(ManualRunPhase::Succeeded));
        assert_eq!(manual.completed_at.as_deref(), Some("2026-06-11T12:01:42Z"));
        let json = serde_json::to_value(&manual).unwrap();
        assert_eq!(json["requestedAt"], "2026-06-11T12:00:00Z");
        assert_eq!(json["mode"], "full");
        assert_eq!(json["phase"], "Succeeded");
        assert_eq!(json["completedAt"], "2026-06-11T12:01:42Z");

        // #394: a non-terminal run emits an EXPLICIT null completedAt, so the
        // RFC-7386 merge-patch deletes the previous run's stamp rather than
        // leaving it standing over a fresh `Running`.
        let running = serde_json::to_value(ManualRunStatus {
            requested_at: Some("2026-06-11T13:00:00Z".into()),
            mode: Some(ManualRunMode::Quick),
            phase: Some(ManualRunPhase::Running),
            completed_at: None,
        })
        .unwrap();
        assert_eq!(
            running,
            serde_json::json!({
                "requestedAt": "2026-06-11T13:00:00Z",
                "mode": "quick",
                "phase": "Running",
                "completedAt": null,
            })
        );
        // The explicit null reads back as "no completion instant".
        let back: ManualRunStatus = serde_json::from_value(running).unwrap();
        assert!(back.completed_at.is_none());
        assert_eq!(
            serde_json::to_value(ManualRunStatus::default()).unwrap(),
            serde_json::json!({ "completedAt": null })
        );

        // A status that never requested a run still has NO manualRun key —
        // the parent field keeps its own skip_serializing_if.
        let never: MaintenanceStatus = from_yaml("observedGeneration: 3\n");
        assert!(never.manual_run.is_none());
        assert!(
            serde_json::to_value(&never)
                .unwrap()
                .get("manualRun")
                .is_none()
        );
    }

    #[test]
    fn lease_identity_is_hostname_safe_and_stable() {
        let (user, host) = kopia_lease_identity("kopiur/media/My_App.x");
        assert_eq!(user, "kopiur");
        assert_eq!(host, "kopiur-media-my-app-x");
        // Long leases cap at a DNS label and never end with '-'.
        let (_, host) = kopia_lease_identity(&format!("kopiur/{}/x", "n".repeat(100)));
        assert!(host.len() <= 63, "{host}");
        assert!(!host.ends_with('-'), "{host}");
        // Deterministic.
        assert_eq!(
            kopia_owner_for_lease("kopiur/media/nas"),
            kopia_owner_for_lease("kopiur/media/nas")
        );
    }

    #[test]
    fn parse_run_annotations_covers_ok_default_and_garbage() {
        use std::collections::BTreeMap;
        assert_eq!(parse_run_annotations(None), Ok(None));
        let mut a = BTreeMap::new();
        a.insert(
            crate::consts::RUN_REQUESTED_ANNOTATION.to_string(),
            "2026-06-11T12:00:00Z".to_string(),
        );
        let (_, mode) = parse_run_annotations(Some(&a)).unwrap().unwrap();
        assert_eq!(mode, ManualRunMode::Quick, "mode defaults to quick");
        a.insert(
            crate::consts::RUN_MODE_ANNOTATION.to_string(),
            "full".to_string(),
        );
        let (_, mode) = parse_run_annotations(Some(&a)).unwrap().unwrap();
        assert_eq!(mode, ManualRunMode::Full);
        a.insert(
            crate::consts::RUN_REQUESTED_ANNOTATION.to_string(),
            "yesterday".to_string(),
        );
        let err = parse_run_annotations(Some(&a)).unwrap_err();
        assert!(err.contains("must be an RFC3339 timestamp"), "{err}");
        assert!(err.contains("kubectl kopiur maintenance run"), "{err}");
    }

    #[test]
    fn maintenance_crd_metadata_is_correct() {
        let crd = Maintenance::crd();
        assert_eq!(crd.spec.group, "kopiur.home-operations.com");
        assert_eq!(crd.spec.names.kind, "Maintenance");
        assert_eq!(crd.spec.scope, "Namespaced");
        assert_eq!(crd.spec.versions[0].name, "v1alpha1");
    }

    #[test]
    fn maintenance_roundtrip_matches_adr_shape() {
        // Mirrors ADR-0001 §3.7.
        let yaml = r#"
repository:
  kind: Repository
  name: nas-primary
schedule:
  quick: { cron: "0 */6 * * *", jitter: 30m }
  full:  { cron: "0 3 * * 0", jitter: 1h }
  timezone: UTC
ownership:
  owner: "kopia-operator/nas-primary"
  takeoverPolicy: PromptCondition
mover:
  resources: { requests: { cpu: 250m, memory: 1Gi }, limits: { cpu: "2", memory: 4Gi } }
  securityContext: { runAsUser: 1000, runAsNonRoot: true }
  podSecurityContext: { fsGroup: 1000 }
failurePolicy:
  backoffLimit: 1
  activeDeadlineSeconds: 14400
"#;
        let spec: MaintenanceSpec = from_yaml(yaml);
        assert_eq!(spec.repository.kind, RepositoryKind::Repository);
        // The mover security contexts (container + pod) round-trip on Maintenance too.
        let mover = spec.mover.as_ref().expect("mover");
        assert_eq!(
            mover.security_context.as_ref().and_then(|s| s.run_as_user),
            Some(1000)
        );
        assert_eq!(
            mover.pod_security_context.as_ref().and_then(|p| p.fs_group),
            Some(1000)
        );
        assert_eq!(spec.schedule.quick.cron, "0 */6 * * *");
        assert_eq!(spec.schedule.quick.jitter.as_deref(), Some("30m"));
        assert_eq!(spec.schedule.full.cron, "0 3 * * 0");
        assert_eq!(spec.schedule.timezone.as_deref(), Some("UTC"));
        assert_eq!(spec.ownership.owner, "kopia-operator/nas-primary");
        assert_eq!(
            spec.ownership.takeover_policy,
            TakeoverPolicy::PromptCondition
        );
        assert_eq!(
            spec.failure_policy
                .as_ref()
                .unwrap()
                .active_deadline_seconds,
            Some(14400)
        );

        let json = serde_json::to_value(&spec).expect("serialize");
        let reparsed: MaintenanceSpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(spec, reparsed);
    }

    #[test]
    fn maintenance_status_roundtrips() {
        // Mirrors ADR-0001 §3.7 status block.
        let yaml = r#"
ownership:
  owner: "kopia-operator/nas-primary"
  claimedAt: 2026-05-12T08:14:02Z
quick:
  lastRunAt: 2026-05-24T12:00:11Z
  nextScheduledAt: 2026-05-24T18:00:00Z
  consecutiveFailures: 0
  lastContentReclaimedBytes: 1234567
full:
  lastRunAt: 2026-05-19T03:01:42Z
  nextScheduledAt: 2026-05-26T03:00:00Z
  consecutiveFailures: 0
  lastContentReclaimedBytes: 89456789012
"#;
        let status: MaintenanceStatus = from_yaml(yaml);
        assert_eq!(
            status.ownership.as_ref().unwrap().owner.as_deref(),
            Some("kopia-operator/nas-primary")
        );
        assert_eq!(
            status.quick.as_ref().unwrap().last_content_reclaimed_bytes,
            Some(1234567)
        );
        assert_eq!(
            status.full.as_ref().unwrap().last_content_reclaimed_bytes,
            Some(89456789012)
        );

        let json = serde_json::to_value(&status).unwrap();
        let reparsed: MaintenanceStatus = serde_json::from_value(json).unwrap();
        assert_eq!(status, reparsed);
    }

    #[test]
    fn repository_maintenance_defaults_to_enabled() {
        // An empty `spec.maintenance: {}` is default-on with no overrides.
        let m: RepositoryMaintenanceSpec = from_yaml("{}\n");
        assert!(
            m.enabled,
            "absent `enabled` must default to true (default-on)"
        );
        assert!(m.schedule.is_none());
        assert!(m.namespace.is_none());
        assert!(m.takeover_policy.is_none());
        // The constructed Default agrees with the deserialized `{}`.
        assert_eq!(m, RepositoryMaintenanceSpec::default());
    }

    #[test]
    fn repository_maintenance_roundtrip_with_overrides() {
        let yaml = r#"
enabled: false
schedule:
  quick: { cron: "0 */4 * * *", jitter: 20m }
  full:  { cron: "30 2 * * *", jitter: 45m }
  timezone: America/Chicago
takeoverPolicy: Force
namespace: kopia-system
failurePolicy:
  backoffLimit: 2
"#;
        let m: RepositoryMaintenanceSpec = from_yaml(yaml);
        assert!(!m.enabled);
        let s = m.schedule.as_ref().expect("schedule");
        assert_eq!(s.quick.cron, "0 */4 * * *");
        assert_eq!(s.full.jitter.as_deref(), Some("45m"));
        assert_eq!(s.timezone.as_deref(), Some("America/Chicago"));
        assert_eq!(m.takeover_policy, Some(TakeoverPolicy::Force));
        assert_eq!(m.namespace.as_deref(), Some("kopia-system"));
        assert_eq!(m.failure_policy.as_ref().unwrap().backoff_limit, Some(2));

        let json = serde_json::to_value(&m).expect("serialize");
        let reparsed: RepositoryMaintenanceSpec = serde_json::from_value(json).expect("reparse");
        assert_eq!(m, reparsed);
    }

    #[test]
    fn default_maintenance_schedule_is_quick_6h_full_daily() {
        let s = default_maintenance_schedule();
        assert_eq!(s.quick.cron, "0 */6 * * *");
        assert_eq!(s.quick.jitter.as_deref(), Some("30m"));
        assert_eq!(s.full.cron, "0 3 * * *");
        assert_eq!(s.full.jitter.as_deref(), Some("1h"));
        assert!(s.timezone.is_none());
    }

    #[test]
    fn free_lease_is_claimed_regardless_of_policy() {
        for p in [
            TakeoverPolicy::Never,
            TakeoverPolicy::PromptCondition,
            TakeoverPolicy::Force,
        ] {
            assert_eq!(lease_action(p, false), LeaseAction::Claim);
        }
    }

    #[test]
    fn held_lease_dispatches_by_policy() {
        assert_eq!(
            lease_action(TakeoverPolicy::Never, true),
            LeaseAction::Yield
        );
        assert_eq!(
            lease_action(TakeoverPolicy::PromptCondition, true),
            LeaseAction::Prompt
        );
        assert_eq!(
            lease_action(TakeoverPolicy::Force, true),
            LeaseAction::Takeover
        );
    }

    #[test]
    fn takeover_policy_serializes_to_expected_strings() {
        assert_eq!(
            serde_json::to_value(TakeoverPolicy::Never).unwrap(),
            "Never"
        );
        assert_eq!(
            serde_json::to_value(TakeoverPolicy::PromptCondition).unwrap(),
            "PromptCondition"
        );
        assert_eq!(
            serde_json::to_value(TakeoverPolicy::Force).unwrap(),
            "Force"
        );
        assert_eq!(TakeoverPolicy::default(), TakeoverPolicy::Never);
    }

    #[test]
    fn manual_run_mode_parses_exact_lowercase_and_defaults_to_quick() {
        assert_eq!(ManualRunMode::default(), ManualRunMode::Quick);
        assert_eq!(ManualRunMode::parse("quick"), Some(ManualRunMode::Quick));
        assert_eq!(ManualRunMode::parse("full"), Some(ManualRunMode::Full));
        assert_eq!(ManualRunMode::parse("FULL"), None); // exact, lowercase only
        assert_eq!(serde_json::to_value(ManualRunMode::Quick).unwrap(), "quick");
    }

    // --- M6: cluster-qualified maintenance lease -----------------------------

    #[test]
    fn managed_lease_covers_all_four_arms() {
        assert_eq!(
            managed_lease(RepositoryKind::Repository, "media", "nas", None),
            "kopiur/media/nas"
        );
        assert_eq!(
            managed_lease(RepositoryKind::Repository, "media", "nas", Some("east")),
            "kopiur/east/media/nas"
        );
        assert_eq!(
            managed_lease(RepositoryKind::ClusterRepository, "ignored", "shared", None),
            "kopiur/clusterrepository/shared"
        );
        assert_eq!(
            managed_lease(
                RepositoryKind::ClusterRepository,
                "ignored",
                "shared",
                Some("east")
            ),
            "kopiur/east/clusterrepository/shared"
        );
    }

    #[test]
    fn legacy_lease_shapes_are_byte_identical_to_pre_m6() {
        // 3-segment namespaced-Repository format: unchanged.
        assert_eq!(
            kopia_lease_identity("kopiur/media/nas"),
            ("kopiur".to_string(), "kopiur-media-nas".to_string())
        );
        // 3-segment ClusterRepository format: unchanged.
        assert_eq!(
            kopia_lease_identity("kopiur/clusterrepository/shared"),
            (
                "kopiur".to_string(),
                "kopiur-clusterrepository-shared".to_string()
            )
        );
        // 2-segment hand-authored owner: unchanged (falls through to legacy).
        assert_eq!(
            kopia_lease_identity("kopia-operator/nas-primary"),
            (
                "kopiur".to_string(),
                "kopia-operator-nas-primary".to_string()
            )
        );
        // Mixed-case/punctuation whole-string sanitization: unchanged.
        let (user, host) = kopia_lease_identity("kopiur/media/My_App.x");
        assert_eq!(user, "kopiur");
        assert_eq!(host, "kopiur-media-my-app-x");
        // Long legacy leases still cap at a DNS label and never end with '-'.
        let (_, host) = kopia_lease_identity(&format!("kopiur/{}/x", "n".repeat(100)));
        assert!(host.len() <= 63, "{host}");
        assert!(!host.ends_with('-'), "{host}");
    }

    /// Hardening (fix round 1): a hand-authored `Ownership.owner` that HAPPENS
    /// to be 4 `/`-separated segments must NOT be mistaken for one of
    /// `managed_lease`'s generated cluster-qualified formats — those are always
    /// `"kopiur"`-first-segment. Without this gate, an owner like `a/b/c/d`
    /// would silently change derivation from the legacy `a-b-c-d` to the
    /// dot-joined `a.b.c.d` across an operator upgrade, and with
    /// `takeoverPolicy: Never` maintenance would then yield forever.
    #[test]
    fn four_segment_non_kopiur_lease_is_legacy_byte_identical_to_pre_m6() {
        assert_eq!(
            kopia_lease_identity("a/b/c/d"),
            ("kopiur".to_string(), "a-b-c-d".to_string())
        );
        // Still legacy even when the segments individually look plausible.
        assert_eq!(
            kopia_lease_identity("east/media/nas/extra"),
            ("kopiur".to_string(), "east-media-nas-extra".to_string())
        );
    }

    #[test]
    fn cluster_qualified_lease_is_dot_joined_and_injective() {
        // Verbatim dot-join for already-clean generated segments.
        assert_eq!(
            kopia_lease_identity("kopiur/east/media/nas"),
            ("kopiur".to_string(), "kopiur.east.media.nas".to_string())
        );
        assert_eq!(
            kopia_lease_identity("kopiur/east/clusterrepository/shared"),
            (
                "kopiur".to_string(),
                "kopiur.east.clusterrepository.shared".to_string()
            )
        );

        // Adversarial: a '-' inside a segment must not be forgeable into a
        // fake '.' boundary (the whole-string collapse this replaces would
        // make these two leases collide).
        assert_ne!(
            kopia_lease_identity("kopiur/east-prod/db/x").1,
            kopia_lease_identity("kopiur/east/prod-db/x").1
        );

        // Two 32-char clusters sharing a 31-char prefix must stay distinct —
        // the old cap-at-63 truncation would have collided these.
        let cluster_a = format!("{}x", "a".repeat(31));
        let cluster_b = format!("{}y", "a".repeat(31));
        assert_eq!(cluster_a.len(), 32);
        assert_eq!(cluster_b.len(), 32);
        let lease_a = format!("kopiur/{cluster_a}/media/nas");
        let lease_b = format!("kopiur/{cluster_b}/media/nas");
        assert_ne!(
            kopia_lease_identity(&lease_a).1,
            kopia_lease_identity(&lease_b).1
        );

        // Dots land in the hostname, and the shared identity-shape validator
        // (kopia's username/hostname contract) accepts them: dots are
        // explicitly permitted, only '@'/':'/whitespace/control chars/length
        // are rejected.
        let (_, host) = kopia_lease_identity("kopiur/east/media/nas");
        assert!(host.contains('.'));
        assert!(crate::validate::validate_identity_component("hostname", &host).is_ok());
    }

    #[test]
    fn lease_held_by_other_table() {
        let lease = "kopiur/east/media/nas";
        let alias = "kopiur/media/nas";
        let mine = kopia_owner_for_lease(lease);
        let alias_owner = kopia_owner_for_lease(alias);

        // empty current: never-run repo, never "held by another".
        assert!(!lease_held_by_other("", lease, &[]));
        assert!(!lease_held_by_other("", lease, &[alias.to_string()]));
        // own owner: not held by another, with or without aliases configured.
        assert!(!lease_held_by_other(&mine, lease, &[]));
        assert!(!lease_held_by_other(&mine, lease, &[alias.to_string()]));
        // a registered alias's owner: treated as self (migration path).
        assert!(!lease_held_by_other(
            &alias_owner,
            lease,
            &[alias.to_string()]
        ));
        // the SAME owner string, but the alias isn't registered: foreign.
        assert!(lease_held_by_other(&alias_owner, lease, &[]));
        // a genuinely foreign owner, with an unrelated alias configured: foreign.
        assert!(lease_held_by_other(
            "someone@else",
            lease,
            &[alias.to_string()]
        ));
    }
}
