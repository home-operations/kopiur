//! Pure work-spec / Job builders for the `Snapshot` reconciler.
//!
//! These translate a resolved recipe + repository into the mover wire types
//! (work spec, identity, labels, limits) and the actionable messages the
//! reconcile core in [`super`] surfaces. No `ctx`, no kube IO, no `async`.

use std::collections::BTreeMap;

use kopiur_api::common::ResolvedIdentity as ApiResolvedIdentity;
use kopiur_api::{Origin, Snapshot, SnapshotPolicy};
use kopiur_mover::workspec::{
    MoverOptions, MoverWorkSpec, Operation, RepositoryConnect, ResolvedIdentity as MoverIdentity,
    SnapshotAnchor, SnapshotOp, TargetRef,
};
use kube::ResourceExt;

use crate::consts::{API_VERSION, CONFIG_LABEL, ORIGIN_LABEL};
use crate::error::{Error, Result};
use crate::io::{self, ResolvedRepository};
use crate::jobs::{CredsEnvFrom, JobLimits, VolumeMountSpec};

/// Build everything a backup run needs: the work spec, the source volume mount
/// (PVC or inline NFS), the repo volume mount (filesystem only), and the
/// credentials `envFrom` entries.
pub(super) type SnapshotRun<'a> = (
    MoverWorkSpec,
    Option<VolumeMountSpec>,
    Option<VolumeMountSpec>,
    Vec<CredsEnvFrom>,
);
pub(super) fn build_backup_run(
    backup: &Snapshot,
    config: &SnapshotPolicy,
    repo: &ResolvedRepository,
    namespace: &str,
    _name: &str,
) -> Result<SnapshotRun<'static>> {
    let pin = backup.spec.source.as_ref();
    let identity = resolve_identity_for(config, namespace, repo.identity_defaults.as_ref(), pin)?;

    // Which source this run covers: the pinned one for a fanned-out child of a
    // `pvcSelector` expansion, else `sources[0]` (byte-for-byte the old path).
    let eff = crate::expand::effective_source(config, pin)
        .map_err(|e| Error::Validation(e.to_string()))?;
    let source = config
        .spec
        .sources
        .get(eff.index)
        .ok_or_else(|| Error::Invariant("SnapshotPolicy has no sources".into()))?;
    let strategy = crate::expand::strategy_for(source);

    // Read-only unless the recipe opts out. kopia only reads the source, so the mount
    // is read-only by default; `readOnly: false` exists solely so the kubelet will
    // apply `fsGroup` (it skips its recursive chgrp on a read-only mount). Admission
    // rejects the combinations where that cannot work or would rewrite live data
    // unacknowledged — see `validate_source`/`validate_backup_config`.
    let read_only = eff.read_only;

    // The mover snapshots whatever is mounted at `source_path`, so the mount path
    // and the kopia source path are the same. PVC: `/pvc/<name>` by default (or
    // `/pvc/<ns>/<name>` under `sourcePathStrategy: PvcNamespacedName` for a
    // selector source); NFS: the export path by default; either overridable by
    // `sourcePathOverride`.
    let (source_path, source_volume) = match (&eff.pvc, &source.nfs) {
        (Some(pvc), None) => {
            let path = eff
                .kopia_source_path(strategy)
                .unwrap_or_else(|| format!("/pvc/{}", pvc.name));
            (
                path.clone(),
                VolumeMountSpec::pvc(pvc.name.clone(), path, read_only),
            )
        }
        (None, Some(nfs)) => {
            // The export's server-side path (`nfs.path`) is what the volume is
            // mounted FROM; it is NOT necessarily a valid in-container mount
            // path. An NFSv4 pseudo-root export ("/") must not be mounted at "/"
            // in the container — that mounts over the rootfs and the pod fails to
            // start. Remap a "/" target to a safe path; kopia snapshots there.
            let requested = source
                .source_path_override
                .clone()
                .unwrap_or_else(|| nfs.path.clone());
            let mount_path = if requested == "/" {
                crate::consts::NFS_SOURCE_MOUNT_PATH.to_string()
            } else {
                requested
            };
            (
                mount_path.clone(),
                VolumeMountSpec::nfs(nfs.server.clone(), nfs.path.clone(), mount_path, read_only),
            )
        }
        // A `pvcSelector` source reaches here ONLY when this Snapshot carries no
        // `spec.source` pin — i.e. someone applied a bare `policyRef` Snapshot
        // against a selector policy, or edited a `pvc:` policy into a selector
        // after its Snapshots existed (which admission never sees). Refuse with
        // the fix rather than the old `invariant violated … please report it`,
        // which is what #346 actually looked like to users.
        (None, None) if source.pvc_selector.is_some() => {
            return Err(Error::Validation(format!(
                "SnapshotPolicy `{}` source #{} is a `pvcSelector`, which expands to one Snapshot                  per matched PVC — but this Snapshot carries no `spec.source` saying which PVC it                  covers, so there is nothing unambiguous to back up. Create it with `kubectl                  kopiur snapshot now --policy {0}` or let a SnapshotSchedule fire it; both expand                  the selector for you.",
                config.name_any(),
                eff.index,
            )));
        }
        // Genuinely mutually-exclusive/empty shapes, which admission does reject.
        _ => {
            return Err(Error::Invariant(
                "backup mover path requires exactly one of source.pvc or source.nfs".into(),
            ));
        }
    };

    let creds_secrets = io::plain_creds(io::mover_creds_secrets(&repo.backend, &repo.encryption));

    // Effective kopia cache budgets: the repository's cacheDefaults overlaid by this
    // recipe's mover.cache (ADR §3.1).
    let cache = crate::cache::cache_tuning(
        crate::cache::effective_cache(
            repo,
            config.spec.mover.as_ref().and_then(|m| m.cache.as_ref()),
        )
        .as_ref(),
    );
    let work_spec = MoverWorkSpec {
        version: 1,
        operation: Operation::Snapshot(SnapshotOp {
            source_path: source_path.clone(),
            tags: tags_for(backup, config),
            // Flattened SnapshotPolicy policy knobs → kopia `policy set` flags
            // (compression / files / errorHandling / upload / extraArgs). Wired so
            // these never stay inert (ADR-0005 §13(b)/§13(f), ADR-0004 §4b).
            policy: policy_args_for(config),
            // `snapshot create` argv flags (M4 flag sweep, issue #216 category
            // sweep) — NOT `policy set` knobs, so they ride here rather than
            // `policy_args_for`'s `PolicyArgsSpec`. `failFast`/`limitMb` are
            // the recipe's (`SnapshotPolicy`); `description` is this run's
            // (`Snapshot.spec`), matching the recipe/invocation split.
            fail_fast: config
                .spec
                .error_handling
                .as_ref()
                .and_then(|e| e.fail_fast.then_some(true)),
            upload_limit_mb: config.spec.upload.as_ref().and_then(|u| u.limit_mb),
            description: backup.spec.description.clone(),
        }),
        identity,
        repository: repository_connect(repo)?,
        target_ref: TargetRef {
            api_version: API_VERSION.to_string(),
            kind: "Snapshot".to_string(),
            name: _name.to_string(),
            namespace: namespace.to_string(),
        },
        // Observability only: the CONTROLLER executes hooks around this Job
        // (ADR §4.8); the summary lets the mover/work-spec show what ran.
        hook_plan: hook_plan_for(config),
        options: MoverOptions::default(),
        cache,
        // Repository-wide throttle (moverDefaults.throttle, ADR-0005 §13(e)).
        throttle: crate::io::throttle_spec(repo.mover_defaults.as_ref()),
    };

    let source_volume = Some(source_volume);
    let repo_volume =
        io::filesystem_repo_mount_source(&repo.backend).map(|source| VolumeMountSpec {
            source,
            mount_path: io::filesystem_repo_path(&repo.backend).unwrap_or_default(),
            read_only: false,
        });

    Ok((work_spec, source_volume, repo_volume, creds_secrets))
}

/// Stable identity anchors for a Snapshot's kopia manifest, read from its
/// recorded status. kopia rewrites a snapshot's manifest id on pin, so the pin
/// and delete movers re-resolve the live id by these anchors (source path +
/// start time, both stable across the rewrite) — AND, when recorded,
/// username/hostname, so the re-resolve can never cross-match a different
/// identity's snapshot at the same path (the same PVC subpath repeats across
/// namespaces, and, in a shared repository, across clusters). Empty when the
/// Snapshot has no recorded identity/timing yet (the mover then falls back to
/// id-only / path-only behavior).
pub(super) fn snapshot_anchor(backup: &Snapshot) -> SnapshotAnchor {
    let status = backup.status.as_ref();
    let identity = status
        .and_then(|s| s.snapshot.as_ref())
        .map(|s| &s.identity);
    SnapshotAnchor {
        source_path: identity
            .and_then(|i| i.source_path.clone())
            .unwrap_or_default(),
        start_time: status
            .and_then(|s| s.timing.as_ref())
            .and_then(|t| t.start_time.clone()),
        username: identity.map(|i| i.username.clone()),
        hostname: identity.map(|i| i.hostname.clone()),
    }
}

/// Whether a `kopia snapshot list` `entry` IS the snapshot for `identity`:
/// source path AND username/hostname must all match. Path alone is NOT
/// authoritative once two sources share it — the same PVC subpath repeats
/// across namespaces, and, in a shared repository, across clusters — so
/// identity disambiguates. Pulled out of `resolve_succeeded_snapshot` (the
/// filesystem-repo in-process resolution) so the predicate is unit-testable
/// without a live kopia connection.
pub(super) fn matches_snapshot_identity(
    entry: &kopiur_kopia::SnapshotListEntry,
    identity: &MoverIdentity,
) -> bool {
    entry.source.path == identity.source_path
        && entry.source.user_name == identity.username
        && entry.source.host == identity.hostname
}

/// The hook plan summary carried on the work spec (`<index>:<form>` per hook) —
/// pure observability; execution lives in [`crate::hooks`].
fn hook_plan_for(config: &SnapshotPolicy) -> kopiur_mover::workspec::HookPlanSummary {
    let summarize = |hooks: &[kopiur_api::Hook]| {
        hooks
            .iter()
            .enumerate()
            .map(|(i, h)| format!("{i}:{}", h.kind_str()))
            .collect()
    };
    match &config.spec.hooks {
        Some(h) => kopiur_mover::workspec::HookPlanSummary {
            pre: summarize(&h.before_snapshot),
            post: summarize(&h.after_snapshot),
        },
        None => Default::default(),
    }
}

/// Resolve identity from a `SnapshotPolicy` (overrides + the repository's CEL
/// `identityDefaults`) into the mover wire identity. Reuses
/// `api::identity::resolve_identity` (the tested kernel). `defaults` is the
/// referenced repository's `identityDefaults` (ADR-0004 §5) — `Repository` or
/// `ClusterRepository`, whichever `config.spec.repository` targets; `None` when
/// it has none set.
pub(super) fn resolve_identity_for(
    config: &SnapshotPolicy,
    namespace: &str,
    defaults: Option<&kopiur_api::IdentityDefaults>,
    pin: Option<&kopiur_api::SnapshotSourceRef>,
) -> Result<MoverIdentity> {
    // `pin` is the fanned-out child's `spec.source`. Without it this resolved
    // `sources.first()`, which for a `pvcSelector` policy has no `pvc` at all —
    // so every child of an expansion would have resolved the SAME path-less
    // identity and written into one shared kopia source (#346).
    let eff = crate::expand::effective_source(config, pin)
        .map_err(|e| Error::Validation(e.to_string()))?;
    let strategy = config
        .spec
        .sources
        .get(eff.index)
        .map(crate::expand::strategy_for)
        .unwrap_or_default();
    // The resolved kopia path, honoring `sourcePathStrategy` for selector
    // sources. Passed as the override so the identity kernel uses it verbatim
    // rather than re-deriving `/pvc/<name>` from the PVC name alone.
    let resolved_path = eff.kopia_source_path(strategy);
    let pvc_name = eff.pvc.as_ref().map(|p| p.name.clone());
    // A non-PVC NFS source supplies the sourcePath default (the export path).
    let nfs_source_path = eff.nfs_path.clone();
    let source_path_override = resolved_path;
    let inputs = kopiur_api::IdentityInputs {
        object_name: &config.name_any(),
        namespace,
        overrides: config.spec.identity.as_ref(),
        defaults,
        labels: config.metadata.labels.as_ref(),
        annotations: config.metadata.annotations.as_ref(),
        pvc_name: pvc_name.as_deref(),
        default_source_path: nfs_source_path.as_deref(),
        source_path_override: source_path_override.as_deref(),
    };
    let resolved: ApiResolvedIdentity =
        kopiur_api::resolve_identity(&inputs).map_err(|e| Error::Validation(e.to_string()))?;
    Ok(MoverIdentity {
        username: resolved.username,
        hostname: resolved.hostname,
        source_path: resolved.source_path.unwrap_or_else(|| "/data".to_string()),
    })
}

/// Public wrapper so the restore reconciler can reuse the backend mapping.
pub(crate) fn repository_connect_pub(repo: &ResolvedRepository) -> Result<RepositoryConnect> {
    repository_connect(repo)
}

/// Map a `Repository`'s backend to the mover's `RepositoryConnect`.
///
/// Exhaustive over every CRD `Backend` variant — a new backend cannot compile
/// until it is wired through to the mover. Credentials never appear here; they
/// flow to the mover Job as env vars from the referenced Secret (ADR §4.10).
pub(super) fn repository_connect(repo: &ResolvedRepository) -> Result<RepositoryConnect> {
    // `ca_bundle_pem` was resolved once by `io::resolve_repository_ref` — the
    // single source — so every Snapshot/Restore/pin/snapdel work spec built
    // through here carries the private-CA trust bundle without a per-site
    // ConfigMap read.
    Ok(backend_to_repository_connect(
        &repo.backend,
        repo.ca_bundle_pem.clone(),
    ))
}

// The pure `Backend -> RepositoryConnect` translation (and its exhaustive
// every-variant test) moved to `kopiur_mover::repo_meta` so the kubectl-plugin
// browse spawner can use it without a controller dependency. Re-exported so
// the sibling reconcilers keep importing it from `crate::snapshot`.
pub(crate) use kopiur_mover::repo_meta::backend_to_repository_connect;

/// Actionable message for a backup refused because its repository is `ReadOnly`
/// (§11): what / why / how-to-fix. Pure so the text is unit-asserted.
pub(super) fn readonly_backup_message(repo_name: &str) -> String {
    format!(
        "refusing to create a backup: repository `{repo_name}` is `mode: ReadOnly` (ADR-0005 §11), \
         which serves restores only. Set the repository's `spec.mode: ReadWrite` to allow backups, \
         or target a different repository."
    )
}

/// Message for a backup held in `Pending` because its repository is not `Ready`
/// (backend unreachable). Pure so the text is unit-asserted.
pub(super) fn repository_not_ready_message(repo_name: &str) -> String {
    format!(
        "waiting for repository `{repo_name}` to become `Ready` before launching the backup \
         (its backend is unreachable); the backup proceeds once the repository reconnects."
    )
}

/// Snapshot tags for a run: the user's `Snapshot.spec.tags` (invalid keys
/// defensively SKIPPED with a warning — a stored pre-validator object must
/// never fail its backup over a tag), then the reserved operator tags inserted
/// LAST so they always win. Admission ([`kopiur_api::validate::validate_snapshot_tags`])
/// rejects the same keys on new objects; this skip is the defensive layer for
/// already-persisted ones.
pub(super) fn tags_for(backup: &Snapshot, config: &SnapshotPolicy) -> BTreeMap<String, String> {
    let mut tags = BTreeMap::new();
    if let Some(user) = &backup.spec.tags {
        for (key, value) in user {
            if tags.len() >= kopiur_api::validate::MAX_SNAPSHOT_TAGS {
                tracing::warn!(
                    backup = %backup.name_any(),
                    dropped = user.len() - tags.len(),
                    "Snapshot.spec.tags exceeds the {} tag bound; the remainder is skipped",
                    kopiur_api::validate::MAX_SNAPSHOT_TAGS
                );
                break;
            }
            match kopiur_api::validate::snapshot_tag_error(key, value) {
                None => {
                    tags.insert(key.clone(), value.clone());
                }
                Some(reason) => tracing::warn!(
                    backup = %backup.name_any(),
                    tag = %key,
                    "skipping invalid Snapshot.spec.tags entry (stored pre-validation): {reason}"
                ),
            }
        }
    }
    // Reserved operator tags go in LAST so they always win over user tags.
    // The legacy CLI string `kopiur:config:<name>` keeps being written
    // byte-for-byte (kopia stores it as manifest key `tag:kopiur`, value
    // `config:<name>` — the first-colon split); tooling depends on that shape.
    tags.insert("kopiur:config".to_string(), config.name_any());
    tags
}

/// The metadata recorded on the kopia snapshot (the `kopiur-meta` tag) for one
/// run: the RESOLVED effective mover identity plus its provenance. Pure — the
/// same value feeds both the tag and the launch `status.recorded` patch, so the
/// two can never diverge.
///
/// `src` derivation:
/// - `Inherited` iff the inherited layer pinned an identity with a concrete UID
///   AND that UID survived the merge as the resolved effective UID — only then
///   did the identity actually track the workload.
/// - else `Explicit` iff the recipe's explicit `mover.securityContext`/
///   `podSecurityContext` pins the winning effective UID (this covers the
///   inherit-Fallback-proceeded-on-explicit case).
/// - else `Defaults` (moverDefaults/hardened pinned it, or nothing did — then
///   `uid` is `None` and the mover's UID was image-determined).
pub(super) fn recorded_meta(
    resolved: &kopiur_api::common::ResolvedMover,
    outcome: &crate::io::InheritOutcome,
    explicit: Option<&kopiur_api::common::MoverSpec>,
) -> kopiur_api::RecordedSnapshotMeta {
    use crate::io::InheritOutcome;
    use kopiur_api::common::{effective_run_as_group, effective_run_as_user};
    use kopiur_api::{KOPIUR_META_SCHEMA_V1, RecordedSnapshotMeta, RecordedSrc};

    let uid = effective_run_as_user(
        Some(&resolved.security_context),
        resolved.pod_security_context.as_ref(),
    );
    let gid = effective_run_as_group(
        Some(&resolved.security_context),
        resolved.pod_security_context.as_ref(),
    );
    let fs_group = resolved
        .pod_security_context
        .as_ref()
        .and_then(|p| p.fs_group);

    // Did the inherited layer's own pinned UID survive as the resolved one?
    // Exhaustive over the outcome so a new variant must be classified here.
    let inherited_won = match outcome {
        InheritOutcome::Inherited {
            pins_identity: true,
            uid: Some(u),
            ..
        } => uid == Some(*u),
        InheritOutcome::Inherited { .. }
        | InheritOutcome::Fallback { .. }
        | InheritOutcome::NotRequested => false,
        // Restore-only outcome (`inheritSecurityContextFrom.snapshot`); a backup —
        // the only producer of recorded metadata — can never carry it (the variant
        // is admission-rejected on SnapshotPolicy). Classified defensively as
        // not-workload-tracked: replaying a recorded identity is NOT reading the
        // live workload, so it must never be re-recorded as `src: inherited`.
        InheritOutcome::InheritedFromSnapshot { .. } => false,
    };
    // Does the recipe's explicit context pin the winning effective UID? Covers
    // the Fallback-proceeded-on-explicit case (the explicit context IS the run's
    // identity source then).
    let explicit_uid = explicit.and_then(|m| {
        effective_run_as_user(m.security_context.as_ref(), m.pod_security_context.as_ref())
    });
    let explicit_won = explicit_uid.is_some() && explicit_uid == uid;

    let src = if inherited_won {
        RecordedSrc::Inherited
    } else if explicit_won {
        RecordedSrc::Explicit
    } else {
        RecordedSrc::Defaults
    };
    RecordedSnapshotMeta {
        schema: KOPIUR_META_SCHEMA_V1,
        src,
        uid,
        gid,
        fs_group,
    }
}

/// Resolve the kopia `policy set` knobs the mover applies before snapshotting
/// (compression / files / errorHandling / upload / extraArgs). Delegates to the
/// single tested mapping so these flattened `SnapshotPolicy` fields are never
/// inert (ADR-0005 §13(b)/§13(f), ADR-0004 §4b).
pub(super) fn policy_args_for(config: &SnapshotPolicy) -> kopiur_mover::workspec::PolicyArgsSpec {
    kopiur_mover::workspec::PolicyArgsSpec::from_policy(&config.spec)
}

/// Labels applied to the mover Job/ConfigMap and any child objects.
pub(super) fn run_labels(config: &SnapshotPolicy, origin: Origin) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    labels.insert(ORIGIN_LABEL.to_string(), origin_str(origin).to_string());
    labels.insert(CONFIG_LABEL.to_string(), config.name_any());
    labels
}

/// The label set a BACKUP mover `Job` (and its pod) carries, before the mover
/// identity decorates it: [`run_labels`] plus the repository-pool membership
/// label, since a backup counts toward its repository's
/// `spec.concurrency.maxConcurrentJobs`.
///
/// `repo_ref` MUST be the NORMALIZED identity of the repository that actually
/// resolved (`io::ResolvedRepository::repository_ref`), never a raw spec ref —
/// a `Repository` ref with `namespace: None` hashes `repository:/name` and
/// would split one repository's pool in two.
///
/// Extracted from the reconciler so the exact set is assertable without a
/// cluster (the caller only adds `MoverIdentity::decorate_labels` on top).
pub(super) fn backup_job_labels(
    config: &SnapshotPolicy,
    origin: Origin,
    repo_ref: &kopiur_api::common::RepositoryRef,
) -> BTreeMap<String, String> {
    let mut labels = run_labels(config, origin);
    labels.extend(crate::pool::repo_pool_label(
        crate::pool::MoverJobKind::Backup,
        repo_ref,
    ));
    labels
}

/// The label set a batched snapshot-DELETE mover `Job` carries: managed-by
/// (added by `build_job`) plus the batch op and the repo hash, so the
/// dispatcher can LIST one repository's batch Jobs.
///
/// Notably it carries [`crate::consts::DELETE_REPO_LABEL`] — the same
/// `repo_label` value the pool label uses — but NOT
/// [`kopiur_api::consts::REPO_POOL_LABEL`]: a batch delete is already
/// single-flighted per repository by its own dispatcher and it REDUCES
/// repository load, so holding it behind backups would grow the backlog it
/// exists to drain ([`crate::pool::counts_toward_repo_pool`]).
pub(super) fn batch_job_labels(
    repo_ref: &kopiur_api::common::RepositoryRef,
) -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            crate::consts::OP_LABEL.to_string(),
            crate::consts::OP_SNAPSHOT_DELETE_BATCH.to_string(),
        ),
        (
            crate::consts::DELETE_REPO_LABEL.to_string(),
            crate::naming::repo_label(repo_ref),
        ),
    ])
}

pub(super) fn origin_str(origin: Origin) -> &'static str {
    match origin {
        Origin::Scheduled => "scheduled",
        Origin::Manual => "manual",
        Origin::Discovered => "discovered",
        Origin::Adopted => "adopted",
        Origin::Replicated => "replicated",
    }
}

/// Job limits from the backup's `failurePolicy`, falling back to ADR defaults.
pub(super) fn job_limits(backup: &Snapshot) -> JobLimits {
    match &backup.spec.failure_policy {
        Some(fp) => JobLimits {
            backoff_limit: fp.backoff_limit.unwrap_or(2),
            active_deadline_seconds: fp.active_deadline_seconds,
            ..JobLimits::default()
        },
        None => JobLimits::default(),
    }
}

/// Actionable failure message for a mover pod wedged in a non-starting state past the
/// grace window — what failed, why, and how to fix it (the what/why/fix rule).
pub(crate) fn wedged_pod_message(reason: &str, detail: &str, grace_seconds: i64) -> String {
    format!(
        "mover pod stuck ({reason}) for over {grace_seconds}s and cannot start: {detail}. Usual \
         causes: an invalid securityContext for the namespace's Pod Security policy (e.g. \
         inherited root UID, no privileged-mover opt-in), an unavailable image, or an \
         unschedulable source volume. Fix: correct the mover config (securityContext / \
         inheritSecurityContextFrom / image) or the namespace policy, then re-run; tune the \
         window with spec.failurePolicy.podStartupDeadlineSeconds."
    )
}

// The mover imagePullPolicy decision moved to
// `crate::config::effective_mover_pull_policy` (pure, unit-tested there);
// reconcilers reach it via `ctx.mover_pull_policy()`.
