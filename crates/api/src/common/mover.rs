use super::*;
use k8s_openapi::api::core::v1::{
    Affinity, PodSecurityContext, ResourceRequirements, SecurityContext, Toleration,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Per-recipe mover overrides (resources, cache, security context). ADR §3.3.
///
/// These overlay the repository's [`MoverDefaults`] **field-wise** (recipe wins, the
/// repo default fills, the hardened base underneath) via [`resolve_mover`] — they are
/// merged, never replace-the-whole-context (ADR-0004 §2). A partial `securityContext`
/// here can therefore only *tighten*; it never drops the hardened `drop:[ALL]`/seccomp.
///
/// Not `Eq`: embeds `k8s-openapi` types (`ResourceRequirements`, `SecurityContext`)
/// which only implement `PartialEq`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MoverSpec {
    /// Resource requests/limits for the mover container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<k8s_openapi::api::core::v1::ResourceRequirements>,
    /// Override the repository's [`CacheDefaults`] for this recipe's movers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheDefaults>,
    /// Security context applied to the mover **container** (`runAsUser`/`runAsGroup`,
    /// capabilities, seccomp, …). Merged field-wise over `moverDefaults.securityContext`
    /// and the hardened base (ADR-0004 §2) — set only the fields you want to change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_context: Option<k8s_openapi::api::core::v1::SecurityContext>,
    /// Security context applied to the mover **pod** — notably `fsGroup`, which makes
    /// a freshly-provisioned volume group-writable so an unprivileged
    /// (`runAsUser != 0`) mover can populate it on **restore** without root. Distinct
    /// from the container-level [`MoverSpec::security_context`]; a pod-level
    /// `runAsUser: 0` / `runAsNonRoot: false` here is still gated as privileged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_security_context: Option<k8s_openapi::api::core::v1::PodSecurityContext>,
    /// Opt-in, namespace-gated; preserves UID/GID on restore. ADR §4.11/§G16.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub privileged_mode: Option<bool>,
    /// Opt-in: copy security context from a live workload pod instead of setting an
    /// explicit `securityContext`/`podSecurityContext` (mutually exclusive with both).
    /// Either name the workload by label (`workloadSelector`) or, on a **backup**
    /// source, auto-derive it from the PVC being backed up (`pvcConsumer`). ADR §4.11.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherit_security_context_from: Option<InheritSecurityContextFrom>,
    /// Per-recipe override of `moverDefaults.ttlSecondsAfterFinished` — the
    /// `Job.spec.ttlSecondsAfterFinished` for this recipe's mover Jobs so finished
    /// backup/restore Jobs self-GC. Recipe wins over the repo default; when neither
    /// is set a built-in default applies ([`DEFAULT_JOB_TTL_SECONDS`]). ADR-0005 §12.
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

/// How the mover pod co-locates with the node a `ReadWriteOnce` source/destination
/// PVC is attached to, to avoid a Kubernetes **Multi-Attach error**.
///
/// A `ReadWriteOnce` (RWO) PVC can only be attached to one node at a time, but it
/// *can* be mounted by multiple pods **on that same node**. When an app pod already
/// holds an RWO PVC on node A and the mover lands on node B, the kubelet on B cannot
/// attach the volume and the mover pod is stuck `Multi-Attach error`. The controller
/// resolves the node the PVC is attached to (consuming pod → PV `nodeAffinity` →
/// `VolumeAttachment`) and pins the mover there so it co-locates with the workload.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
pub enum SourceColocationMode {
    /// Pin an RWO PVC's mover to the node the PVC is attached to **when** that node
    /// is discoverable; otherwise schedule freely (nothing holds the volume, so the
    /// mover can attach it anywhere). `ReadWriteMany`/`ReadOnlyMany` are never pinned.
    /// A `ReadWriteOncePod` PVC that is already held by a live pod fails with guidance
    /// (a second pod cannot mount it even on the same node). The default — fixes the
    /// Multi-Attach error with no configuration.
    #[default]
    Auto,
    /// Like `Auto`, but if an RWO PVC's node cannot be determined, **fail** the run
    /// with an actionable error instead of scheduling freely. Use when an RWO source
    /// must never be backed up from the wrong node.
    Required,
    /// Never compute a node pin; the mover uses only the explicit
    /// `nodeSelector`/`affinity`/`tolerations`. The pre-fix behavior — an escape hatch
    /// for topologies that manage placement themselves.
    Disabled,
}

/// Controls mover/source-PVC node co-location (RWO Multi-Attach avoidance). A
/// sub-object (not a bare enum) so future knobs — e.g. a custom hostname label key
/// for non-standard topologies — slot in without an API break (ADR §4.11).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SourceColocation {
    /// The co-location strategy. Defaults to [`SourceColocationMode::Auto`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<SourceColocationMode>,
}

/// Repository-wide mover defaults inherited by **every** mover the repository spawns —
/// bootstrap, backup, restore, maintenance — overridable per-recipe via `mover`
/// (ADR-0004 §1). Replaces the former `cacheDefaults`: the cache lives at
/// [`MoverDefaults::cache`] now.
///
/// `securityContext`/`podSecurityContext`/`resources`/`cache` resolve by **field-wise
/// merge** (`hardened ⊂ moverDefaults ⊂ recipe`, ADR-0004 §2) via [`resolve_mover`];
/// they are never replaced wholesale, so a repo-wide default composes with a partial
/// per-recipe override. This is the single place a repository defines mover
/// identity/hardening/resources/cache — closing the drift between maintenance and
/// backup/restore movers and the bootstrap-mover gap (a filesystem/NFS repo on a
/// non-`65532`-owned directory becomes bootstrappable with no special-case knob).
///
/// Not `Eq`: embeds `k8s-openapi` types (`SecurityContext`, `PodSecurityContext`,
/// `ResourceRequirements`, `Toleration`, `Affinity`) which are `PartialEq` only.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MoverDefaults {
    /// Container security-context base for every mover, merged *under* the recipe's
    /// `mover.securityContext` and *over* the hardened default ([`hardened_security_context`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_context: Option<SecurityContext>,
    /// Pod security-context base (notably `fsGroup`) for every mover, merged under the
    /// recipe's `mover.podSecurityContext`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_security_context: Option<PodSecurityContext>,
    /// Resource requests/limits base for the mover container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
    /// kopia cache defaults (the former repository `cacheDefaults`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheDefaults>,
    /// Defaults for the deep-verification scratch (restore-test) volume, inherited by
    /// `SnapshotPolicy.spec.verification.deep` unless overridden there. Always
    /// ephemeral (no `mode`, unlike [`MoverDefaults::cache`]); `storageClassName`
    /// applies only when a `capacity` is set.
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
    /// How a mover co-locates with the node its RWO source/destination PVC is
    /// attached to, to avoid a Multi-Attach error. Defaults to
    /// [`SourceColocationMode::Auto`] when unset. ADR §3.7 / RWO multi-attach fix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_colocation: Option<SourceColocation>,
    /// `Job.spec.ttlSecondsAfterFinished` for every mover Job, so finished
    /// backup/restore/maintenance Jobs self-GC (ADR-0005 §12). A recipe's
    /// `mover` can override it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds_after_finished: Option<i64>,
    /// Repository throttle limits (`kopia repository throttle set`) applied by every
    /// mover after it connects, so a run doesn't saturate the link / hammer the
    /// object store. ADR-0005 §13(e).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub throttle: Option<Throttle>,
}

/// Built-in default `Job.spec.ttlSecondsAfterFinished` (1h) applied to a mover Job
/// when neither `moverDefaults.ttlSecondsAfterFinished` nor the recipe's
/// `mover.ttlSecondsAfterFinished` sets one, so finished backup/restore Jobs and
/// their pods self-GC instead of lingering (ADR-0005 §12).
pub const DEFAULT_JOB_TTL_SECONDS: i64 = 3600;

/// Repository-wide throttling for a mover's kopia connection (ADR-0005 §13(e)).
/// Each `None` leaves kopia's current limit. Maps to `kopia repository throttle set`.
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

/// Resolve the effective mover configuration via the 3-layer field-wise merge
/// `hardened ⊂ moverDefaults ⊂ recipe` (ADR-0004 §1/§2).
///
/// - `defaults`: the repository's `moverDefaults` (None when the repo sets none).
/// - `recipe_sc`/`recipe_psc`: the recipe's **effective** container/pod context — the
///   explicit `mover.securityContext`/`podSecurityContext`, OR the context the controller
///   resolved from `inheritSecurityContextFrom`. Inheritance is mutually exclusive with
///   explicit (webhook-enforced), so at most one is `Some`. Inherited context enters here
///   as the *recipe layer*, NOT a whole-chain replacement — so the hardened base +
///   `moverDefaults` still supply `drop:[ALL]`/seccomp and an inherited partial context
///   can only tighten.
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
    let hardened = hardened_security_context();
    // hardened ⊂ moverDefaults.securityContext
    let sc_base = match defaults.and_then(|d| d.security_context.as_ref()) {
        Some(d_sc) => merge_security_context(&hardened, d_sc),
        None => hardened,
    };
    // (hardened ⊂ moverDefaults) ⊂ recipe.securityContext
    let security_context = match recipe_sc {
        Some(r) => merge_security_context(&sc_base, r),
        None => sc_base,
    };
    // Pod context resolves identically to the container one (ADR-0004 §2): a hardened
    // base (notably the `fsGroup` that makes the cache writable) overlaid by
    // moverDefaults then the recipe. Always `Some` so every mover pod — bootstrap,
    // backup, restore, maintenance, verification, replication — carries the same
    // hardened fsGroup unless explicitly overridden.
    let hardened_psc = hardened_pod_security_context();
    // hardened ⊂ moverDefaults.podSecurityContext
    let psc_base = match defaults.and_then(|d| d.pod_security_context.as_ref()) {
        Some(d_psc) => merge_pod_security_context(&hardened_psc, d_psc),
        None => hardened_psc,
    };
    // (hardened ⊂ moverDefaults) ⊂ recipe.podSecurityContext
    let pod_security_context = Some(match recipe_psc {
        Some(r) => merge_pod_security_context(&psc_base, r),
        None => psc_base,
    });
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

/// Selects workload pods by label. Reuses k8s-openapi `LabelSelector`. ADR §3.3 hooks.
///
/// Not `Eq`: `LabelSelector` only implements `PartialEq`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PodSelector {
    /// Label selector matching the workload pod(s) to read context/hooks from.
    pub pod_selector: LabelSelector,
    /// Which container within the matched pod; absent uses the first/only container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
}

/// Where the mover copies its security context from (instead of an explicit
/// `securityContext`/`podSecurityContext`). **Externally tagged** — exactly one variant,
/// so the source of the inherited identity is unambiguous and a `match` must handle every
/// case (convention #1; NOT a bool + optional selector). Mutually exclusive with explicit
/// contexts (webhook/[`crate::validate::validate_mover`]-enforced). The resolved context
/// enters [`resolve_mover`] as the *recipe layer*, so the hardened base still applies.
///
/// Not `Eq`: `WorkloadSelector` embeds [`PodSelector`] → `LabelSelector` (`PartialEq` only).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum InheritSecurityContextFrom {
    /// Match the workload pod(s) by an explicit label selector and inherit the chosen
    /// container's `securityContext` plus the pod's `spec.securityContext`. Works for
    /// both backup and restore (restore inherits from the pod that will *read* the data).
    WorkloadSelector(PodSelector),
    /// **Backup sources only.** Auto-derive the workload pod from the PVC this snapshot
    /// backs up: the operator finds the pod(s) mounting the source claim and inherits
    /// their securityContext, so the mover's UID/GID matches the workload *by
    /// construction* — no hand-written selector. Meaningless on a restore (the consuming
    /// pod may not exist yet, exactly like a populator), so restore must use
    /// `workloadSelector`.
    PvcConsumer(PvcConsumerInherit),
}

/// Tuning for [`InheritSecurityContextFrom::PvcConsumer`]. A sub-object (not a bare flag)
/// so future knobs slot in without an API break (ADR §4.11).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PvcConsumerInherit {
    /// Which container within the matched consumer pod to inherit from; absent uses the
    /// first/only container (same semantics as [`PodSelector::container`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
}
