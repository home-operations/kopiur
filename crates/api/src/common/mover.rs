use super::*;
use k8s_openapi::api::core::v1::{
    Affinity, PodSecurityContext, ResourceRequirements, SecurityContext, Toleration,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Per-recipe mover overrides (resources, cache, security context).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MoverSpec {
    /// Resource requests/limits for the mover container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<k8s_openapi::api::core::v1::ResourceRequirements>,
    /// Override the repository's [`CacheDefaults`] for this recipe's movers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheDefaults>,
    /// Container security context for the mover; merged field-wise over the hardened base,
    /// `moverDefaults`, and any inherited context — this is the highest layer, so every field
    /// set here wins. Combines with `inheritSecurityContextFrom`: fields you set override the
    /// workload's, fields you omit are inherited, and this context stands in alone when
    /// inheritance cannot resolve a pod.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_context: Option<k8s_openapi::api::core::v1::SecurityContext>,
    /// Pod security context for the mover (notably `fsGroup` for group-writable restore
    /// volumes). Same layering as `securityContext`: highest layer, merged field-wise, and
    /// combinable with `inheritSecurityContextFrom`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_security_context: Option<k8s_openapi::api::core::v1::PodSecurityContext>,
    /// Opt-in, namespace-gated privileged mode; preserves UID/GID on restore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub privileged_mode: Option<bool>,
    /// Copy the UID/GID security context from a live workload rather than hard-coding it.
    ///
    /// Requires the workload to pin `runAsUser` (container or pod level): a UID that comes
    /// from the container image's `USER` line is invisible in the pod spec and cannot be
    /// inherited — the mover would silently run as its own image's UID instead.
    ///
    /// May be combined with `securityContext`/`podSecurityContext`, which override it
    /// field-wise and act as the fallback when no workload pod can be resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherit_security_context_from: Option<InheritSecurityContextFrom>,
    /// Per-recipe override of `Job.spec.ttlSecondsAfterFinished` so finished Jobs self-GC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds_after_finished: Option<i64>,
}

impl MoverSpec {
    /// Whether this mover requests **elevated privileges** that the workload
    /// namespace must explicitly opt into (ADR §4.11/§G16). True when
    /// `privilegedMode` is set, or the `securityContext` runs as root / privileged
    /// / with escalation / with added Linux capabilities.
    ///
    /// The rationale is the same as VolSync's `privileged-movers` model: the
    /// controller mints a mover `ServiceAccount` in the workload namespace, and a
    /// tenant with access there could reuse it to run pods at the mover's privilege.
    /// Granting an elevated mover is therefore a per-namespace admin decision, gated
    /// by a namespace annotation rather than allowed implicitly. Pure + exhaustive
    /// so the definition of "privileged" lives in one tested place.
    pub fn requires_privilege(&self) -> bool {
        requires_privilege_resolved(
            self.security_context.as_ref(),
            self.pod_security_context.as_ref(),
            self.privileged_mode,
        )
    }
}

/// How the mover co-locates with the node an RWO source/destination PVC is attached to.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
pub enum SourceColocationMode {
    /// Pin to the attached node when discoverable, else schedule freely; the default.
    #[default]
    Auto,
    /// Like `Auto`, but fail the run when an RWO PVC's node cannot be determined.
    Required,
    /// Never compute a node pin; use only the explicit `nodeSelector`/`affinity`/`tolerations`.
    Disabled,
}

/// Controls mover/source-PVC node co-location (RWO Multi-Attach avoidance).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SourceColocation {
    /// The co-location strategy. Defaults to [`SourceColocationMode::Auto`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<SourceColocationMode>,
}

/// Repository-wide mover defaults inherited by every mover, overridable per-recipe via `mover`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MoverDefaults {
    /// Container security-context base for every mover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_context: Option<SecurityContext>,
    /// Pod security-context base (notably `fsGroup`) for every mover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_security_context: Option<PodSecurityContext>,
    /// Resource requests/limits base for the mover container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
    /// kopia cache defaults for every mover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheDefaults>,
    /// Defaults for the deep-verification scratch (restore-test) volume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scratch: Option<ScratchDefaults>,
    /// Pod `nodeSelector` for every mover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_selector: Option<BTreeMap<String, String>>,
    /// Pod tolerations for every mover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tolerations: Option<Vec<Toleration>>,
    /// Pod affinity for every mover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity: Option<Affinity>,
    /// How a mover co-locates with its RWO PVC's node; defaults to [`SourceColocationMode::Auto`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_colocation: Option<SourceColocation>,
    /// `Job.spec.ttlSecondsAfterFinished` for every mover Job so finished Jobs self-GC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds_after_finished: Option<i64>,
    /// Repository throttle limits applied by every mover after it connects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub throttle: Option<Throttle>,
}

/// Built-in default `Job.spec.ttlSecondsAfterFinished` (1h) applied to a mover Job
/// when neither `moverDefaults.ttlSecondsAfterFinished` nor the recipe's
/// `mover.ttlSecondsAfterFinished` sets one, so finished backup/restore Jobs and
/// their pods self-GC instead of lingering (ADR-0005 §12).
pub const DEFAULT_JOB_TTL_SECONDS: i64 = 3600;

/// Repository-wide throttling for a mover's kopia connection; each `None` leaves kopia's current limit.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Throttle {
    /// Cap upload throughput in bytes/sec (`--upload-bytes-per-second`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_bytes_per_second: Option<i64>,
    /// Cap download throughput in bytes/sec (`--download-bytes-per-second`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_bytes_per_second: Option<i64>,
    /// Cap read/list ops/sec (`--read-requests-per-second`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_ops_per_second: Option<i64>,
    /// Cap write ops/sec (`--write-requests-per-second`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_ops_per_second: Option<i64>,
}

/// The fully-resolved mover configuration for a single run, after the 3-layer
/// field-wise merge `hardened ⊂ repo.moverDefaults ⊂ recipe.mover` (ADR-0004 §1/§2).
/// `security_context` is ALWAYS present (the hardened base guarantees it); the rest are
/// `Some` only when some layer set them. The privileged-mover gate (§4.11/§G16) runs on
/// `security_context`/`pod_security_context` *here* — the merged result — not on the raw
/// recipe, so an elevation introduced by `moverDefaults` is still gated.
pub struct ResolvedMover {
    /// Merged container security context — always present (hardened base).
    pub security_context: SecurityContext,
    /// Merged pod security context, if any layer set one.
    pub pod_security_context: Option<PodSecurityContext>,
    /// Merged resource requirements, if any layer set them.
    pub resources: Option<ResourceRequirements>,
    /// Merged cache config, if any layer set it.
    pub cache: Option<CacheDefaults>,
    /// Pod node selector from `moverDefaults` (no per-recipe override surface today).
    pub node_selector: Option<BTreeMap<String, String>>,
    /// Pod tolerations from `moverDefaults`.
    pub tolerations: Option<Vec<Toleration>>,
    /// Pod affinity from `moverDefaults`.
    pub affinity: Option<Affinity>,
    /// Resolved RWO source/destination co-location mode (`moverDefaults.sourceColocation.mode`),
    /// defaulting to [`SourceColocationMode::Auto`]. Always `Some` so the reconciler
    /// has a concrete strategy. RWO multi-attach fix.
    pub source_colocation: SourceColocationMode,
    /// Resolved Job TTL (recipe `mover.ttlSecondsAfterFinished` wins over
    /// `moverDefaults.ttlSecondsAfterFinished`, falling back to
    /// [`DEFAULT_JOB_TTL_SECONDS`]). Always `Some` so finished Jobs self-GC. §12.
    pub ttl_seconds_after_finished: Option<i64>,
    /// Resolved repository throttle (`moverDefaults.throttle`), if any. §13(e).
    pub throttle: Option<Throttle>,
}

/// Resolve the effective mover configuration via the layer merge
/// `hardened ⊂ moverDefaults ⊂ recipe` (ADR-0004 §1/§2).
///
/// - `defaults`: the repository's `moverDefaults` (None when the repo sets none).
/// - `recipe_sc`/`recipe_psc`: the recipe's **effective** container/pod context, which the
///   controller has already resolved — the explicit `mover.securityContext`/
///   `podSecurityContext` overlaid on top of any context inherited from a workload via
///   `inheritSecurityContextFrom` (they combine; explicit wins). The full ladder is
///   therefore `hardened ⊂ moverDefaults ⊂ inherited ⊂ explicit`. Layers merge as
///   `(container, pod)` **pairs** via [`merge_context_pair`]: field-wise per dimension,
///   plus identity promotion so the effective UID/GID belongs to the **highest layer that
///   pins one, regardless of which dimension it wrote** — a `moverDefaults` container-level
///   `runAsUser` can never shadow an inherited pod-level one. The pair merge is
///   associative, so folding the inner two layers before this call is identical to a flat
///   four-layer merge. The recipe layer enters here as a *layer*, NOT a whole-chain
///   replacement — the hardened base + `moverDefaults` still supply `drop:[ALL]`/seccomp
///   and a partial recipe context can only tighten.
/// - `recipe_resources`/`recipe_cache`: from `mover.resources` / `mover.cache`.
///
/// `node_selector`/`tolerations`/`affinity`/`ttl` flow from `moverDefaults` (no per-recipe
/// surface for the first three today; TTL is overridable by the caller post-resolve).
pub fn resolve_mover(
    defaults: Option<&MoverDefaults>,
    recipe_sc: Option<&SecurityContext>,
    recipe_psc: Option<&PodSecurityContext>,
    recipe_resources: Option<&ResourceRequirements>,
    recipe_cache: Option<&CacheDefaults>,
    recipe_ttl_seconds_after_finished: Option<i64>,
) -> ResolvedMover {
    // The hardened pair is the lowest layer: the container hardening plus the pod-level
    // fsGroup that makes the cache writable. Both are always present, so every mover pod
    // — bootstrap, backup, restore, maintenance, verification, replication — carries the
    // hardened defaults unless a higher layer overrides them.
    let hardened_sc = hardened_security_context();
    let hardened_psc = hardened_pod_security_context();
    // hardened ⊂ moverDefaults, as one (container, pod) layer pair.
    let (base_sc, base_psc) = merge_context_pair(
        Some(&hardened_sc),
        Some(&hardened_psc),
        defaults.and_then(|d| d.security_context.as_ref()),
        defaults.and_then(|d| d.pod_security_context.as_ref()),
    );
    // (hardened ⊂ moverDefaults) ⊂ recipe.
    let (security_context, pod_security_context) =
        merge_context_pair(base_sc.as_ref(), base_psc.as_ref(), recipe_sc, recipe_psc);
    let security_context =
        security_context.expect("the hardened container base layer is always present");
    // Normalize the merged result against every kubelet/apiserver security-context invariant
    // (see `crate::invariants`) so a contradiction the field-wise merge can assemble — most
    // importantly an inherited-root `runAsUser: 0` left under the hardened `runAsNonRoot:
    // true` — becomes a VALID (privileged-gated) mover rather than a pod wedged forever in
    // `CreateContainerConfigError`.
    let (security_context, pod_security_context) =
        crate::invariants::enforce_security_context_invariants(
            security_context,
            pod_security_context,
        );
    ResolvedMover {
        security_context,
        pod_security_context,
        resources: merge_resources_opt(
            defaults.and_then(|d| d.resources.as_ref()),
            recipe_resources,
        ),
        cache: CacheDefaults::merge(defaults.and_then(|d| d.cache.as_ref()), recipe_cache),
        node_selector: defaults.and_then(|d| d.node_selector.clone()),
        tolerations: defaults.and_then(|d| d.tolerations.clone()),
        affinity: defaults.and_then(|d| d.affinity.clone()),
        // `moverDefaults.sourceColocation.mode`, defaulting to `Auto` so RWO movers
        // co-locate with their source PVC's node out of the box (RWO multi-attach fix).
        source_colocation: defaults
            .and_then(|d| d.source_colocation.as_ref())
            .and_then(|c| c.mode)
            .unwrap_or_default(),
        // Recipe TTL wins over the repo default; a built-in default applies when
        // neither sets one so every finished Job self-GCs (ADR-0005 §12).
        ttl_seconds_after_finished: Some(
            recipe_ttl_seconds_after_finished
                .or_else(|| defaults.and_then(|d| d.ttl_seconds_after_finished))
                .unwrap_or(DEFAULT_JOB_TTL_SECONDS),
        ),
        throttle: defaults.and_then(|d| d.throttle.clone()),
    }
}

/// Selects workload pods by label.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PodSelector {
    /// Label selector matching the workload pod(s) to read context/hooks from.
    pub pod_selector: LabelSelector,
    /// Which container within the matched pod; absent uses the first/only container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
}

/// Where the mover copies its security context from instead of an explicit context.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum InheritSecurityContextFrom {
    /// Inherit from workload pod(s) matched by an explicit label selector (backup or restore).
    WorkloadSelector(PodSelector),
    /// Backup sources only: auto-derive the workload from the PVC this snapshot backs up.
    PvcConsumer(PvcConsumerInherit),
    /// Restores only: inherit the identity RECORDED on the backup itself
    /// (`Snapshot.status.recorded`, decoded from the `kopiur-meta` kopia tag) —
    /// uid/gid/fsGroup the backup mover actually ran as. Needs no live workload
    /// pod, so it works on a rebuilt cluster and with `target.populator`.
    /// Rejected at admission on SnapshotPolicy/Maintenance (backups read the
    /// live workload; maintenance has no snapshot). Write it as `snapshot: {}`
    /// (an empty sub-object) — a bare `snapshot:` is null and rejected.
    Snapshot(SnapshotInherit),
}

/// Tuning for [`InheritSecurityContextFrom::Snapshot`]. Empty today; a
/// sub-object (like [`PopulatorTarget`](crate::restore::PopulatorTarget)) so
/// future knobs slot in without API breakage.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotInherit {}

/// Tuning for [`InheritSecurityContextFrom::PvcConsumer`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PvcConsumerInherit {
    /// Which container within the matched consumer pod to inherit from; absent uses the first/only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
}
