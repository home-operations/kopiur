//! RBAC manifest generation (ADR-0003 §4.12).
//!
//! The operator's permissions are expressed as a set of `PolicyRule`s, each
//! block documented inline. We emit two install flavours.
//!
//! Cluster-scoped (`deploy/rbac/operator-clusterrole.yaml`): a `ClusterRole` +
//! `ServiceAccount` + `ClusterRoleBinding` for the default cluster-wide install.
//! Includes the cluster-scoped `clusterrepositories` CRD and the
//! `serviceaccounts` create/get used to mint per-namespace mover SAs.
//!
//! Namespaced (`deploy/rbac/operator-role.yaml`): a `Role` + `ServiceAccount` +
//! `RoleBinding` for the namespaced-install mode (§4.11/§4.12), with the same
//! rules minus the cluster-scoped bits (`clusterrepositories`, `serviceaccounts`
//! minting) that don't apply when the operator is confined to a single
//! namespace.
//!
//! k8s-openapi structs don't carry `apiVersion`/`kind` as fields, so we
//! serialize to a `serde_json::Value` and splice those in from the `Resource`
//! trait constants before rendering YAML — exactly the shape `kubectl apply`
//! expects.

use anyhow::{Context, Result};
use k8s_openapi::Resource;
use k8s_openapi::api::core::v1::ServiceAccount;
use k8s_openapi::api::rbac::v1::{
    AggregationRule, ClusterRole, ClusterRoleBinding, PolicyRule, Role, RoleBinding, RoleRef,
    Subject,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use serde::Serialize;

use crate::artifact::{Artifact, RBAC_HEADER};

/// Operator identity used across the generated manifests.
const SA_NAME: &str = "kopiur-controller";
const CLUSTERROLE_NAME: &str = "kopiur-controller";
const ROLE_NAME: &str = "kopiur-controller";
const DEFAULT_NAMESPACE: &str = "kopiur-system";

/// The dedicated, least-privilege mover role (§4.12). Bound to a per-namespace
/// `kopiur-mover` ServiceAccount by a controller-minted RoleBinding; the SA and
/// binding are created at runtime, so only the role itself is shipped here.
const MOVER_CLUSTERROLE_NAME: &str = "kopiur-mover";
/// The dedicated snapshot-replication mover role (issue #368). The replication
/// mover creates and DELETES `Snapshot` CRs (copy-CR reconciliation + pruning),
/// verbs the generic `kopiur-mover` role must NEVER hold: a compromised generic
/// mover pod holding namespace-wide Snapshot delete could erase every backup
/// record in its namespace. So those verbs live on this separate role, bound
/// only to the equally dedicated `kopiur-snapshot-replication-mover`
/// ServiceAccount the controller mints per namespace for snapshot-replication
/// Jobs alone (`io::ensure_snapshot_replication_mover_identity`).
const SNAPSHOT_REPLICATION_MOVER_NAME: &str = "kopiur-snapshot-replication-mover";
/// Name of the leader-election Role + RoleBinding the cluster artifact pairs
/// with its ClusterRole (the Lease is namespace-local; see [`leader_election_rules`]).
const LEADER_ROLE_NAME: &str = "kopiur-leader-election";
/// Default election Lease name (`KOPIUR_LEASE_NAME` unset — off-chart runs).
/// Matches `kopiur_controller::config::DEFAULT_LEASE_NAME`.
const DEFAULT_LEASE_NAME: &str = "kopiur-leader";
const MOVER_ROLE_NAME: &str = "kopiur-mover";

/// CRD plurals whose `.status` the mover PATCHes (the dynamic `targetRef` kinds).
/// Deliberately excludes `snapshotschedules`, which the mover never touches.
const MOVER_STATUS_CRDS: &[&str] = &[
    "snapshots",
    "restores",
    "repositories",
    "clusterrepositories",
    "maintenances",
    // The replication mover PATCHes RepositoryReplication/status (ADR-0005 §13(d)),
    // and the verify mover PATCHes snapshotpolicies/status (ADR-0005 §4) — both
    // covered here.
    "snapshotpolicies",
    "repositoryreplications",
    // The snapshot-replication mover PATCHes SnapshotReplication/status at the
    // end of a run (issue #368).
    "snapshotreplications",
];

const KOPIA_GROUP: &str = "kopiur.home-operations.com";

/// Default names of the webhook configurations and serving Secret for the
/// self-managed-TLS RBAC (`webhook.tls.mode: self`). These match the chart's
/// helpers for the default release: `{fullname}-validating`/`-mutating` and the
/// `webhook.tls.secretName` default. The Helm templates re-derive them from the
/// release; the shipped static artifacts use the default-release names.
const WEBHOOK_VALIDATING_CONFIG: &str = "kopiur-validating";
const WEBHOOK_MUTATING_CONFIG: &str = "kopiur-mutating";
const WEBHOOK_TLS_SECRET: &str = "kopiur-webhook-tls";

/// All 9 CRD plurals in `kopiur.home-operations.com`. `clusterrepositories` is cluster-scoped.
const NAMESPACED_CRDS: &[&str] = &[
    "repositories",
    "snapshotpolicies",
    "snapshots",
    "snapshotschedules",
    "restores",
    "maintenances",
    "repositoryreplications",
    "snapshotreplications",
];
const CLUSTER_CRDS: &[&str] = &["clusterrepositories"];

fn verbs(vs: &[&str]) -> Vec<String> {
    vs.iter().map(|s| s.to_string()).collect()
}

fn rule(api_groups: &[&str], resources: &[String], vs: &[&str]) -> PolicyRule {
    PolicyRule {
        api_groups: Some(api_groups.iter().map(|s| s.to_string()).collect()),
        resources: Some(resources.to_vec()),
        verbs: verbs(vs),
        ..Default::default()
    }
}

/// Like [`rule`] but scoped to specific `resourceNames` (least privilege). Use
/// only with verbs that honor names (`get`/`update`/`patch`/`delete`, and
/// `impersonate` on `users`/`groups`/`userextras`) — `create`, `list`, and
/// `watch` are NOT name-scopable and would be denied.
fn rule_named(
    api_groups: &[&str],
    resources: &[String],
    vs: &[&str],
    names: &[String],
) -> PolicyRule {
    PolicyRule {
        api_groups: Some(api_groups.iter().map(|s| s.to_string()).collect()),
        resources: Some(resources.to_vec()),
        verbs: verbs(vs),
        resource_names: Some(names.to_vec()),
        ..Default::default()
    }
}

const FULL_VERBS: &[&str] = &[
    "get", "list", "watch", "create", "update", "patch", "delete",
];
const SUBRESOURCE_VERBS: &[&str] = &["get", "update", "patch"];
const READ_VERBS: &[&str] = &["get", "list", "watch"];

/// Build the `kopiur.home-operations.com` CRD rules shared by both flavours.
///
/// `include_cluster_crds` adds the cluster-scoped `clusterrepositories` CRD,
/// which only belongs in the cluster-scoped `ClusterRole`.
fn kopia_crd_rules(include_cluster_crds: bool) -> Vec<PolicyRule> {
    let mut crds: Vec<&str> = NAMESPACED_CRDS.to_vec();
    if include_cluster_crds {
        crds.extend_from_slice(CLUSTER_CRDS);
    }

    // Primary resources: full CRUD.
    let primary: Vec<String> = crds.iter().map(|s| s.to_string()).collect();
    // Subresources: status + finalizers, get/update/patch only.
    let mut sub: Vec<String> = Vec::with_capacity(crds.len() * 2);
    for c in &crds {
        sub.push(format!("{c}/status"));
        sub.push(format!("{c}/finalizers"));
    }

    vec![
        rule(&[KOPIA_GROUP], &primary, FULL_VERBS),
        rule(&[KOPIA_GROUP], &sub, SUBRESOURCE_VERBS),
    ]
}

/// Core / batch / snapshot rules the controller needs to drive movers, hooks,
/// jobs, and CSI snapshots (§4.12).
///
/// Includes the `serviceaccounts` + `rolebindings` minting rules: before every
/// mover Job the controller ensures a least-privilege `kopiur-mover`
/// ServiceAccount and a RoleBinding (to the `kopiur-mover` ClusterRole/Role)
/// exist in the Job's namespace — the workload namespace, which differs from the
/// operator's. Without this the mover Job's SA does not exist in that namespace
/// and the Job never schedules a pod (`FailedCreate: serviceaccount ... not
/// found`). The controller already holds a superset of the mover role's perms, so
/// creating the binding is not a privilege escalation (no `bind` verb needed).
fn workload_rules() -> Vec<PolicyRule> {
    vec![
        // Mover pods + exec for pre/post hooks; PVCs for snapshot/restore I/O;
        // events for surfacing reconcile outcomes; configmaps carry the
        // repository-bootstrap result channel (and the sweep reaps LEGACY
        // per-run work-spec ConfigMaps) — the mover runs as this SA, so its
        // result write reuses these configmaps verbs (no separate rule needed).
        rule(
            &[""],
            &[
                "pods".into(),
                "persistentvolumeclaims".into(),
                "configmaps".into(),
            ],
            FULL_VERBS,
        ),
        // Hook execution into running workload pods.
        rule(&[""], &["pods/exec".into()], &["create", "get"]),
        // Surface reconcile outcomes as Kubernetes Events. kube's `Recorder`
        // writes the modern `events.k8s.io/v1` Event (not the legacy core Event),
        // so BOTH api groups are required — the core `""` group alone yields a 403
        // on create and the Event is silently dropped.
        rule(&[""], &["events".into()], &["create", "patch"]),
        rule(&["events.k8s.io"], &["events".into()], &["create", "patch"]),
        // Secrets READ is always required: the controller resolves repository
        // credentials (KOPIA_PASSWORD + backend creds) on every reconcile.
        rule(&[""], &["secrets".into()], &["get", "list", "watch"]),
        // Secrets WRITE backs two opt-in features. This generated artifact is the
        // maximal "all features on" set; the Helm chart gates each write
        // INDEPENDENTLY behind a feature flag for least privilege:
        //   1. Credential projection (`spec.credentialProjection`,
        //      `features.credentialProjection.enabled`): the controller copies a
        //      repository's Secret(s) into each mover Job's namespace via
        //      server-side apply (a PATCH). `create` cannot be resourceName-scoped
        //      (the authorizer can't match a name at create time) and the projected
        //      name embeds the consuming CR's name, so this is necessarily unscoped.
        //      Needs create+patch+delete: ownerRef GC reaps the stable copy with its
        //      CR, while `delete` backs the cleanup paths (the leader-only sweep of
        //      legacy per-run copies, reap-on-shrink, reap-on-disable).
        //   2. kopia web-UI server (`spec.server`, `features.kopiaUi.enabled`):
        //      create-once the generated-auth Secret (Generate mode), SSA-patch the
        //      cross-namespace credentials mirror for ClusterRepository servers, and
        //      `delete` both on server teardown / namespace migration (owner-ref GC
        //      can't reach a cluster-scoped owner's namespaced children).
        // Union = create+patch+delete. There is deliberately NO `update`: every
        // write site is an SSA PATCH, a create-once, or a delete — nothing updates a
        // Secret in place. (The webhook-TLS rules below grant a separate, name-scoped
        // `update` on the serving Secret only.)
        rule(&[""], &["secrets".into()], &["create", "patch", "delete"]),
        // Services exposing the kopia web-UI server (`spec.server`).
        rule(&[""], &["services".into()], FULL_VERBS),
        // Mover Jobs and the kopia web-UI server Deployment (`spec.server`).
        rule(&["batch"], &["jobs".into()], FULL_VERBS),
        rule(&["apps"], &["deployments".into()], FULL_VERBS),
        // CSI volume snapshots used as a consistent source for snapshotting
        // (`copyMethod: Snapshot`/`Clone`, ADR §3.3) — namespaced, so they live here.
        // `patch` backs the server-side apply the controller uses to (idempotently)
        // create the staged VolumeSnapshot. The cluster-scoped VolumeSnapshotClasses /
        // VolumeSnapshotContents / StorageClasses are granted in the ClusterRole only.
        rule(
            &["snapshot.storage.k8s.io"],
            &["volumesnapshots".into()],
            &["get", "list", "watch", "create", "patch", "delete"],
        ),
        // `patch` is required because `io::apply` is a server-side apply (a
        // PATCH): N members race to converge the SAME shared group object, and
        // SSA over a deterministic name is what makes that safe. `create`
        // alone would 403 every one of them.
        rule(
            &["groupsnapshot.storage.k8s.io"],
            &["volumegroupsnapshots".into()],
            &["get", "list", "watch", "create", "patch", "delete"],
        ),
        // Per-namespace mover RBAC minted by the controller (§4.12): a
        // `kopiur-mover` ServiceAccount plus a RoleBinding to the `kopiur-mover`
        // role, created in each mover Job's namespace. `io::ensure_mover_rbac`
        // mints via server-side apply (a PATCH), so `patch` (and `update`) are
        // REQUIRED alongside `create`/`get` — without `patch` the apply is 403'd
        // and the SA is never minted, so the mover Job FailedCreates.
        // `list`/`watch` added for workload identity: the Repository /
        // ClusterRepository controllers watch ServiceAccounts so creating the
        // `auth.workloadIdentity` SA un-sticks a blocked repo immediately.
        rule(
            &[""],
            &["serviceaccounts".into()],
            &["get", "list", "watch", "create", "update", "patch"],
        ),
        rule(
            &["rbac.authorization.k8s.io"],
            &["rolebindings".into()],
            &["get", "create", "update", "patch"],
        ),
    ]
}

/// Leader election (`--leader-elect` / `controller.leaderElection.enabled`):
/// the controller claims and renews a `coordination.k8s.io/v1` Lease in its own
/// namespace (`crates/controller/src/leader.rs`). `get`/`update` (the CAS
/// renew/claim is a `replace`) are `resourceName`-scoped to the one Lease the
/// protocol ever touches; `create` cannot be name-scoped (the authorizer can't
/// match a name at create time) but stays namespace-local. No `list`/`watch` —
/// the protocol polls its one Lease by name. Deliberately a namespaced Role in
/// BOTH install flavours (the cluster artifact pairs its ClusterRole with a
/// small Role+RoleBinding): a cluster-wide leases grant would let the operator
/// re-stamp node-heartbeat Leases or steal other controllers' elections.
fn leader_election_rules(lease_name: &str) -> Vec<PolicyRule> {
    vec![
        rule(&["coordination.k8s.io"], &["leases".into()], &["create"]),
        rule_named(
            &["coordination.k8s.io"],
            &["leases".into()],
            &["get", "update"],
            &[lease_name.into()],
        ),
    ]
}

/// RBAC for self-managed webhook TLS (`webhook.tls.mode: self`): the controller
/// mints its own serving certificate and injects the `caBundle` into its webhook
/// configurations, removing the cert-manager dependency. Carried by both install
/// flavours (admission configs are cluster-scoped; the Secret is namespace-local);
/// the chart only grants these in `self` mode.
///
/// The **cluster-scoped** half: `get`+`patch` on the two webhook configurations,
/// scoped by `resourceName`. The controller GETs each by name and patches its
/// `caBundle` (no `list`/`watch` needed — and those can't be name-scoped anyway).
/// Webhook configurations are cluster-scoped, so this only belongs in a
/// `ClusterRole`; a namespaced install pairs its Role with a small ClusterRole
/// the chart renders for `self` mode.
fn webhook_cert_cluster_rules() -> Vec<PolicyRule> {
    vec![rule_named(
        &["admissionregistration.k8s.io"],
        &[
            "validatingwebhookconfigurations".into(),
            "mutatingwebhookconfigurations".into(),
        ],
        &["get", "patch"],
        &[
            WEBHOOK_VALIDATING_CONFIG.into(),
            WEBHOOK_MUTATING_CONFIG.into(),
        ],
    )]
}

/// The **namespace-local** half: writing the serving Secret. `create` is unscoped
/// because the authorizer cannot match a `resourceName` at create time; the
/// rotation re-apply (`update`/`patch`) is scoped to the serving Secret by name.
/// Reads come from the existing `secrets` `get`/`list`/`watch` rule. Valid in
/// both a Role and a ClusterRole.
fn webhook_cert_secret_rules() -> Vec<PolicyRule> {
    vec![
        rule(&[""], &["secrets".into()], &["create"]),
        rule_named(
            &[""],
            &["secrets".into()],
            &["update", "patch"],
            &[WEBHOOK_TLS_SECRET.into()],
        ),
    ]
}

/// The minimal RBAC a mover Job actually uses, verified against `crates/mover/src/`:
/// it PATCHes the owning CR's `.status` subresource (the target kind is dynamic —
/// `backups`, `restores`, `repositories`, `clusterrepositories`, `maintenances`)
/// and PATCHes the result `ConfigMap` to write the repository-bootstrap result.
/// Nothing else — credentials are env-mounted (no `secrets` access) and the work
/// spec rides the Job's own pod env (no `configmaps` get for it). This is deliberately a
/// tiny subset of [`workload_rules`]; the mover runs as its own least-privilege SA.
///
/// The `get` verb on `{crd}/status` is LOAD-BEARING (#401): the restore mover
/// reads the CR's pinned `status.resolved` (retry determinism) via a GET on the
/// status subresource — the one route this role authorizes. The mover must
/// never GET/LIST the base resources, and this role must never grant them; the
/// mover-side counterpart is `KubeStatusReporter::read_resolved` using
/// `get_status`, guarded by tests on both sides.
///
/// `cluster` decides whether `clusterrepositories/status` is included. The
/// namespaced mover Role must EXCLUDE it: RBAC escalation prevention rejects a
/// RoleBinding whose referenced role grants permissions the binder does not
/// hold, and the operator's namespaced Role can never hold a cluster-scoped
/// kind — so an "inert parity" entry here made the controller's runtime
/// RoleBinding mint 403 and blocked EVERY mover in a namespaced install.
fn mover_rules(cluster: bool) -> Vec<PolicyRule> {
    let statuses: Vec<String> = MOVER_STATUS_CRDS
        .iter()
        .filter(|c| cluster || **c != "clusterrepositories")
        .map(|c| format!("{c}/status"))
        .collect();
    vec![
        rule(&[KOPIA_GROUP], &statuses, &["get", "patch"]),
        rule(&[""], &["configmaps".into()], &["get", "patch"]),
    ]
}

/// The dedicated snapshot-replication mover's RBAC, verified against
/// `crates/mover/src/replicate.rs` + `main.rs`:
///
/// - `snapshots` get/list/create/patch/delete: the copy-CR reconciliation
///   LISTs the dest repo's rows, server-side-APPLIES each copy CR (SSA is a
///   `patch`, plus `create` for a first write — m:217cd0be), and deletes
///   discovered duplicates / pruned copies.
/// - `snapshots/status` get/patch: the atomic per-copy status body.
/// - `snapshotreplications/status` get/patch: the terminal run stamp
///   (phase/lastReplicated/lastRun).
/// - `configmaps` get/patch: parity with the generic mover role (the shared
///   result-channel machinery), nothing broader.
///
/// **Deliberately NOT a widening of the generic mover role**: namespace-wide
/// `snapshots` create/delete must never reach every mover pod — see
/// [`SNAPSHOT_REPLICATION_MOVER_NAME`].
fn snapshot_replication_mover_rules() -> Vec<PolicyRule> {
    vec![
        rule(
            &[KOPIA_GROUP],
            &["snapshots".into()],
            &["get", "list", "create", "patch", "delete"],
        ),
        rule(
            &[KOPIA_GROUP],
            &[
                "snapshots/status".into(),
                "snapshotreplications/status".into(),
            ],
            &["get", "patch"],
        ),
        rule(&[""], &["configmaps".into()], &["get", "patch"]),
    ]
}

/// Splice `apiVersion`/`kind` into a serialized k8s-openapi object and render
/// it as a YAML document body (no leading header).
fn render<T: Serialize + Resource>(obj: &T) -> Result<String> {
    let mut value = serde_json::to_value(obj).context("serializing RBAC object")?;
    let map = value
        .as_object_mut()
        .context("RBAC object did not serialize to a JSON object")?;
    map.insert("apiVersion".into(), T::API_VERSION.into());
    map.insert("kind".into(), T::KIND.into());
    let yaml = serde_yaml::to_string(&value).context("rendering RBAC object to YAML")?;
    Ok(yaml)
}

/// Join several rendered documents with `---` under a single header.
fn document(parts: &[String]) -> String {
    let body = parts
        .iter()
        .map(|p| p.trim_end())
        .collect::<Vec<_>>()
        .join("\n---\n");
    format!("{RBAC_HEADER}{body}\n")
}

fn metadata(name: &str, namespace: Option<&str>) -> ObjectMeta {
    ObjectMeta {
        name: Some(name.to_string()),
        namespace: namespace.map(str::to_string),
        labels: Some(std::collections::BTreeMap::from([(
            "app.kubernetes.io/name".to_string(),
            "kopiur".to_string(),
        )])),
        ..Default::default()
    }
}

/// Generate the cluster-scoped RBAC artifact.
fn cluster_artifact() -> Result<Artifact> {
    let mut rules = kopia_crd_rules(true);
    rules.extend(workload_rules());
    // Read Namespaces to check the privileged-movers opt-in annotation (ADR
    // §4.11/§G16). Cluster-scoped resource → cluster install only; a namespaced
    // install can't grant it and the controller fails the check open there.
    rules.push(rule(&[""], &["namespaces".into()], READ_VERBS));
    // RWO Multi-Attach avoidance: discover the node a ReadWriteOnce source/
    // destination PVC is attached to so the mover can be pinned there. The bound
    // PV's `nodeAffinity` (topology-pinned volumes) and the CSI `VolumeAttachment`
    // (ground-truth attached node) are read-only fallbacks when no consuming pod is
    // found. `patch` lets staging flip a staged PVC's bound PV from a `Retain` reclaim
    // policy to `Delete` before deleting it, so a `Retain` StorageClass doesn't leak the
    // PV + backend volume (ADR §3.3). All cluster-scoped → cluster install only (a
    // namespaced install can't grant them; co-location then relies on the
    // consuming-pod lookup, which is namespace-local and still works).
    rules.push(rule(
        &[""],
        &["persistentvolumes".into()],
        &["get", "list", "watch", "patch"],
    ));
    rules.push(rule(
        &["storage.k8s.io"],
        &["volumeattachments".into()],
        READ_VERBS,
    ));
    // CSI snapshot staging (`copyMethod: Snapshot`/`Clone`, ADR §3.3): read
    // StorageClasses to resolve the source PVC's provisioner (driver); read
    // VolumeSnapshotClasses to pick the driver's class + detect whether the snapshot
    // stack is installed; delete VolumeSnapshotContents on cleanup. All cluster-scoped →
    // cluster install only (a namespaced install can't stage CSI snapshots).
    rules.push(rule(
        &["storage.k8s.io"],
        &["storageclasses".into()],
        READ_VERBS,
    ));
    rules.push(rule(
        &["snapshot.storage.k8s.io"],
        &["volumesnapshotclasses".into()],
        READ_VERBS,
    ));
    // Cluster-scoped, and the reason `groupBy: VolumeGroupSnapshot` cannot work
    // on a namespaced install: resolving the class for a group capture is a
    // cluster-wide read, exactly like its per-volume counterpart above.
    rules.push(rule(
        &["groupsnapshot.storage.k8s.io"],
        &["volumegroupsnapshotclasses".into()],
        READ_VERBS,
    ));
    rules.push(rule(
        &["snapshot.storage.k8s.io"],
        &["volumesnapshotcontents".into()],
        &["get", "list", "watch", "delete"],
    ));
    // Self-managed webhook TLS (`webhook.tls.mode: self`): both halves fit a
    // ClusterRole.
    rules.extend(webhook_cert_cluster_rules());
    rules.extend(webhook_cert_secret_rules());
    // Leader election deliberately NOT here: the Lease is namespace-local, so
    // the cluster artifact pairs this ClusterRole with a small Role+RoleBinding
    // below instead of granting leases cluster-wide.

    let clusterrole = ClusterRole {
        metadata: metadata(CLUSTERROLE_NAME, None),
        rules: Some(rules),
        ..Default::default()
    };

    let sa = ServiceAccount {
        metadata: metadata(SA_NAME, Some(DEFAULT_NAMESPACE)),
        ..Default::default()
    };

    let binding = ClusterRoleBinding {
        metadata: metadata(CLUSTERROLE_NAME, None),
        role_ref: RoleRef {
            api_group: "rbac.authorization.k8s.io".to_string(),
            kind: "ClusterRole".to_string(),
            name: CLUSTERROLE_NAME.to_string(),
        },
        subjects: Some(vec![Subject {
            kind: "ServiceAccount".to_string(),
            name: SA_NAME.to_string(),
            namespace: Some(DEFAULT_NAMESPACE.to_string()),
            api_group: None,
        }]),
    };

    // Leader election: the Lease is namespace-local, so pair the ClusterRole
    // with a small Role+RoleBinding instead of a cluster-wide leases grant
    // (which would reach node-heartbeat and other controllers' Leases).
    let leader_role = Role {
        metadata: metadata(LEADER_ROLE_NAME, Some(DEFAULT_NAMESPACE)),
        rules: Some(leader_election_rules(DEFAULT_LEASE_NAME)),
    };
    let leader_binding = RoleBinding {
        metadata: metadata(LEADER_ROLE_NAME, Some(DEFAULT_NAMESPACE)),
        role_ref: RoleRef {
            api_group: "rbac.authorization.k8s.io".to_string(),
            kind: "Role".to_string(),
            name: LEADER_ROLE_NAME.to_string(),
        },
        subjects: Some(vec![Subject {
            kind: "ServiceAccount".to_string(),
            name: SA_NAME.to_string(),
            namespace: Some(DEFAULT_NAMESPACE.to_string()),
            api_group: None,
        }]),
    };

    let content = document(&[
        render(&sa)?,
        render(&clusterrole)?,
        render(&binding)?,
        render(&leader_role)?,
        render(&leader_binding)?,
    ]);
    Ok(Artifact::new(
        "rbac/operator-clusterrole.yaml".to_string(),
        content,
    ))
}

/// Generate the namespaced RBAC artifact (§4.11/§4.12 namespaced-install mode).
fn namespaced_artifact() -> Result<Artifact> {
    // No cluster-scoped clusterrepositories. The controller still mints the mover
    // SA + RoleBinding in the (single) workload namespace, so the SA/rolebinding
    // minting rules are retained.
    let mut rules = kopia_crd_rules(false);
    rules.extend(workload_rules());
    // Self-managed webhook TLS (`webhook.tls.mode: self`): only the Secret write
    // is namespace-local and fits a Role. The cluster-scoped webhook-config patch
    // cannot live in a Role — a self-mode namespaced install pairs this Role with
    // the small ClusterRole the chart renders.
    rules.extend(webhook_cert_secret_rules());
    // Leader election Lease (lives in the release namespace, so a Role covers it).
    rules.extend(leader_election_rules(DEFAULT_LEASE_NAME));

    let role = Role {
        metadata: metadata(ROLE_NAME, Some(DEFAULT_NAMESPACE)),
        rules: Some(rules),
    };

    let sa = ServiceAccount {
        metadata: metadata(SA_NAME, Some(DEFAULT_NAMESPACE)),
        ..Default::default()
    };

    let binding = RoleBinding {
        metadata: metadata(ROLE_NAME, Some(DEFAULT_NAMESPACE)),
        role_ref: RoleRef {
            api_group: "rbac.authorization.k8s.io".to_string(),
            kind: "Role".to_string(),
            name: ROLE_NAME.to_string(),
        },
        subjects: Some(vec![Subject {
            kind: "ServiceAccount".to_string(),
            name: SA_NAME.to_string(),
            namespace: Some(DEFAULT_NAMESPACE.to_string()),
            api_group: None,
        }]),
    };

    let content = document(&[render(&sa)?, render(&role)?, render(&binding)?]);
    Ok(Artifact::new(
        "rbac/operator-role.yaml".to_string(),
        content,
    ))
}

/// Generate the cluster-scoped mover `ClusterRole`. The per-namespace
/// `kopiur-mover` ServiceAccount and its RoleBinding are minted by the controller
/// at runtime (in each mover Job's namespace), so only the role is shipped.
fn mover_cluster_artifact() -> Result<Artifact> {
    let clusterrole = ClusterRole {
        metadata: metadata(MOVER_CLUSTERROLE_NAME, None),
        rules: Some(mover_rules(true)),
        ..Default::default()
    };
    // The dedicated snapshot-replication mover role ships alongside (same
    // runtime minting model: SA + RoleBinding created per namespace by the
    // controller, only for snapshot-replication Jobs).
    let srepl_clusterrole = ClusterRole {
        metadata: metadata(SNAPSHOT_REPLICATION_MOVER_NAME, None),
        rules: Some(snapshot_replication_mover_rules()),
        ..Default::default()
    };
    let content = document(&[render(&clusterrole)?, render(&srepl_clusterrole)?]);
    Ok(Artifact::new(
        "rbac/mover-clusterrole.yaml".to_string(),
        content,
    ))
}

/// Generate the namespaced mover `Role` (namespaced-install mode). Same minimal
/// rules as the ClusterRole MINUS `clusterrepositories/status` (see
/// [`mover_rules`] — an entry the binder can't hold trips RBAC escalation
/// prevention on the controller's runtime RoleBinding mint); the controller
/// mints the SA + RoleBinding in the workload namespace at runtime.
fn mover_namespaced_artifact() -> Result<Artifact> {
    let role = Role {
        metadata: metadata(MOVER_ROLE_NAME, Some(DEFAULT_NAMESPACE)),
        rules: Some(mover_rules(false)),
    };
    // Same rules in both flavours: everything the snapshot-replication mover
    // touches is namespaced, so no `clusterrepositories/status`-style
    // escalation-prevention carve-out applies here.
    let srepl_role = Role {
        metadata: metadata(SNAPSHOT_REPLICATION_MOVER_NAME, Some(DEFAULT_NAMESPACE)),
        rules: Some(snapshot_replication_mover_rules()),
    };
    let content = document(&[render(&role)?, render(&srepl_role)?]);
    Ok(Artifact::new("rbac/mover-role.yaml".to_string(), content))
}

// =============================================================================
// kopiur-ui (the web console)
// =============================================================================
//
// `kopiur-ui` never acts as itself on a user's behalf: it reads trusted identity
// headers from an authenticating proxy and IMPERSONATES that user on every
// apiserver call, so Kubernetes RBAC — not the console — decides what each
// person may see and do. That design concentrates all of the risk in one place.
// A ServiceAccount that may impersonate AND read Secrets can read every
// repository credential in the cluster while appearing to be nobody in
// particular, so the rules below are a short allow-list guarded by tests that
// assert the ABSENCES (`crates/xtask/tests/ui_rbac.rs`).
//
// Four human roles ship alongside it. `kopiur-ui-viewer` and `kopiur-ui-editor`
// aggregate into `kopiur-ui-user`, the everyday grant. `kopiur-ui-browse` does
// NOT aggregate, and that is deliberate — see [`ui_browse_rules`].

/// ServiceAccount + ClusterRole name for the console itself.
const UI_SA_NAME: &str = "kopiur-ui";
const UI_CLUSTERROLE_NAME: &str = "kopiur-ui";
/// Read-only human role (aggregates).
const UI_VIEWER_NAME: &str = "kopiur-ui-viewer";
/// Action human role (aggregates).
const UI_EDITOR_NAME: &str = "kopiur-ui-editor";
/// The umbrella role administrators actually bind; its rules come from the
/// aggregation, so it ships with none of its own.
const UI_USER_NAME: &str = "kopiur-ui-user";
/// File-browsing role. Unaggregated on purpose — see [`ui_browse_rules`].
const UI_BROWSE_NAME: &str = "kopiur-ui-browse";
/// Namespaced companion Role for doctor's controller check.
const UI_DOCTOR_ROLE_NAME: &str = "kopiur-ui-doctor";

/// Label that folds a ClusterRole into [`UI_USER_NAME`]. Kopiur-domained rather
/// than `rbac.authorization.k8s.io/aggregate-to-*` so it can never widen one of
/// Kubernetes' own built-in roles by accident.
pub const UI_AGGREGATE_LABEL: &str = "rbac.kopiur.home-operations.com/aggregate-to-ui-user";

/// The one `system:` group an impersonated identity always carries. Kubernetes
/// adds it to every authenticated request, so a name-scoped `groups` rule that
/// omits it rejects EVERY impersonation the UI attempts — a fail-closed trap
/// that presents as a broken console rather than as a missing RBAC entry.
const SYSTEM_AUTHENTICATED: &str = "system:authenticated";

/// API group owning the `userextras/<key>` impersonation resources.
const AUTHENTICATION_GROUP: &str = "authentication.k8s.io";

/// The seven kinds a console action PATCHes: suspend/resume, run maintenance,
/// run replication, scan catalog. Mirrors the capability probe table in
/// `crates/ui/src/api/me.rs` — the UI asks whether the user may `patch` exactly
/// these, so a kind here that is not there (or vice versa) is a bug on one side.
const UI_PATCHED_CRDS: &[&str] = &[
    "snapshotpolicies",
    "snapshotschedules",
    "repositories",
    "clusterrepositories",
    "maintenances",
    "repositoryreplications",
    "snapshotreplications",
];

/// The fixed identity an anonymous-mode console runs every request as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiAnonymous {
    /// Username (`KOPIUR_UI_ANONYMOUS_USER`).
    pub user: String,
    /// Groups paired with it (`KOPIUR_UI_ANONYMOUS_GROUPS`).
    pub groups: Vec<String>,
}

/// All nine Kopiur CRD plurals, cluster-scoped one included.
fn all_kopia_crds() -> Vec<String> {
    NAMESPACED_CRDS
        .iter()
        .chain(CLUSTER_CRDS.iter())
        .map(|s| (*s).to_string())
        .collect()
}

/// Sort + dedupe a `resourceNames` list so the generated YAML is stable
/// regardless of the order the values arrived in.
fn sorted_names(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v.dedup();
    v
}

/// The console ServiceAccount's own rules.
///
/// * `extra_keys` — each becomes its OWN `userextras/<key>` resource. There is
///   no wildcard form: `userextras/*` is accepted by the API and authorizes
///   nothing, so an install that used it would fail at the first request with a
///   403 naming a header rather than a rule.
/// * `allowed_groups` — when the deployment restricts which groups may be
///   impersonated, the same list is pinned as `resourceNames` so the apiserver
///   enforces it too and a bug in the UI's own filter is still caught.
/// * `anonymous_only` — an anonymous-mode console has exactly one identity, so
///   both impersonate rules are pinned to it by name. Even a fooled header
///   parser then cannot reach a second user.
/// * `cache` — reads are served from watch-fed stores under this ServiceAccount
///   and filtered per user with `SubjectAccessReview`s. That is the ONLY reason
///   the console ever reads a Kopiur object as itself, so with the cache off the
///   grant disappears rather than lingering unused.
///
/// Never `secrets` and never `pods/exec`: browse execs as the *user*, and the
/// session pod — not the console — loads the repository credentials.
pub fn ui_rules(
    extra_keys: &[String],
    allowed_groups: &[String],
    anonymous_only: Option<&UiAnonymous>,
    cache: bool,
) -> Vec<PolicyRule> {
    let mut rules = Vec::new();

    // Users. Pinned by name only in anonymous mode, where there is exactly one.
    rules.push(match anonymous_only {
        Some(anon) => rule_named(
            &[""],
            &["users".into()],
            &["impersonate"],
            std::slice::from_ref(&anon.user),
        ),
        None => rule(&[""], &["users".into()], &["impersonate"]),
    });

    // Groups. Pinned when anonymous mode or an explicit allow-list narrows them;
    // `system:authenticated` is always included or nothing authorizes at all.
    let pinned_groups: Option<Vec<String>> = match (anonymous_only, allowed_groups.is_empty()) {
        (Some(anon), _) => {
            let mut g = anon.groups.clone();
            g.push(SYSTEM_AUTHENTICATED.to_string());
            Some(sorted_names(g))
        }
        (None, false) => {
            let mut g = allowed_groups.to_vec();
            g.push(SYSTEM_AUTHENTICATED.to_string());
            Some(sorted_names(g))
        }
        (None, true) => None,
    };
    rules.push(match pinned_groups {
        Some(g) => rule_named(&[""], &["groups".into()], &["impersonate"], &g),
        None => rule(&[""], &["groups".into()], &["impersonate"]),
    });

    // One rule per configured extra key. Never a wildcard.
    for key in extra_keys {
        rules.push(rule(
            &[AUTHENTICATION_GROUP],
            &[format!("userextras/{key}")],
            &["impersonate"],
        ));
    }

    if cache {
        // Read-only, and only the Kopiur kinds: the stores hold nothing else.
        rules.push(rule(&[KOPIA_GROUP], &all_kopia_crds(), READ_VERBS));
        // Every cached read is gated by a SubjectAccessReview for the calling
        // identity, so Kubernetes RBAC still decides what each person sees.
        rules.push(rule(
            &["authorization.k8s.io"],
            &["subjectaccessreviews".into()],
            &["create"],
        ));
    }

    rules
}

/// `kopiur-ui-viewer`: everything the console renders, and nothing it writes.
///
/// The CRD read backs doctor's schema comparison; the `events.k8s.io` list backs
/// the per-object event feed (the console reads `events.k8s.io/v1`, the group
/// kube's Recorder writes, so the legacy core group is deliberately not granted).
pub fn ui_viewer_rules() -> Vec<PolicyRule> {
    vec![
        rule(&[KOPIA_GROUP], &all_kopia_crds(), READ_VERBS),
        rule(
            &["apiextensions.k8s.io"],
            &["customresourcedefinitions".into()],
            &["get", "list"],
        ),
        rule(&["events.k8s.io"], &["events".into()], &["list"]),
    ]
}

/// `kopiur-ui-editor`: exactly the verbs behind the console's buttons.
///
/// `create`/`delete` on `snapshots` is "snapshot now" and "delete snapshot";
/// `create` on `restores` is the restore dialog; `patch` on the seven
/// [`UI_PATCHED_CRDS`] is suspend/resume, run-maintenance, run-replication and
/// scan-catalog; `list` on PVCs is what the snapshot-now dialog expands a
/// `pvcSelector` against. Nothing here grants `update` — every console write is
/// a merge patch — and nothing grants a read the viewer role does not already
/// carry, so the pair is additive by construction.
pub fn ui_editor_rules() -> Vec<PolicyRule> {
    vec![
        rule(&[KOPIA_GROUP], &["snapshots".into()], &["create", "delete"]),
        rule(&[KOPIA_GROUP], &["restores".into()], &["create"]),
        rule(
            &[KOPIA_GROUP],
            &UI_PATCHED_CRDS
                .iter()
                .map(|s| (*s).to_string())
                .collect::<Vec<_>>(),
            &["patch"],
        ),
        rule(&[""], &["persistentvolumeclaims".into()], &["list"]),
    ]
}

/// `kopiur-ui-browse`: the read-only file browser inside a snapshot.
///
/// **Granting this grants that namespace's repository credentials.** The browse
/// data plane runs a session pod and `pods/exec`s kopia commands into it, and
/// that pod loads the repository credentials through `envFrom` — so anyone who
/// can exec into the namespace can simply print them with `env`. `pods/exec
/// create` is not name-scopable, so RBAC cannot narrow it to the session pod.
///
/// That is why this role ships **unaggregated** and is documented as
/// binding-only: an administrator must bind it on purpose, ideally with a
/// namespaced RoleBinding, rather than receive it by holding `kopiur-ui-user`.
/// (`ui.rbac.execPolicy` renders a `ValidatingAdmissionPolicy` narrowing the exec
/// to `kopiur-browse-*` pods running kopia — the enforcement RBAC cannot express.)
///
/// Differs from the CLI's `<release>-browse` role by dropping `apps/deployments`:
/// the console passes the mover image through `KOPIUR_MOVER_IMAGE`, so a browsing
/// user never needs to discover it from the controller Deployment.
pub fn ui_browse_rules() -> Vec<PolicyRule> {
    vec![
        // Resolve the Snapshot → repository chain.
        rule(
            &[KOPIA_GROUP],
            &["snapshots".into(), "repositories".into()],
            &["get", "list"],
        ),
        rule(&[KOPIA_GROUP], &["clusterrepositories".into()], &["get"]),
        // The session Job (find-or-create, end).
        rule(
            &["batch"],
            &["jobs".into()],
            &["create", "get", "list", "delete"],
        ),
        // `get`: a repository's `tls.caBundleRef` CA-bundle ConfigMap.
        // `delete`: reap a LEGACY session's work-spec ConfigMap.
        rule(&[""], &["configmaps".into()], &["get", "delete"]),
        // Wait for the session pod to become Ready; surface its logs on failure.
        rule(&[""], &["pods".into()], READ_VERBS),
        rule(&[""], &["pods/log".into()], &["get"]),
        // The read path itself.
        rule(&[""], &["pods/exec".into()], &["create"]),
    ]
}

/// `kopiur-ui-doctor`: a namespaced Role in the operator's namespace granting
/// the one `apps` read doctor's `ControllerRunning` check needs.
///
/// Namespaced rather than folded into the viewer role because a cluster-wide
/// Deployment read is a real grant (every workload's image, env var names and
/// replica counts) to buy one health line. Without this Role bound, doctor
/// degrades that check to a Warn — it does not fail.
pub fn ui_doctor_role_rules() -> Vec<PolicyRule> {
    vec![rule(&["apps"], &["deployments".into()], &["get", "list"])]
}

/// [`metadata`] plus extra labels (the aggregation marker).
fn metadata_labeled(name: &str, namespace: Option<&str>, extra: &[(&str, &str)]) -> ObjectMeta {
    let mut meta = metadata(name, namespace);
    let labels = meta.labels.get_or_insert_with(Default::default);
    for (k, v) in extra {
        labels.insert((*k).to_string(), (*v).to_string());
    }
    meta
}

/// Generate `deploy/rbac/ui.yaml`.
///
/// Like the operator's own artifact this is the maximal, "every feature on"
/// flavour — cache enabled, impersonation unscoped (header mode), no extra keys
/// configured — and the Helm chart is what narrows it per install. The static
/// file exists for a chartless `kubectl apply` and as the reviewable baseline the
/// chart's templates are kept in sync with.
fn ui_artifact() -> Result<Artifact> {
    let sa = ServiceAccount {
        metadata: metadata(UI_SA_NAME, Some(DEFAULT_NAMESPACE)),
        ..Default::default()
    };

    let clusterrole = ClusterRole {
        metadata: metadata(UI_CLUSTERROLE_NAME, None),
        rules: Some(ui_rules(&[], &[], None, true)),
        ..Default::default()
    };

    let binding = ClusterRoleBinding {
        metadata: metadata(UI_CLUSTERROLE_NAME, None),
        role_ref: RoleRef {
            api_group: "rbac.authorization.k8s.io".to_string(),
            kind: "ClusterRole".to_string(),
            name: UI_CLUSTERROLE_NAME.to_string(),
        },
        subjects: Some(vec![Subject {
            kind: "ServiceAccount".to_string(),
            name: UI_SA_NAME.to_string(),
            namespace: Some(DEFAULT_NAMESPACE.to_string()),
            api_group: None,
        }]),
    };

    let aggregate = [(UI_AGGREGATE_LABEL, "true")];
    let viewer = ClusterRole {
        metadata: metadata_labeled(UI_VIEWER_NAME, None, &aggregate),
        rules: Some(ui_viewer_rules()),
        ..Default::default()
    };
    let editor = ClusterRole {
        metadata: metadata_labeled(UI_EDITOR_NAME, None, &aggregate),
        rules: Some(ui_editor_rules()),
        ..Default::default()
    };
    // The umbrella role: rules are filled in by the apiserver's aggregation
    // controller, so `rules` stays absent here on purpose.
    let user = ClusterRole {
        metadata: metadata(UI_USER_NAME, None),
        aggregation_rule: Some(AggregationRule {
            cluster_role_selectors: Some(vec![LabelSelector {
                match_labels: Some(std::collections::BTreeMap::from([(
                    UI_AGGREGATE_LABEL.to_string(),
                    "true".to_string(),
                )])),
                ..Default::default()
            }]),
        }),
        rules: None,
    };
    // Deliberately NOT labelled for aggregation — see `ui_browse_rules`.
    let browse = ClusterRole {
        metadata: metadata(UI_BROWSE_NAME, None),
        rules: Some(ui_browse_rules()),
        ..Default::default()
    };
    let doctor = Role {
        metadata: metadata(UI_DOCTOR_ROLE_NAME, Some(DEFAULT_NAMESPACE)),
        rules: Some(ui_doctor_role_rules()),
    };

    let content = document(&[
        render(&sa)?,
        render(&clusterrole)?,
        render(&binding)?,
        render(&viewer)?,
        render(&editor)?,
        render(&user)?,
        render(&browse)?,
        render(&doctor)?,
    ]);
    Ok(Artifact::new("rbac/ui.yaml".to_string(), content))
}

/// All RBAC artifacts.
pub fn artifacts() -> Result<Vec<Artifact>> {
    Ok(vec![
        cluster_artifact()?,
        namespaced_artifact()?,
        mover_cluster_artifact()?,
        mover_namespaced_artifact()?,
        ui_artifact()?,
    ])
}
