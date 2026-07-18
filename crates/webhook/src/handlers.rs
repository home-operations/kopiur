//! One validating + mutating admission handler per CRD (ADR §5.3, §2.2 principle 8).
//!
//! Each handler follows the same shape, so behavior is consistent and every handler
//! is a thin adapter over the shared `kopiur_api::validate` validators — **no forked
//! validation logic** (SKILL hard-rule 4):
//!
//! 1. Decode the typed spec from the incoming `DynamicObject` (`object.data["spec"]`).
//!    A decode failure denies with a clear message (fail closed).
//! 2. Run the corresponding `validate_*` aggregate. A non-empty `Vec<ValidationError>`
//!    denies with **all** messages joined, so a user sees every problem in one apply.
//! 3. Apply mutating defaults as a JSON patch (RFC 6902) on the `AdmissionResponse`
//!    via `AdmissionResponse::with_patch`.
//! 4. For consumer CRs referencing a `ClusterRepository`, enforce the
//!    `allowedNamespaces` tenancy gate (fail closed) — see [`crate::tenancy`].

use kopiur_api as api;

use api::cluster_repository::ClusterRepositorySpec;
use api::common::{DeletionPolicy, PolicyRef, RepositoryKind, RepositoryRef};
use api::error::ValidationError;
use api::maintenance::MaintenanceSpec;
use api::repository::RepositorySpec;
use api::repository_replication::RepositoryReplicationSpec;
use api::restore::RestoreSpec;
use api::snapshot::{Origin, SnapshotSpec};
use api::snapshot_policy::SnapshotPolicySpec;
use api::snapshot_schedule::SnapshotScheduleSpec;

use crate::error::{AdmissionError, AdmissionResult};
use crate::tenancy::{self, TenancyDecision, TenancyDenial};
use json_patch::{AddOperation, Patch, PatchOperation, jsonptr::PointerBuf};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::Client;
use kube::core::DynamicObject;
use kube::core::admission::{AdmissionRequest, AdmissionResponse, Operation};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// The finalizer that ties a kopia snapshot's lifecycle to its `Snapshot` CR (ADR §4.5).
/// Single definition shared with the controller via `kopiur-api` so the two can't drift.
pub use api::consts::SNAPSHOT_CLEANUP_FINALIZER;

/// Dispatch a decoded `AdmissionRequest` to the handler for its `kind`.
///
/// Unknown kinds are allowed (the webhook only registers for kopiur.home-operations.com kinds; an
/// unexpected kind reaching us is not a reason to block the cluster). The `client`
/// is used only for `ClusterRepository` tenancy resolution; `None` forces those
/// checks to fail closed.
///
/// This is the **single deny choke point** (ADR §5.5): every handler returns
/// `Result<AdmissionResponse, AdmissionError>`, and only this `match` turns a
/// typed [`AdmissionError`] into `AdmissionResponse::deny` — grep for `.deny(`
/// and this is the one production call. The denial is logged with its stable
/// [`AdmissionError::reason`] label.
pub async fn dispatch(
    req: &AdmissionRequest<DynamicObject>,
    client: Option<&Client>,
) -> AdmissionResponse {
    let base = AdmissionResponse::from(req);
    let result = match req.kind.kind.as_str() {
        "SnapshotPolicy" => handle_snapshot_policy(req, base, client).await,
        "Snapshot" => handle_snapshot(req, base),
        "SnapshotSchedule" => handle_snapshot_schedule(req, base),
        "Restore" => handle_restore(req, base, client).await,
        "Maintenance" => handle_maintenance(req, base, client).await,
        "RepositoryReplication" => handle_repository_replication(req, base, client).await,
        "ClusterRepository" => handle_cluster_repository(req, base, client).await,
        "Repository" => handle_repository(req, base, client).await,
        other => {
            tracing::warn!(
                kind = other,
                "admission request for unregistered kind; allowing"
            );
            Ok(base)
        }
    };
    match result {
        Ok(resp) => resp,
        Err(err) => {
            tracing::info!(
                kind = %req.kind.kind,
                name = %req.name,
                namespace = req.namespace.as_deref().unwrap_or(""),
                reason = err.reason(),
                error = %err,
                "denying admission"
            );
            AdmissionResponse::from(req).deny(err.to_string())
        }
    }
}

// --- decode helpers ---------------------------------------------------------

/// Extract the incoming object from the request, denying if absent.
///
/// Note: a `DynamicObject` splits the wire object into `metadata` (typed
/// [`ObjectMeta`]) and `data` (everything else: `spec`, `status`). `apiVersion`/
/// `kind` land in `types`. So spec lives in `obj.data["spec"]` and labels/finalizers
/// live in `obj.metadata`, NOT in `data`.
fn raw_object(req: &AdmissionRequest<DynamicObject>) -> AdmissionResult<&DynamicObject> {
    match &req.object {
        Some(obj) => Ok(obj),
        None => Err(AdmissionError::MissingObject),
    }
}

/// Deserialize `object.data["spec"]` into a typed spec `T`. A missing `spec`
/// deserializes from `null`/`{}` so specs that are entirely optional (e.g. a
/// discovered `Snapshot`) still decode.
fn decode_spec<T: serde::de::DeserializeOwned>(data: &Value) -> Result<T, serde_json::Error> {
    let spec = data
        .get("spec")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    serde_json::from_value(spec)
}

/// Decode the OLD object's typed spec `T` from an UPDATE admission request, if
/// present. `None` for CREATE (no old object) or when the old object carries no
/// decodable spec. Used by the create-time-immutability checks (ADR-0005 §7), which
/// only run on UPDATE.
fn decode_old_spec<T: serde::de::DeserializeOwned>(
    req: &AdmissionRequest<DynamicObject>,
) -> Option<T> {
    let old = req.old_object.as_ref()?;
    decode_spec(&old.data).ok()
}

/// Decode the OLD object's typed `status` `T` from an UPDATE admission request. The
/// status subresource is part of the stored object the API server sends as
/// `oldObject`, so it is present once the controller has written it. A missing
/// `status` decodes from `{}` (every status field is optional), yielding the default —
/// used by the fork-on-edit guard to read the previously-pinned identity + history.
fn decode_old_status<T: serde::de::DeserializeOwned>(
    req: &AdmissionRequest<DynamicObject>,
) -> Option<T> {
    let old = req.old_object.as_ref()?;
    let status = old
        .data
        .get("status")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    serde_json::from_value(status).ok()
}

/// Apply a JSON patch to a response; a serialization failure is the typed
/// [`AdmissionError::InternalPatch`] (fail closed at the dispatch choke point).
fn with_patch(resp: AdmissionResponse, ops: Vec<PatchOperation>) -> AdmissionResult {
    if ops.is_empty() {
        return Ok(resp);
    }
    resp.with_patch(Patch(ops))
        .map_err(|source| AdmissionError::InternalPatch { source })
}

fn ptr(path: &str) -> PointerBuf {
    PointerBuf::parse(path).expect("static JSON pointer is valid")
}

/// Attach non-blocking admission warnings to an already-allowed response (e.g. the
/// inline-NFS `fsGroup` footgun). Empty input leaves the response untouched so the
/// API server doesn't surface a spurious empty warning list.
fn with_warnings(mut resp: AdmissionResponse, warnings: Vec<String>) -> AdmissionResponse {
    if !warnings.is_empty() {
        resp.warnings = Some(warnings);
    }
    resp
}

/// `metadata.finalizers` may be absent. Build a patch op that appends the snapshot
/// finalizer without clobbering existing finalizers.
///
/// Never (re-)add the finalizer to an object already being deleted
/// (`deletionTimestamp` set): the controller's deletion path PATCHes the object to
/// REMOVE this finalizer, and that PATCH is itself an UPDATE admission — re-adding
/// here would immediately undo the removal, so the snapshot-cleanup finalizer
/// could never clear and the `Snapshot` CR would never be garbage-collected.
fn ensure_finalizer_ops(meta: &ObjectMeta, ops: &mut Vec<PatchOperation>) {
    if meta.deletion_timestamp.is_some() {
        return;
    }
    match &meta.finalizers {
        None => {
            // No finalizers array at all: create it with our finalizer.
            ops.push(PatchOperation::Add(AddOperation {
                path: ptr("/metadata/finalizers"),
                value: json!([SNAPSHOT_CLEANUP_FINALIZER]),
            }));
        }
        Some(existing) => {
            if !existing.iter().any(|f| f == SNAPSHOT_CLEANUP_FINALIZER) {
                // Append to the end of the existing array (RFC 6902 "-" token).
                ops.push(PatchOperation::Add(AddOperation {
                    path: ptr("/metadata/finalizers/-"),
                    value: json!(SNAPSHOT_CLEANUP_FINALIZER),
                }));
            }
        }
    }
}

// --- SnapshotPolicy -----------------------------------------------------------

async fn handle_snapshot_policy(
    req: &AdmissionRequest<DynamicObject>,
    resp: AdmissionResponse,
    client: Option<&Client>,
) -> AdmissionResult {
    let obj = raw_object(req)?;
    let spec: SnapshotPolicySpec =
        decode_spec(&obj.data).map_err(|source| AdmissionError::SpecDecode {
            kind: "SnapshotPolicy",
            source,
        })?;

    let errs = api::validate::validate_backup_config(&spec);
    if !errs.is_empty() {
        return Err(AdmissionError::Invalid(errs));
    }

    // ClusterRepository tenancy (fail closed). namespace comes from the request.
    if let TenancyDecision::Deny(denial) =
        tenancy_for(&spec.repository, req.namespace.as_deref(), client).await
    {
        return Err(denial.into());
    }

    // Identity-collision detection (ADR-0005 §6): reject a SnapshotPolicy whose
    // resolved `username@hostname[:path]` identity collides with an already-admitted
    // policy's identity in the same repository — two recipes must not interleave
    // snapshots into one kopia identity. The IO is best-effort (fails open), so a
    // transient list/get error never wedges an apply.
    if let Some(ns) = req.namespace.as_deref() {
        let name = obj.metadata.name.as_deref().unwrap_or(req.name.as_str());
        if let Some(collision) = crate::identity_collision::check_identity_collision(
            client,
            name,
            ns,
            &spec,
            obj.metadata.labels.as_ref(),
            obj.metadata.annotations.as_ref(),
        )
        .await
        {
            return Err(AdmissionError::Invalid(vec![
                ValidationError::IdentityCollision {
                    identity: collision.identity,
                    conflict: collision.conflict,
                },
            ]));
        }
    }

    // Fork-on-edit guard: on UPDATE, reject a change that would re-identify a policy
    // with existing snapshot history (orphaning the old kopia source) unless it is
    // acknowledged with the allow-identity-change annotation. Reads the old object's
    // pinned identity + history; degrades to allow when it can't decide confidently.
    if req.operation == Operation::Update
        && let (Some(old_spec), Some(old_status)) = (
            decode_old_spec::<SnapshotPolicySpec>(req),
            decode_old_status::<api::snapshot_policy::SnapshotPolicyStatus>(req),
        )
    {
        let name = obj.metadata.name.as_deref().unwrap_or(req.name.as_str());
        let ns = req.namespace.as_deref().unwrap_or_default();
        if let Some(err) = crate::identity_fork::check_identity_fork(
            client,
            name,
            ns,
            &spec,
            obj.metadata.labels.as_ref(),
            obj.metadata.annotations.as_ref(),
            &old_spec,
            &old_status,
        )
        .await
        {
            return Err(AdmissionError::Invalid(vec![err]));
        }
    }

    // Best-effort, non-blocking securityContext-compatibility warning (the earliest surface).
    let warnings = crate::secctx::backup_warnings(
        client,
        req.namespace.as_deref(),
        spec.mover.as_ref(),
        &spec.sources,
    )
    .await;
    Ok(with_warnings(resp, warnings))
}

// --- Snapshot -----------------------------------------------------------------

fn handle_snapshot(
    req: &AdmissionRequest<DynamicObject>,
    resp: AdmissionResponse,
) -> AdmissionResult {
    let obj = raw_object(req)?;
    let spec: SnapshotSpec =
        decode_spec(&obj.data).map_err(|source| AdmissionError::SpecDecode {
            kind: "Snapshot",
            source,
        })?;

    let origin = backup_origin(&obj.metadata, &obj.data);

    let errs = api::validate::validate_backup(&spec, origin);
    if !errs.is_empty() {
        return Err(AdmissionError::Invalid(errs));
    }

    // Manual backups carrying a policyRef to a ClusterRepository are not expressible
    // here (Snapshot.spec has no repository field — it derives from the policyRef), so
    // there is no inline ClusterRepository ref to gate on a Snapshot. Tenancy for the
    // recipe is enforced on the SnapshotPolicy. We still default deletionPolicy and the
    // finalizer.

    let mut ops = Vec::new();

    // Origin-aware default deletionPolicy when absent (ADR §4.5):
    //   discovered → forced Retain; produced (scheduled/manual) → Delete.
    // (SnapshotPolicy.defaultDeletionPolicy inheritance is the controller's job once it
    // resolves the policyRef; the webhook only sets the safe origin-aware default.)
    if spec.deletion_policy.is_none() {
        let default = match origin {
            Origin::Discovered => DeletionPolicy::Retain,
            Origin::Scheduled | Origin::Manual => DeletionPolicy::Delete,
        };
        ops.push(set_spec_field(
            &obj.data,
            "deletionPolicy",
            serde_json::to_value(default).expect("DeletionPolicy serializes"),
        ));
    }

    // Every Snapshot carries the snapshot-cleanup finalizer (ADR §4.5).
    ensure_finalizer_ops(&obj.metadata, &mut ops);

    // Stamp CONFIG_LABEL on CREATE only. Today the label is stamped by the schedule
    // controller and by the CLI's `snapshot now` — but a raw-`kubectl apply`'d manual
    // Snapshot with `spec.policyRef` never gets it, making it invisible to GFS
    // retention, the policy fan-out watch, and the SnapshotPolicy deletion cascade
    // (all of which select a policy's children by this label). Deliberately no
    // controller-side backfill of pre-existing CRs: retro-labeling would make a
    // previously-immortal raw-applied manual Snapshot GFS-prunable — silent data loss.
    if req.operation == Operation::Create
        && let Some(value) = config_label_stamp(
            origin,
            spec.policy_ref.as_ref(),
            obj.metadata
                .namespace
                .as_deref()
                .or(req.namespace.as_deref())
                .unwrap_or_default(),
            obj.metadata.labels.as_ref(),
        )
    {
        ops.push(config_label_op(&obj.metadata, &value));
    }

    with_patch(resp, ops)
}

/// Decide whether a `Snapshot` referencing a `SnapshotPolicy` should have
/// `CONFIG_LABEL` stamped on it, and the value to stamp. Pure (no IO), so it
/// unit-tests without a cluster.
///
/// Fires only when ALL hold:
/// - `origin` is `Manual` or `Scheduled` — never `Discovered`: a catalog-materialized
///   Snapshot never ran through a policy, so a `policyRef` on one (if any) doesn't
///   earn the label.
/// - `policy_ref` is present with a nonempty name.
/// - The ref targets the Snapshot's OWN namespace (absent/empty `namespace`, or equal
///   to `cr_namespace`). The stamped label value is a bare policy name with no
///   namespace component, so a cross-namespace ref must not mint a label that
///   collides with a same-named LOCAL policy.
/// - The label isn't already present, regardless of its value (idempotent: never
///   overwrite an existing value).
fn config_label_stamp(
    origin: Origin,
    policy_ref: Option<&PolicyRef>,
    cr_namespace: &str,
    existing_labels: Option<&BTreeMap<String, String>>,
) -> Option<String> {
    match origin {
        Origin::Discovered => return None,
        Origin::Manual | Origin::Scheduled => {}
    }

    let policy_ref = policy_ref?;
    if policy_ref.name.is_empty() {
        return None;
    }

    if let Some(ns) = policy_ref.namespace.as_deref()
        && !ns.is_empty()
        && ns != cr_namespace
    {
        return None;
    }

    if existing_labels.is_some_and(|labels| labels.contains_key(api::consts::CONFIG_LABEL)) {
        return None;
    }

    Some(policy_ref.name.clone())
}

/// Build the JSON-patch op that stamps `CONFIG_LABEL = value` on a `Snapshot`,
/// mirroring `set_spec_field`'s absent-parent handling: if `metadata.labels` is
/// absent, add the whole map; otherwise add just the key (an RFC 6902 `add` on an
/// existing map sets/creates that one member without clobbering siblings). Only
/// called once `config_label_stamp` has already confirmed the key is absent.
fn config_label_op(meta: &ObjectMeta, value: &str) -> PatchOperation {
    match &meta.labels {
        None => PatchOperation::Add(AddOperation {
            path: PointerBuf::from_tokens(["metadata", "labels"]),
            value: json!({ api::consts::CONFIG_LABEL: value }),
        }),
        Some(_) => PatchOperation::Add(AddOperation {
            path: PointerBuf::from_tokens(["metadata", "labels", api::consts::CONFIG_LABEL]),
            value: json!(value),
        }),
    }
}

/// Resolve a `Snapshot`'s origin from the `kopiur.home-operations.com/origin` label (canonical) or
/// `status.origin`, defaulting to `manual` for user-created backups with no marker.
fn backup_origin(meta: &ObjectMeta, data: &Value) -> Origin {
    let from_label = meta
        .labels
        .as_ref()
        .and_then(|l| l.get(api::consts::ORIGIN_LABEL))
        .map(|s| s.as_str());
    let from_status = data
        .get("status")
        .and_then(|s| s.get("origin"))
        .and_then(|v| v.as_str());
    match from_label.or(from_status) {
        Some("discovered") => Origin::Discovered,
        Some("scheduled") => Origin::Scheduled,
        // A user `kubectl create`-ing a Snapshot with no origin marker is manual.
        _ => Origin::Manual,
    }
}

// --- SnapshotSchedule ---------------------------------------------------------

fn handle_snapshot_schedule(
    req: &AdmissionRequest<DynamicObject>,
    resp: AdmissionResponse,
) -> AdmissionResult {
    let obj = raw_object(req)?;
    let data = &obj.data;
    let spec: SnapshotScheduleSpec =
        decode_spec(data).map_err(|source| AdmissionError::SpecDecode {
            kind: "SnapshotSchedule",
            source,
        })?;

    let errs = api::validate::validate_backup_schedule(&spec);
    if !errs.is_empty() {
        return Err(AdmissionError::Invalid(errs));
    }

    // No spec-mutating defaulting here: `schedule.runOnCreate` (false) and
    // `schedule.concurrencyPolicy` (Forbid) now carry real OpenAPI `default:`s in the
    // CRD schema (ADR-0005 §1), so the apiserver materializes them. The webhook writes
    // no user spec (the status-only-write invariant, ADR-0005 §14(d)) — a write-back
    // into spec makes Argo/Flux perpetually `OutOfSync`.

    // Non-blocking footgun warning for a sub-hourly cadence (issue #249): per-run
    // Snapshot CRs accumulate up to the retention window and each re-reconciles for
    // its whole life, so a sub-hourly schedule with a wide retention can pile up
    // thousands of CRs. A warning, not a rejection — sub-hourly is legitimate.
    let warnings = api::validate::schedule_cr_growth_warning(&spec.schedule.cron)
        .into_iter()
        .collect();
    Ok(with_warnings(resp, warnings))
}

// --- Restore ----------------------------------------------------------------

async fn handle_restore(
    req: &AdmissionRequest<DynamicObject>,
    resp: AdmissionResponse,
    client: Option<&Client>,
) -> AdmissionResult {
    let obj = raw_object(req)?;
    let spec: RestoreSpec =
        decode_spec(&obj.data).map_err(|source| AdmissionError::SpecDecode {
            kind: "Restore",
            source,
        })?;

    let errs = api::validate::validate_restore_spec(&spec);
    if !errs.is_empty() {
        return Err(AdmissionError::Invalid(errs));
    }

    if let Some(repo) = &spec.repository
        && let TenancyDecision::Deny(denial) =
            tenancy_for(repo, req.namespace.as_deref(), client).await
    {
        return Err(denial.into());
    }

    // Best-effort, non-blocking restore-direction securityContext warning.
    let target_pvc = match &spec.target {
        api::restore::RestoreTarget::PvcRef(r) => Some(r.name.as_str()),
        api::restore::RestoreTarget::Pvc(t) => Some(t.name.as_str()),
        api::restore::RestoreTarget::Populator(_) => None,
    };
    let warnings = crate::secctx::restore_warnings(
        client,
        req.namespace.as_deref(),
        spec.mover.as_ref(),
        target_pvc,
    )
    .await;
    Ok(with_warnings(resp, warnings))
}

// --- Maintenance ------------------------------------------------------------

async fn handle_maintenance(
    req: &AdmissionRequest<DynamicObject>,
    resp: AdmissionResponse,
    client: Option<&Client>,
) -> AdmissionResult {
    let obj = raw_object(req)?;
    let spec: MaintenanceSpec =
        decode_spec(&obj.data).map_err(|source| AdmissionError::SpecDecode {
            kind: "Maintenance",
            source,
        })?;

    let errs = api::validate::validate_maintenance(&spec);
    if !errs.is_empty() {
        return Err(AdmissionError::Invalid(errs));
    }

    // The run-now annotations are user input too: refuse garbage at admission
    // (same shared parser the controller uses) so a typo'd timestamp can't
    // reach the reconciler. Objects annotated while the webhook was down are
    // still degraded gracefully controller-side.
    if let Err(message) = api::maintenance::parse_run_annotations(obj.metadata.annotations.as_ref())
    {
        return Err(AdmissionError::Invalid(vec![
            ValidationError::InvalidRunAnnotation { message },
        ]));
    }

    if let TenancyDecision::Deny(denial) =
        tenancy_for(&spec.repository, req.namespace.as_deref(), client).await
    {
        return Err(denial.into());
    }

    Ok(resp)
}

// --- RepositoryReplication --------------------------------------------------

async fn handle_repository_replication(
    req: &AdmissionRequest<DynamicObject>,
    resp: AdmissionResponse,
    client: Option<&Client>,
) -> AdmissionResult {
    let obj = raw_object(req)?;
    let spec: RepositoryReplicationSpec =
        decode_spec(&obj.data).map_err(|source| AdmissionError::SpecDecode {
            kind: "RepositoryReplication",
            source,
        })?;

    let errs = api::validate::validate_repository_replication(&spec);
    if !errs.is_empty() {
        return Err(AdmissionError::Invalid(errs));
    }

    // Tenancy: a ClusterRepository sourceRef is gated against allowedNamespaces.
    if let TenancyDecision::Deny(denial) =
        tenancy_for(&spec.source_ref, req.namespace.as_deref(), client).await
    {
        return Err(denial.into());
    }

    // §13(d): the destination must differ from the source's backend (no self-mirror),
    // and the source/destination auth pair must be safe in one mover pod (a same-kind
    // static/workload-identity mix leaks the static env into the ambient chain).
    // Resolve the source backend via the client and compare. Best-effort — a missing
    // client / unresolvable source skips these checks (the structural validations
    // above already ran), mirroring how tenancy degrades when inputs are unavailable.
    if let Some(client) = client
        && let Some(source_backend) =
            resolve_source_backend(client, &spec.source_ref, req.namespace.as_deref()).await
    {
        if !api::validate::replication_destination_differs(&source_backend, &spec.destination) {
            return Err(AdmissionError::Invalid(vec![
                ValidationError::ReplicationDestinationSameAsSource {
                    backend: spec.destination.kind_str().to_string(),
                },
            ]));
        }
        if let Err(e) = api::validate::validate_replication_auth(&source_backend, &spec.destination)
        {
            return Err(AdmissionError::Invalid(vec![e]));
        }
    }

    Ok(resp)
}

/// Resolve a replication source's backend from its `RepositoryRef` (a namespaced
/// `Repository` or a cluster-scoped `ClusterRepository`). Returns `None` when the
/// repo can't be fetched (so the differs check is skipped rather than guessed).
async fn resolve_source_backend(
    client: &Client,
    source: &RepositoryRef,
    consumer_namespace: Option<&str>,
) -> Option<api::backend::Backend> {
    use kube::Api;
    match source.kind {
        RepositoryKind::Repository => {
            let ns = source.namespace.as_deref().or(consumer_namespace)?;
            let api: Api<api::Repository> = Api::namespaced(client.clone(), ns);
            api.get_opt(&source.name)
                .await
                .ok()
                .flatten()
                .map(|r| r.spec.backend)
        }
        RepositoryKind::ClusterRepository => {
            let api: Api<api::ClusterRepository> = Api::all(client.clone());
            api.get_opt(&source.name)
                .await
                .ok()
                .flatten()
                .map(|r| r.spec.backend)
        }
    }
}

// --- ClusterRepository ------------------------------------------------------

async fn handle_cluster_repository(
    req: &AdmissionRequest<DynamicObject>,
    resp: AdmissionResponse,
    client: Option<&Client>,
) -> AdmissionResult {
    let obj = raw_object(req)?;
    let spec: ClusterRepositorySpec =
        decode_spec(&obj.data).map_err(|source| AdmissionError::SpecDecode {
            kind: "ClusterRepository",
            source,
        })?;

    // Decode the old spec once (UPDATE only) — reused by both the create-time
    // immutability check and the identityDefaults edit guard below.
    let old_spec = (req.operation == Operation::Update)
        .then(|| decode_old_spec::<ClusterRepositorySpec>(req))
        .flatten();

    let mut errs = api::validate::validate_cluster_repository(&spec);
    // Create-time immutability (ADR-0005 §7): on UPDATE, reject changes to
    // `create.{splitter,hash,encryption,ecc}` — kopia bakes them into the repository
    // format. The password Secret reference is intentionally NOT locked (a rename with
    // identical content must pass). CREATE has no old object, so the check is UPDATE-only.
    if let Some(old) = &old_spec {
        errs.extend(api::validate::validate_cluster_repository_immutability(
            old, &spec,
        ));
    }
    if !errs.is_empty() {
        return Err(AdmissionError::Invalid(errs));
    }

    let mut warnings =
        api::validate::repository_warnings(&spec.backend, spec.mover_defaults.as_ref());

    // identityDefaults edit guard (silent fleet-wide re-identification): editing
    // identityDefaults on a live ClusterRepository re-resolves every consumer
    // SnapshotPolicy's identity on its next backup with no per-policy edit to
    // acknowledge it — reject unless the repository carries the
    // allow-identity-change annotation. UPDATE-only (see
    // `identity_repo_edit`'s module doc for the accepted CREATE residual gap:
    // a delete + re-apply has no oldObject to diff, so it bypasses this guard by
    // design). Degrades to allow when there is no client or the consumer LIST
    // fails (fail-open, same posture as the fork/collision guards).
    if let Some(old) = &old_spec {
        let name = obj.metadata.name.as_deref().unwrap_or(req.name.as_str());
        let self_key = crate::identity_collision::repo_key(
            &RepositoryRef {
                kind: RepositoryKind::ClusterRepository,
                name: name.to_string(),
                namespace: None,
            },
            "",
        );
        let outcome = crate::identity_repo_edit::check_repository_identity_change(
            client,
            &self_key,
            old.identity_defaults.as_ref(),
            spec.identity_defaults.as_ref(),
            obj.metadata.annotations.as_ref(),
        )
        .await;
        if let Some(err) = outcome.error {
            return Err(AdmissionError::Invalid(vec![err]));
        }
        if !outcome.consumers.is_empty() {
            // Allowed only because it was acknowledged (see
            // `check_repository_identity_change`): non-empty consumers with no
            // error implies the ack annotation was present.
            warnings.push(format!(
                "identityDefaults change acknowledged: this re-identifies {}",
                api::error::describe_identity_change_consumers(&outcome.consumers)
            ));
        }
    }

    Ok(with_warnings(resp, warnings))
}

// --- Repository -------------------------------------------------------------

async fn handle_repository(
    req: &AdmissionRequest<DynamicObject>,
    resp: AdmissionResponse,
    client: Option<&Client>,
) -> AdmissionResult {
    let obj = raw_object(req)?;
    let spec: RepositorySpec =
        decode_spec(&obj.data).map_err(|source| AdmissionError::SpecDecode {
            kind: "Repository",
            source,
        })?;

    // Decode the old spec once (UPDATE only) — reused by both the create-time
    // immutability check and the identityDefaults edit guard below.
    let old_spec = (req.operation == Operation::Update)
        .then(|| decode_old_spec::<RepositorySpec>(req))
        .flatten();

    let mut errs = api::validate::validate_repository(&spec);
    // Create-time immutability (ADR-0005 §7), UPDATE-only: `create.*` algorithms only;
    // the password Secret reference is mutable (a rename with identical content passes).
    if let Some(old) = &old_spec {
        errs.extend(api::validate::validate_repository_immutability(old, &spec));
    }
    if !errs.is_empty() {
        return Err(AdmissionError::Invalid(errs));
    }

    let mut warnings =
        api::validate::repository_warnings(&spec.backend, spec.mover_defaults.as_ref());

    // identityDefaults edit guard (silent re-identification): same rule as
    // `handle_cluster_repository` above. The consumer SnapshotPolicy LIST is
    // cluster-wide, not scoped to this Repository's own namespace —
    // `RepositoryRef.namespace` is a documented, supported cross-namespace
    // reference (see `api::common::RepositoryRef`), so a namespaced
    // Repository's consumers are NOT confined to its own namespace. This
    // mirrors the pre-existing collision guard
    // (`identity_collision::check_identity_collision` uses `Api::all`
    // unconditionally). Degrades to allow (fail-open) when there is no client
    // or the LIST fails — including a 403 under a namespaced Role install —
    // same posture as the fork/collision guards (see
    // `identity_repo_edit::affected_consumers`'s doc).
    if let Some(old) = &old_spec {
        let name = obj.metadata.name.as_deref().unwrap_or(req.name.as_str());
        let namespace = obj
            .metadata
            .namespace
            .as_deref()
            .or(req.namespace.as_deref())
            .unwrap_or_default();
        let self_key = crate::identity_collision::repo_key(
            &RepositoryRef {
                kind: RepositoryKind::Repository,
                name: name.to_string(),
                namespace: None,
            },
            namespace,
        );
        let outcome = crate::identity_repo_edit::check_repository_identity_change(
            client,
            &self_key,
            old.identity_defaults.as_ref(),
            spec.identity_defaults.as_ref(),
            obj.metadata.annotations.as_ref(),
        )
        .await;
        if let Some(err) = outcome.error {
            return Err(AdmissionError::Invalid(vec![err]));
        }
        if !outcome.consumers.is_empty() {
            warnings.push(format!(
                "identityDefaults change acknowledged: this re-identifies {}",
                api::error::describe_identity_change_consumers(&outcome.consumers)
            ));
        }
    }

    Ok(with_warnings(resp, warnings))
}

// --- shared tenancy adapter -------------------------------------------------

/// Gate a consumer's `RepositoryRef` against `ClusterRepository` tenancy.
///
/// - `Repository` refs are not gated here (cross-namespace `Repository` references
///   are allowed and RBAC-gated elsewhere).
/// - `ClusterRepository` refs go through the **fail-closed** resolver
///   ([`tenancy::resolve_tenancy_inputs`]): it fetches the `ClusterRepository` + the
///   consumer namespace's labels and evaluates the gate. No client / unresolvable
///   inputs → deny.
async fn tenancy_for(
    repo: &RepositoryRef,
    consumer_namespace: Option<&str>,
    client: Option<&Client>,
) -> TenancyDecision {
    match repo.kind {
        RepositoryKind::Repository => TenancyDecision::Allow,
        RepositoryKind::ClusterRepository => {
            // validate_repository_ref already rejected a set namespace; the consumer's
            // own namespace is what the gate is evaluated against.
            let Some(ns) = consumer_namespace else {
                return TenancyDecision::Deny(TenancyDenial::NoConsumerNamespace);
            };
            tenancy::resolve_tenancy_inputs(client, ns, &repo.name).await
        }
    }
}

/// Build a JSON-patch op that sets `spec.<field>`, creating `/spec` first if the raw
/// object had no spec object at all (an empty discovered `Snapshot`). We use a `test`
/// guard only when `/spec` is known present to keep patches minimal.
fn set_spec_field(data: &Value, field: &str, value: Value) -> PatchOperation {
    if data.get("spec").and_then(|s| s.as_object()).is_some() {
        PatchOperation::Add(AddOperation {
            path: ptr(&format!("/spec/{field}")),
            value,
        })
    } else {
        // No spec object: add the whole spec with just this field.
        PatchOperation::Add(AddOperation {
            path: ptr("/spec"),
            value: json!({ field: value }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- config_label_stamp (pure) ---------------------------------------------

    fn policy_ref(name: &str, namespace: Option<&str>) -> PolicyRef {
        PolicyRef {
            name: name.to_string(),
            namespace: namespace.map(str::to_string),
        }
    }

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn manual_same_ns_ref_with_no_labels_map_stamps() {
        let r = policy_ref("nightly", None);
        assert_eq!(
            config_label_stamp(Origin::Manual, Some(&r), "billing", None),
            Some("nightly".to_string())
        );
    }

    #[test]
    fn manual_ref_with_existing_labels_map_missing_key_stamps() {
        let r = policy_ref("nightly", None);
        let existing = labels(&[("some-other", "label")]);
        assert_eq!(
            config_label_stamp(Origin::Manual, Some(&r), "billing", Some(&existing)),
            Some("nightly".to_string())
        );
    }

    #[test]
    fn label_already_present_is_a_no_op_regardless_of_value() {
        let r = policy_ref("nightly", None);
        let existing = labels(&[(api::consts::CONFIG_LABEL, "some-other-policy")]);
        assert_eq!(
            config_label_stamp(Origin::Manual, Some(&r), "billing", Some(&existing)),
            None
        );
    }

    #[test]
    fn explicit_cross_namespace_ref_is_a_no_op() {
        let r = policy_ref("nightly", Some("other-ns"));
        assert_eq!(
            config_label_stamp(Origin::Manual, Some(&r), "billing", None),
            None
        );
    }

    #[test]
    fn explicit_same_namespace_ref_still_stamps() {
        let r = policy_ref("nightly", Some("billing"));
        assert_eq!(
            config_label_stamp(Origin::Manual, Some(&r), "billing", None),
            Some("nightly".to_string())
        );
    }

    #[test]
    fn absent_policy_ref_is_a_no_op() {
        assert_eq!(
            config_label_stamp(Origin::Manual, None, "billing", None),
            None
        );
    }

    #[test]
    fn discovered_origin_is_a_no_op_even_with_a_ref() {
        let r = policy_ref("nightly", None);
        assert_eq!(
            config_label_stamp(Origin::Discovered, Some(&r), "billing", None),
            None
        );
    }

    #[test]
    fn scheduled_origin_with_ref_stamps_same_as_manual() {
        // Harmless idempotent parity with the schedule controller, which already
        // stamps this label itself — this codepath is a no-op there in practice.
        let r = policy_ref("nightly", None);
        assert_eq!(
            config_label_stamp(Origin::Scheduled, Some(&r), "billing", None),
            Some("nightly".to_string())
        );
    }

    // --- config_label_op (ops-building) -----------------------------------------

    fn add_op(op: PatchOperation) -> AddOperation {
        match op {
            PatchOperation::Add(add) => add,
            other => panic!("expected an Add op, got {other:?}"),
        }
    }

    #[test]
    fn config_label_op_creates_the_labels_map_when_absent() {
        let meta = ObjectMeta::default();
        let op = add_op(config_label_op(&meta, "nightly"));
        assert_eq!(op.path.to_string(), "/metadata/labels");
        assert_eq!(
            op.value,
            json!({ "kopiur.home-operations.com/config": "nightly" })
        );
    }

    #[test]
    fn config_label_op_adds_just_the_key_when_the_map_already_exists() {
        let meta = ObjectMeta {
            labels: Some(labels(&[("some-other", "label")])),
            ..Default::default()
        };
        let op = add_op(config_label_op(&meta, "nightly"));
        // The slash in the label key must be RFC 6901 ("~1") escaped.
        assert_eq!(
            op.path.to_string(),
            "/metadata/labels/kopiur.home-operations.com~1config"
        );
        assert_eq!(op.value, json!("nightly"));
    }

    /// Build a CREATE `AdmissionRequest` for the given kind/spec, the way the API
    /// server would. No cluster needed — Repository/ClusterRepository validation is
    /// pure, and `dispatch` only touches a `Client` for the tenancy-gated kinds.
    fn admission_request(kind: &str, spec: Value) -> AdmissionRequest<DynamicObject> {
        let review = json!({
            "apiVersion": "admission.k8s.io/v1",
            "kind": "AdmissionReview",
            "request": {
                "uid": "test-uid",
                "kind": { "group": "kopiur.home-operations.com", "version": "v1alpha1", "kind": kind },
                "resource": { "group": "kopiur.home-operations.com", "version": "v1alpha1", "resource": "repositories" },
                "name": "repo",
                "namespace": "kopiur-system",
                "operation": "CREATE",
                "userInfo": { "username": "tester" },
                "object": {
                    "apiVersion": "kopiur.home-operations.com/v1alpha1",
                    "kind": kind,
                    "metadata": { "name": "repo", "namespace": "kopiur-system" },
                    "spec": spec,
                }
            }
        });
        let review: kube::core::admission::AdmissionReview<DynamicObject> =
            serde_json::from_value(review).unwrap();
        review.try_into().unwrap()
    }

    #[tokio::test]
    async fn nfs_repository_admission_carries_the_fsgroup_warning() {
        let spec = json!({
            "backend": { "filesystem": { "path": "/repo", "volume": { "nfs": { "server": "nas.lan", "path": "/export/kopia" } } } },
            "encryption": { "passwordSecretRef": { "name": "creds" } },
        });
        let req = admission_request("Repository", spec);
        let resp = dispatch(&req, None).await;
        assert!(resp.allowed, "NFS repo must still be admitted");
        assert_eq!(
            resp.warnings.as_deref(),
            Some(&[api::validate::NFS_FSGROUP_WARNING.to_string()][..]),
        );
    }

    #[tokio::test]
    async fn s3_repository_admission_has_no_warnings() {
        let spec = json!({
            "backend": { "s3": { "bucket": "b", "endpoint": "https://minio" } },
            "encryption": { "passwordSecretRef": { "name": "creds" } },
        });
        let req = admission_request("Repository", spec);
        let resp = dispatch(&req, None).await;
        assert!(resp.allowed);
        assert!(
            resp.warnings.is_none(),
            "no spurious warnings: {:?}",
            resp.warnings
        );
    }

    // --- identity hardening ----------------------------------------------------

    #[tokio::test]
    async fn create_with_bad_identity_override_is_rejected() {
        // A '@' in an explicit username override would misparse — rejected at admission
        // (client-free path, no cluster needed).
        let spec = json!({
            "repository": { "kind": "Repository", "name": "r" },
            "identity": { "username": "bad@user" },
            "sources": [ { "pvc": { "name": "data" } } ],
        });
        let req = admission_request("SnapshotPolicy", spec);
        let resp = dispatch(&req, None).await;
        assert!(!resp.allowed, "bad identity override must be rejected");
        assert!(
            resp.result
                .message
                .contains("not a valid kopia identity component"),
            "{:?}",
            resp.result.message
        );
    }

    /// Build an UPDATE `AdmissionRequest` for a `SnapshotPolicy`, with the new object
    /// (spec + annotations) and the `oldObject` (spec + status) as the API server sends
    /// them. Used by the fork-on-edit guard tests.
    fn update_policy_request(
        new_spec: Value,
        new_annotations: Value,
        old_spec: Value,
        old_status: Value,
    ) -> AdmissionRequest<DynamicObject> {
        let review = json!({
            "apiVersion": "admission.k8s.io/v1",
            "kind": "AdmissionReview",
            "request": {
                "uid": "test-uid",
                "kind": { "group": "kopiur.home-operations.com", "version": "v1alpha1", "kind": "SnapshotPolicy" },
                "resource": { "group": "kopiur.home-operations.com", "version": "v1alpha1", "resource": "snapshotpolicies" },
                "name": "pg",
                "namespace": "billing",
                "operation": "UPDATE",
                "userInfo": { "username": "tester" },
                "object": {
                    "apiVersion": "kopiur.home-operations.com/v1alpha1",
                    "kind": "SnapshotPolicy",
                    "metadata": { "name": "pg", "namespace": "billing", "annotations": new_annotations },
                    "spec": new_spec,
                },
                "oldObject": {
                    "apiVersion": "kopiur.home-operations.com/v1alpha1",
                    "kind": "SnapshotPolicy",
                    "metadata": { "name": "pg", "namespace": "billing" },
                    "spec": old_spec,
                    "status": old_status,
                }
            }
        });
        let review: kube::core::admission::AdmissionReview<DynamicObject> =
            serde_json::from_value(review).unwrap();
        review.try_into().unwrap()
    }

    fn source(path_override: Option<&str>) -> Value {
        match path_override {
            Some(p) => json!({ "pvc": { "name": "data" }, "sourcePathOverride": p }),
            None => json!({ "pvc": { "name": "data" } }),
        }
    }

    fn policy_spec(path_override: Option<&str>) -> Value {
        json!({
            "repository": { "kind": "Repository", "name": "r" },
            "sources": [ source(path_override) ],
        })
    }

    fn status_with_history(has_history: bool) -> Value {
        let mut s = json!({
            "resolved": { "identity": { "username": "pg", "hostname": "billing" } }
        });
        if has_history {
            s["lastSuccessfulSnapshot"] = json!("2026-06-19T00:00:00Z");
        }
        s
    }

    #[tokio::test]
    async fn source_path_fork_on_policy_with_history_is_rejected() {
        // The PVC's effective path changes (/pvc/data → /data) on a policy that has
        // produced snapshots. This path is pure (no CEL), so the guard fires with no
        // client.
        let req = update_policy_request(
            policy_spec(Some("/data")),
            json!({}),
            policy_spec(None),
            status_with_history(true),
        );
        let resp = dispatch(&req, None).await;
        assert!(!resp.allowed, "path re-identification must be rejected");
        assert!(
            resp.result
                .message
                .contains("competing in the same GFS retention timeline"),
            "{:?}",
            resp.result.message
        );
    }

    #[tokio::test]
    async fn source_path_fork_is_allowed_with_ack_annotation() {
        let req = update_policy_request(
            policy_spec(Some("/data")),
            json!({ "kopiur.home-operations.com/allow-identity-change": "yes" }),
            policy_spec(None),
            status_with_history(true),
        );
        let resp = dispatch(&req, None).await;
        assert!(
            resp.allowed,
            "acknowledged re-identification must be allowed"
        );
    }

    #[tokio::test]
    async fn source_path_change_is_allowed_without_history() {
        // No successful snapshot yet → nothing to orphan → allowed (typo-fix case).
        let req = update_policy_request(
            policy_spec(Some("/data")),
            json!({}),
            policy_spec(None),
            status_with_history(false),
        );
        let resp = dispatch(&req, None).await;
        assert!(
            resp.allowed,
            "no-history edit must be allowed: {:?}",
            resp.result.message
        );
    }

    // --- repository identityDefaults edit guard -------------------------------

    /// A `Client` whose every request returns the same canned JSON body. These
    /// tests only ever trigger the guard's single cluster-wide `SnapshotPolicy`
    /// LIST, so no method/path branching is needed — a hermetic, no-cluster
    /// stand-in for `Api::list`, mirroring `kopiur-controller::leader`'s
    /// `tower::service_fn` mock-client pattern.
    fn mock_list_client(list_body: Value) -> Client {
        let svc = tower::service_fn(move |_req: http::Request<kube::client::Body>| {
            let body = list_body.clone();
            async move {
                let resp = http::Response::builder()
                    .status(http::StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(kube::client::Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap();
                Ok::<_, std::convert::Infallible>(resp)
            }
        });
        Client::new(svc, "test-ns")
    }

    /// Build an UPDATE `AdmissionRequest` for a `ClusterRepository` (cluster-scoped:
    /// no namespace), with the new object (spec + annotations) and the `oldObject`
    /// spec. Used by the identityDefaults-edit-guard tests.
    fn update_cluster_repository_request(
        name: &str,
        new_spec: Value,
        new_annotations: Value,
        old_spec: Value,
    ) -> AdmissionRequest<DynamicObject> {
        let review = json!({
            "apiVersion": "admission.k8s.io/v1",
            "kind": "AdmissionReview",
            "request": {
                "uid": "test-uid",
                "kind": { "group": "kopiur.home-operations.com", "version": "v1alpha1", "kind": "ClusterRepository" },
                "resource": { "group": "kopiur.home-operations.com", "version": "v1alpha1", "resource": "clusterrepositories" },
                "name": name,
                "operation": "UPDATE",
                "userInfo": { "username": "tester" },
                "object": {
                    "apiVersion": "kopiur.home-operations.com/v1alpha1",
                    "kind": "ClusterRepository",
                    "metadata": { "name": name, "annotations": new_annotations },
                    "spec": new_spec,
                },
                "oldObject": {
                    "apiVersion": "kopiur.home-operations.com/v1alpha1",
                    "kind": "ClusterRepository",
                    "metadata": { "name": name },
                    "spec": old_spec,
                }
            }
        });
        let review: kube::core::admission::AdmissionReview<DynamicObject> =
            serde_json::from_value(review).unwrap();
        review.try_into().unwrap()
    }

    fn cluster_repository_spec(cluster: &str) -> Value {
        json!({
            "backend": { "filesystem": { "path": "/r" } },
            "encryption": { "passwordSecretRef": { "name": "s", "namespace": "kopia-system" } },
            "allowedNamespaces": { "all": true },
            "identityDefaults": { "cluster": cluster }
        })
    }

    fn repository_spec(cluster: &str) -> Value {
        json!({
            "backend": { "filesystem": { "path": "/r" } },
            "encryption": { "passwordSecretRef": { "name": "s" } },
            "identityDefaults": { "cluster": cluster }
        })
    }

    /// Build an UPDATE `AdmissionRequest` for a namespaced `Repository`, with the
    /// new object (spec + annotations) and the `oldObject` spec. Mirrors
    /// `update_cluster_repository_request`; used by the identityDefaults-edit-guard
    /// tests below.
    fn update_repository_request(
        namespace: &str,
        name: &str,
        new_spec: Value,
        new_annotations: Value,
        old_spec: Value,
    ) -> AdmissionRequest<DynamicObject> {
        let review = json!({
            "apiVersion": "admission.k8s.io/v1",
            "kind": "AdmissionReview",
            "request": {
                "uid": "test-uid",
                "kind": { "group": "kopiur.home-operations.com", "version": "v1alpha1", "kind": "Repository" },
                "resource": { "group": "kopiur.home-operations.com", "version": "v1alpha1", "resource": "repositories" },
                "name": name,
                "namespace": namespace,
                "operation": "UPDATE",
                "userInfo": { "username": "tester" },
                "object": {
                    "apiVersion": "kopiur.home-operations.com/v1alpha1",
                    "kind": "Repository",
                    "metadata": { "name": name, "namespace": namespace, "annotations": new_annotations },
                    "spec": new_spec,
                },
                "oldObject": {
                    "apiVersion": "kopiur.home-operations.com/v1alpha1",
                    "kind": "Repository",
                    "metadata": { "name": name, "namespace": namespace },
                    "spec": old_spec,
                }
            }
        });
        let review: kube::core::admission::AdmissionReview<DynamicObject> =
            serde_json::from_value(review).unwrap();
        review.try_into().unwrap()
    }

    /// A `SnapshotPolicyList` LIST response with a single consumer referencing
    /// `repo_name` (as `repo_kind`: `"ClusterRepository"` or `"Repository"`), with
    /// or without snapshot history, and no `spec.identity` override.
    fn consumer_policy_list(
        namespace: &str,
        name: &str,
        repo_kind: &str,
        repo_name: &str,
        has_history: bool,
    ) -> Value {
        let mut status = json!({});
        if has_history {
            status["lastSuccessfulSnapshot"] = json!("2026-06-19T00:00:00Z");
        }
        json!({
            "items": [{
                "metadata": { "name": name, "namespace": namespace },
                "spec": {
                    "repository": { "kind": repo_kind, "name": repo_name },
                    "sources": [ { "pvc": { "name": "data" } } ]
                },
                "status": status
            }]
        })
    }

    #[tokio::test]
    async fn cluster_repository_identity_defaults_change_with_history_denied() {
        // A consumer with existing history references THIS ClusterRepository and
        // doesn't pin identity — editing identityDefaults would silently re-identify
        // it on its next backup.
        let client = mock_list_client(consumer_policy_list(
            "billing",
            "pg",
            "ClusterRepository",
            "shared",
            true,
        ));
        let req = update_cluster_repository_request(
            "shared",
            cluster_repository_spec("west"),
            json!({}),
            cluster_repository_spec("east"),
        );
        let resp = dispatch(&req, Some(&client)).await;
        assert!(!resp.allowed, "must deny: {:?}", resp.result.message);
        assert!(
            resp.result.message.contains("billing/pg"),
            "{:?}",
            resp.result.message
        );
    }

    #[tokio::test]
    async fn cluster_repository_identity_defaults_change_acked_allowed_with_warning() {
        let client = mock_list_client(consumer_policy_list(
            "billing",
            "pg",
            "ClusterRepository",
            "shared",
            true,
        ));
        let req = update_cluster_repository_request(
            "shared",
            cluster_repository_spec("west"),
            json!({ "kopiur.home-operations.com/allow-identity-change": "intentional" }),
            cluster_repository_spec("east"),
        );
        let resp = dispatch(&req, Some(&client)).await;
        assert!(
            resp.allowed,
            "acknowledged change must be allowed: {:?}",
            resp.result.message
        );
        let warnings = resp.warnings.unwrap_or_default();
        assert!(
            warnings.iter().any(|w| w.contains("billing/pg")),
            "expected a warning naming the re-identified consumer: {warnings:?}"
        );
    }

    #[tokio::test]
    async fn cluster_repository_no_identity_defaults_change_is_allowed_without_warning() {
        // No client at all: the guard must short-circuit before ever asking for one.
        let req = update_cluster_repository_request(
            "shared",
            cluster_repository_spec("east"),
            json!({}),
            cluster_repository_spec("east"),
        );
        let resp = dispatch(&req, None).await;
        assert!(resp.allowed, "{:?}", resp.result.message);
        assert!(
            resp.warnings.is_none(),
            "unchanged identityDefaults must not warn: {:?}",
            resp.warnings
        );
    }

    #[tokio::test]
    async fn cluster_repository_identity_defaults_change_without_client_degrades_to_allow() {
        // A real change, but no client to list consumers with — fail open (same
        // posture as the fork/collision guards): a repository apply must not wedge
        // on a webhook that can't reach the API server for a best-effort check.
        let req = update_cluster_repository_request(
            "shared",
            cluster_repository_spec("west"),
            json!({}),
            cluster_repository_spec("east"),
        );
        let resp = dispatch(&req, None).await;
        assert!(
            resp.allowed,
            "no client => degrade to allow: {:?}",
            resp.result.message
        );
    }

    // --- Repository identityDefaults edit guard (M5) --------------------------

    #[tokio::test]
    async fn repository_identity_defaults_change_with_history_denied() {
        // A consumer with existing history references THIS Repository (same
        // namespace) and doesn't pin identity — editing identityDefaults would
        // silently re-identify it on its next backup.
        let client = mock_list_client(consumer_policy_list(
            "billing",
            "pg",
            "Repository",
            "nas",
            true,
        ));
        let req = update_repository_request(
            "billing",
            "nas",
            repository_spec("west"),
            json!({}),
            repository_spec("east"),
        );
        let resp = dispatch(&req, Some(&client)).await;
        assert!(!resp.allowed, "must deny: {:?}", resp.result.message);
        assert!(
            resp.result.message.contains("billing/pg"),
            "{:?}",
            resp.result.message
        );
    }

    #[tokio::test]
    async fn repository_identity_defaults_change_catches_cross_namespace_consumer() {
        // The consumer LIST is cluster-wide: a policy in a DIFFERENT namespace
        // than the edited Repository, referencing it via an explicit
        // `RepositoryRef.namespace` (a documented, supported pattern — see
        // `api::common::RepositoryRef`), must still be caught, not silently
        // missed. This test exercises that end-to-end through `dispatch`; the
        // actual scope guard — proving the underlying LIST request itself
        // never gets namespace-scoped — is the URI-asserting
        // `identity_repo_edit::affected_consumers_lists_cluster_wide`.
        let list_body = json!({
            "items": [{
                "metadata": { "name": "pg", "namespace": "consumer-ns" },
                "spec": {
                    "repository": {
                        "kind": "Repository",
                        "name": "shared",
                        "namespace": "repo-ns",
                    },
                    "sources": [ { "pvc": { "name": "data" } } ]
                },
                "status": { "lastSuccessfulSnapshot": "2026-06-19T00:00:00Z" }
            }]
        });
        let client = mock_list_client(list_body);
        let req = update_repository_request(
            "repo-ns",
            "shared",
            repository_spec("west"),
            json!({}),
            repository_spec("east"),
        );
        let resp = dispatch(&req, Some(&client)).await;
        assert!(!resp.allowed, "must deny: {:?}", resp.result.message);
        assert!(
            resp.result.message.contains("consumer-ns/pg"),
            "{:?}",
            resp.result.message
        );
    }

    #[tokio::test]
    async fn repository_identity_defaults_change_acked_allowed_with_warning() {
        let client = mock_list_client(consumer_policy_list(
            "billing",
            "pg",
            "Repository",
            "nas",
            true,
        ));
        let req = update_repository_request(
            "billing",
            "nas",
            repository_spec("west"),
            json!({ "kopiur.home-operations.com/allow-identity-change": "intentional" }),
            repository_spec("east"),
        );
        let resp = dispatch(&req, Some(&client)).await;
        assert!(
            resp.allowed,
            "acknowledged change must be allowed: {:?}",
            resp.result.message
        );
        let warnings = resp.warnings.unwrap_or_default();
        assert!(
            warnings.iter().any(|w| w.contains("billing/pg")),
            "expected a warning naming the re-identified consumer: {warnings:?}"
        );
    }

    #[tokio::test]
    async fn repository_no_identity_defaults_change_is_allowed_without_warning() {
        // No client at all: the guard must short-circuit before ever asking for one.
        let req = update_repository_request(
            "billing",
            "nas",
            repository_spec("east"),
            json!({}),
            repository_spec("east"),
        );
        let resp = dispatch(&req, None).await;
        assert!(resp.allowed, "{:?}", resp.result.message);
        assert!(
            resp.warnings.is_none(),
            "unchanged identityDefaults must not warn: {:?}",
            resp.warnings
        );
    }

    #[tokio::test]
    async fn repository_identity_defaults_change_without_client_degrades_to_allow() {
        // A real change, but no client to list consumers with — fail open (same
        // posture as the fork/collision guards): a repository apply must not wedge
        // on a webhook that can't reach the API server for a best-effort check.
        let req = update_repository_request(
            "billing",
            "nas",
            repository_spec("west"),
            json!({}),
            repository_spec("east"),
        );
        let resp = dispatch(&req, None).await;
        assert!(
            resp.allowed,
            "no client => degrade to allow: {:?}",
            resp.result.message
        );
    }
}
