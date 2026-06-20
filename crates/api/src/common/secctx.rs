use k8s_openapi::api::core::v1::{
    Capabilities, PodSecurityContext, ResourceRequirements, SeccompProfile, SecurityContext,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use std::collections::BTreeMap;

/// Whether a mover with the given **effective** container security context (the
/// explicit `securityContext`, or the one resolved from `inheritSecurityContextFrom`),
/// **pod** security context, and `privilegedMode` is privileged. The controller
/// resolves an inherited context to a concrete `SecurityContext` and gates on *that* —
/// so an inherited root context is caught exactly like an explicit one — and inspects
/// the pod-level context too so a pod-level `runAsUser: 0` can't slip past. Pure +
/// exhaustive: the single definition of "privileged" for both the spec-only
/// ([`MoverSpec::requires_privilege`]) and the resolved paths.
pub fn requires_privilege_resolved(
    security_context: Option<&k8s_openapi::api::core::v1::SecurityContext>,
    pod_security_context: Option<&k8s_openapi::api::core::v1::PodSecurityContext>,
    privileged_mode: Option<bool>,
) -> bool {
    privileged_mode == Some(true)
        || security_context.is_some_and(security_context_is_elevated)
        || pod_security_context.is_some_and(pod_security_context_is_elevated)
}

/// Whether a container `SecurityContext` requests privileges beyond a normal
/// unprivileged user (root UID, `privileged`, escalation, added capabilities, or an
/// explicit `runAsNonRoot: false`). Pure helper for [`MoverSpec::requires_privilege`].
pub fn security_context_is_elevated(sc: &k8s_openapi::api::core::v1::SecurityContext) -> bool {
    sc.privileged == Some(true)
        || sc.run_as_user == Some(0)
        || sc.run_as_non_root == Some(false)
        || sc.allow_privilege_escalation == Some(true)
        || sc
            .capabilities
            .as_ref()
            .and_then(|c| c.add.as_ref())
            .is_some_and(|add| !add.is_empty())
}

/// Whether a **pod** `PodSecurityContext` requests root. Pod-level only carries a
/// subset of the container knobs — `runAsUser` / `runAsNonRoot` are the ones that can
/// make the mover root (capabilities/privileged are container-only). `fsGroup` and
/// friends are NOT elevation. Pure helper for [`requires_privilege_resolved`].
pub fn pod_security_context_is_elevated(
    psc: &k8s_openapi::api::core::v1::PodSecurityContext,
) -> bool {
    psc.run_as_user == Some(0) || psc.run_as_non_root == Some(false)
}

/// The **effective** `runAsUser` following kubelet precedence: the container
/// `securityContext.runAsUser` if set, else the pod `securityContext.runAsUser`. `None`
/// when neither pins a UID — the UID is then image-determined (the `USER` line) and
/// unknowable from the spec.
///
/// This is the single definition of effective-UID precedence, shared by
/// [`crate::invariants`] (INV-1, which keys "is root" on this) and
/// [`crate::secctx_compat`] (which keys read-compatibility on it) so the two can never
/// fork. "Is root" is `effective_run_as_user(..) == Some(0)` — never `runAsNonRoot`,
/// which the invariants may flip.
pub fn effective_run_as_user(
    sc: Option<&SecurityContext>,
    psc: Option<&PodSecurityContext>,
) -> Option<i64> {
    sc.and_then(|s| s.run_as_user)
        .or_else(|| psc.and_then(|p| p.run_as_user))
}

/// The restricted-PSA-compatible **hardened** container security context (§4.11/G16):
/// non-root, no privilege escalation, drop ALL caps, seccomp `RuntimeDefault`.
///
/// This is the LOWEST merge layer (ADR-0004 §2): `repo.moverDefaults.securityContext`
/// then the recipe's `mover.securityContext` overlay it **field-wise**, so a partial
/// override can only *tighten* — it never drops `capabilities.drop:[ALL]` /
/// `seccompProfile`. Lives in `api` (not the controller) so the webhook and controller
/// share one definition and both resolve the effective mover context identically.
pub fn hardened_security_context() -> SecurityContext {
    SecurityContext {
        run_as_non_root: Some(true),
        allow_privilege_escalation: Some(false),
        read_only_root_filesystem: Some(false),
        capabilities: Some(Capabilities {
            drop: Some(vec!["ALL".to_string()]),
            add: None,
        }),
        seccomp_profile: Some(SeccompProfile {
            type_: "RuntimeDefault".to_string(),
            localhost_profile: None,
        }),
        ..Default::default()
    }
}

/// The nonroot UID/GID baked into the mover image (`docker/Dockerfile.mover`:
/// `USER 65532:65532`, distroless `nonroot`). The hardened **pod** context defaults
/// `fsGroup` to this so the kubelet group-owns every mounted volume to the gid the
/// mover actually runs as — most importantly the operator-managed kopia cache, which
/// is otherwise created `root:root` on PVC-backed storage and unwritable by the
/// unprivileged mover. Centralized here (the single source of the hardened defaults)
/// so the value can never drift from the image.
pub const MOVER_NONROOT_ID: i64 = 65532;

/// The restricted-PSA-compatible **hardened pod** security context — the pod-level
/// peer of [`hardened_security_context`]. Defaults `fsGroup` to [`MOVER_NONROOT_ID`]
/// so every mover pod's volumes (notably the cache) are writable by the unprivileged
/// mover; `fsGroupChangePolicy: OnRootMismatch` skips the recursive chown when the
/// volume root already matches, so it does not needlessly rewrite ownership on every
/// run.
///
/// Same merge story as the container context (ADR-0004 §2): this is the LOWEST layer,
/// overlaid field-wise by `repo.moverDefaults.podSecurityContext` then the recipe's
/// `mover.podSecurityContext`, so any of `fsGroup`/`runAsUser`/… can be overridden
/// (e.g. a restore that must own files as the app's UID) while unset fields keep the
/// hardened default. Lives in `api` so the webhook and controller resolve it identically.
pub fn hardened_pod_security_context() -> PodSecurityContext {
    PodSecurityContext {
        fs_group: Some(MOVER_NONROOT_ID),
        fs_group_change_policy: Some("OnRootMismatch".to_string()),
        ..Default::default()
    }
}

/// Deep-merge two [`Capabilities`]: each of `add`/`drop` is taken from `over` when set,
/// else from `base`. So an `over` that sets only `add` keeps `base.drop` — an add-only
/// override never silently drops the hardened `drop:[ALL]` (the bug ADR-0004 §2 cites).
pub fn merge_capabilities(base: &Capabilities, over: &Capabilities) -> Capabilities {
    Capabilities {
        add: over.add.clone().or_else(|| base.add.clone()),
        drop: over.drop.clone().or_else(|| base.drop.clone()),
    }
}

/// Field-wise overlay of container [`SecurityContext`] `over` onto `base`: each `Some`
/// field in `over` wins, unset fields inherit `base`; `capabilities` deep-merge via
/// [`merge_capabilities`] (ADR-0004 §2).
///
/// The struct literal is **exhaustive** (no `..base` tail) on purpose: when the pinned
/// k8s-openapi `SecurityContext` gains a field, this stops compiling until the new field
/// is considered — the same discipline as the exhaustive-`match` enum thesis (§5.5).
pub fn merge_security_context(base: &SecurityContext, over: &SecurityContext) -> SecurityContext {
    SecurityContext {
        allow_privilege_escalation: over
            .allow_privilege_escalation
            .or(base.allow_privilege_escalation),
        app_armor_profile: over
            .app_armor_profile
            .clone()
            .or_else(|| base.app_armor_profile.clone()),
        capabilities: match (base.capabilities.as_ref(), over.capabilities.as_ref()) {
            (Some(b), Some(o)) => Some(merge_capabilities(b, o)),
            (b, o) => o.cloned().or_else(|| b.cloned()),
        },
        privileged: over.privileged.or(base.privileged),
        proc_mount: over.proc_mount.clone().or_else(|| base.proc_mount.clone()),
        read_only_root_filesystem: over
            .read_only_root_filesystem
            .or(base.read_only_root_filesystem),
        run_as_group: over.run_as_group.or(base.run_as_group),
        run_as_non_root: over.run_as_non_root.or(base.run_as_non_root),
        run_as_user: over.run_as_user.or(base.run_as_user),
        se_linux_options: over
            .se_linux_options
            .clone()
            .or_else(|| base.se_linux_options.clone()),
        seccomp_profile: over
            .seccomp_profile
            .clone()
            .or_else(|| base.seccomp_profile.clone()),
        windows_options: over
            .windows_options
            .clone()
            .or_else(|| base.windows_options.clone()),
    }
}

/// Field-wise overlay of pod [`PodSecurityContext`] `over` onto `base`. Exhaustive
/// literal for the same reason as [`merge_security_context`].
pub fn merge_pod_security_context(
    base: &PodSecurityContext,
    over: &PodSecurityContext,
) -> PodSecurityContext {
    PodSecurityContext {
        app_armor_profile: over
            .app_armor_profile
            .clone()
            .or_else(|| base.app_armor_profile.clone()),
        fs_group: over.fs_group.or(base.fs_group),
        fs_group_change_policy: over
            .fs_group_change_policy
            .clone()
            .or_else(|| base.fs_group_change_policy.clone()),
        run_as_group: over.run_as_group.or(base.run_as_group),
        run_as_non_root: over.run_as_non_root.or(base.run_as_non_root),
        run_as_user: over.run_as_user.or(base.run_as_user),
        se_linux_change_policy: over
            .se_linux_change_policy
            .clone()
            .or_else(|| base.se_linux_change_policy.clone()),
        se_linux_options: over
            .se_linux_options
            .clone()
            .or_else(|| base.se_linux_options.clone()),
        seccomp_profile: over
            .seccomp_profile
            .clone()
            .or_else(|| base.seccomp_profile.clone()),
        supplemental_groups: over
            .supplemental_groups
            .clone()
            .or_else(|| base.supplemental_groups.clone()),
        supplemental_groups_policy: over
            .supplemental_groups_policy
            .clone()
            .or_else(|| base.supplemental_groups_policy.clone()),
        sysctls: over.sysctls.clone().or_else(|| base.sysctls.clone()),
        windows_options: over
            .windows_options
            .clone()
            .or_else(|| base.windows_options.clone()),
    }
}

/// Per-key merge of two `limits`/`requests` quantity maps: `over` keys win, `base` keys
/// fill. Returns `None` only when both are absent.
fn merge_quantity_map(
    base: Option<&BTreeMap<String, Quantity>>,
    over: Option<&BTreeMap<String, Quantity>>,
) -> Option<BTreeMap<String, Quantity>> {
    match (base, over) {
        (None, None) => None,
        (Some(b), None) => Some(b.clone()),
        (None, Some(o)) => Some(o.clone()),
        (Some(b), Some(o)) => {
            let mut merged = b.clone();
            for (k, v) in o {
                merged.insert(k.clone(), v.clone());
            }
            Some(merged)
        }
    }
}

/// Field-wise overlay of [`ResourceRequirements`]: `limits`/`requests` merge per-key
/// (via `merge_quantity_map`); `claims` is taken from `over` when set, else `base`.
pub fn merge_resources(
    base: &ResourceRequirements,
    over: &ResourceRequirements,
) -> ResourceRequirements {
    ResourceRequirements {
        claims: over.claims.clone().or_else(|| base.claims.clone()),
        limits: merge_quantity_map(base.limits.as_ref(), over.limits.as_ref()),
        requests: merge_quantity_map(base.requests.as_ref(), over.requests.as_ref()),
    }
}

/// `Option`-aware [`merge_security_context`] (handles the four `None`/`Some` cases).
pub fn merge_security_context_opt(
    base: Option<&SecurityContext>,
    over: Option<&SecurityContext>,
) -> Option<SecurityContext> {
    match (base, over) {
        (None, None) => None,
        (Some(b), None) => Some(b.clone()),
        (None, Some(o)) => Some(o.clone()),
        (Some(b), Some(o)) => Some(merge_security_context(b, o)),
    }
}

/// `Option`-aware [`merge_pod_security_context`].
pub fn merge_pod_security_context_opt(
    base: Option<&PodSecurityContext>,
    over: Option<&PodSecurityContext>,
) -> Option<PodSecurityContext> {
    match (base, over) {
        (None, None) => None,
        (Some(b), None) => Some(b.clone()),
        (None, Some(o)) => Some(o.clone()),
        (Some(b), Some(o)) => Some(merge_pod_security_context(b, o)),
    }
}

/// `Option`-aware [`merge_resources`].
pub fn merge_resources_opt(
    base: Option<&ResourceRequirements>,
    over: Option<&ResourceRequirements>,
) -> Option<ResourceRequirements> {
    match (base, over) {
        (None, None) => None,
        (Some(b), None) => Some(b.clone()),
        (None, Some(o)) => Some(o.clone()),
        (Some(b), Some(o)) => Some(merge_resources(b, o)),
    }
}
