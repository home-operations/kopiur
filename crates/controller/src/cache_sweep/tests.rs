use super::*;

const MIN_AGE: i64 = 0;
const OLD: Option<i64> = Some(24 * 60 * 60);

fn claim(labels: serde_json::Value, annotations: serde_json::Value) -> PersistentVolumeClaim {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "v1", "kind": "PersistentVolumeClaim",
        "metadata": {
            "name": "kopiur-cache-pg-94c57734fbfe76da", "namespace": "kopiur-system",
            "labels": labels, "annotations": annotations,
        },
    }))
    .expect("PVC")
}

fn policy(spec: serde_json::Value) -> SnapshotPolicy {
    serde_json::from_value(serde_json::json!({
        "apiVersion": crate::consts::API_VERSION, "kind": "SnapshotPolicy",
        "metadata": { "name": "pg", "namespace": "billing", "uid": "uid-pg" },
        "spec": spec,
    }))
    .expect("SnapshotPolicy")
}

/// A single-repository policy with a sized persistent cache.
fn persistent_policy() -> SnapshotPolicy {
    policy(serde_json::json!({
        "repository": { "kind": "ClusterRepository", "name": "shared" },
        "sources": [ { "pvc": { "name": "data" } } ],
        "mover": { "cache": { "capacity": "5Gi", "mode": "Persistent" } },
    }))
}

fn cluster_repo(spec: serde_json::Value) -> ClusterRepository {
    let mut base = serde_json::json!({
        "backend": { "s3": { "bucket": "b" } },
        "encryption": { "passwordSecretRef": { "name": "creds", "namespace": "kopiur-system" } },
        "allowedNamespaces": { "all": true },
    });
    for (k, v) in spec.as_object().expect("object") {
        base[k] = v.clone();
    }
    serde_json::from_value(serde_json::json!({
        "apiVersion": crate::consts::API_VERSION, "kind": "ClusterRepository",
        "metadata": { "name": "shared" },
        "spec": base,
    }))
    .expect("ClusterRepository")
}

fn central_repo() -> ClusterRepository {
    cluster_repo(serde_json::json!({ "execution": { "mode": "Central" } }))
}

#[test]
fn the_owner_is_read_off_the_claims_marks() {
    let policys = claim(
        serde_json::json!({ CENTRAL_POLICY_UID_LABEL: "uid-pg" }),
        serde_json::json!({ CENTRAL_POLICY_ANNOTATION: "billing/pg" }),
    );
    assert_eq!(
        claim_owner(&policys),
        Some(ClaimOwner {
            kind: ClaimOwnerKind::SnapshotPolicy,
            namespace: "billing".into(),
            name: "pg".into(),
            uid: "uid-pg".into(),
        })
    );
    let restores = claim(
        serde_json::json!({ CENTRAL_RESTORE_UID_LABEL: "uid-r" }),
        serde_json::json!({ CENTRAL_RESTORE_ANNOTATION: "billing/pg-restore" }),
    );
    assert_eq!(
        claim_owner(&restores).map(|o| (o.kind, o.name)),
        Some((ClaimOwnerKind::Restore, "pg-restore".to_string()))
    );
    // An owner-referenced cache of a ClusterRepository lives in the operator
    // namespace too. It is not a central claim, and the sweep never touches it.
    assert_eq!(
        claim_owner(&claim(serde_json::json!({}), serde_json::json!({}))),
        None
    );
    // Incomplete marks: never guess an owner.
    assert_eq!(
        claim_owner(&claim(
            serde_json::json!({ CENTRAL_POLICY_UID_LABEL: "uid-pg" }),
            serde_json::json!({})
        )),
        None
    );
    assert_eq!(
        claim_owner(&claim(
            serde_json::json!({ CENTRAL_POLICY_UID_LABEL: "uid-pg" }),
            serde_json::json!({ CENTRAL_POLICY_ANNOTATION: "no-slash" })
        )),
        None
    );
}

/// The whole decision table. A claim is deleted only on positive evidence that
/// nothing will mount it again.
#[test]
fn a_claim_is_deleted_only_on_positive_evidence() {
    use OwnerState::*;
    for (state, delete) in [
        (Gone, true),
        (Terminating, false),
        (LiveRestore, false),
        (LivePolicy(PolicyUse::InUse), false),
        (LivePolicy(PolicyUse::Unknown), false),
        (LivePolicy(PolicyUse::RepositoryDropped), true),
        (LivePolicy(PolicyUse::NotCentral), true),
        (LivePolicy(PolicyUse::NotPersistent), true),
        (LivePolicy(PolicyUse::SourceNotSelected), true),
    ] {
        let verdict = claim_verdict(OLD, MIN_AGE, state);
        assert_eq!(
            matches!(verdict, Verdict::Delete(_)),
            delete,
            "{state:?} -> {verdict:?}"
        );
    }
}

/// A run creates its claim before a cached view has caught up with the spec
/// edit that made the run want it. A young claim is never judged, whatever the
/// configured min-age.
#[test]
fn a_young_claim_is_never_deleted() {
    let young = Some(CACHE_MIN_AGE_FLOOR_SECS - 1);
    assert_eq!(claim_verdict(young, 0, OwnerState::Gone), Verdict::Keep);
    assert_eq!(claim_verdict(None, 0, OwnerState::Gone), Verdict::Keep);
    assert!(matches!(
        claim_verdict(Some(CACHE_MIN_AGE_FLOOR_SECS), 0, OwnerState::Gone),
        Verdict::Delete(_)
    ));
    // A larger configured min-age wins over the floor.
    assert_eq!(
        claim_verdict(
            Some(CACHE_MIN_AGE_FLOOR_SECS),
            2 * CACHE_MIN_AGE_FLOOR_SECS,
            OwnerState::Gone
        ),
        Verdict::Keep
    );
}

#[test]
fn a_policy_uses_its_claim_while_the_repository_is_central_and_the_cache_persistent() {
    let central = central_repo();
    assert_eq!(
        policy_use(&persistent_policy(), Some("shared"), Some(&central), None),
        PolicyUse::InUse
    );
    // The repository's moverDefaults supply the cache.
    let from_defaults = cluster_repo(serde_json::json!({
        "execution": { "mode": "Central" },
        "moverDefaults": { "cache": { "capacity": "5Gi", "mode": "Persistent" } },
    }));
    let bare = policy(serde_json::json!({
        "repository": { "kind": "ClusterRepository", "name": "shared" },
        "sources": [ { "pvc": { "name": "data" } } ],
    }));
    assert_eq!(
        policy_use(&bare, Some("shared"), Some(&from_defaults), None),
        PolicyUse::InUse
    );
    // A per-source claim whose source PVC exists.
    assert_eq!(
        policy_use(
            &persistent_policy(),
            Some("shared"),
            Some(&central),
            Some(true)
        ),
        PolicyUse::InUse
    );
}

#[test]
fn a_policy_stops_using_its_claim_for_each_of_four_reasons() {
    let central = central_repo();
    // 1. The policy points at another repository now.
    assert_eq!(
        policy_use(&persistent_policy(), Some("old-repo"), Some(&central), None),
        PolicyUse::RepositoryDropped
    );
    // A namespaced Repository of the same name is not the ClusterRepository.
    let namespaced = policy(serde_json::json!({
        "repository": { "kind": "Repository", "name": "shared" },
        "sources": [ { "pvc": { "name": "data" } } ],
        "mover": { "cache": { "capacity": "5Gi", "mode": "Persistent" } },
    }));
    assert_eq!(
        policy_use(&namespaced, Some("shared"), Some(&central), None),
        PolicyUse::RepositoryDropped
    );
    // 2. The repository runs movers in the workload namespace again.
    for spec in [
        serde_json::json!({}),
        serde_json::json!({ "execution": { "mode": "InNamespace" } }),
    ] {
        assert_eq!(
            policy_use(
                &persistent_policy(),
                Some("shared"),
                Some(&cluster_repo(spec)),
                None
            ),
            PolicyUse::NotCentral
        );
    }
    // 3. The cache is no longer a sized persistent one.
    for mover in [
        serde_json::json!({ "cache": { "capacity": "5Gi", "mode": "Ephemeral" } }),
        serde_json::json!({ "cache": { "capacity": "5Gi" } }),
        serde_json::json!({ "cache": { "mode": "Persistent" } }),
        serde_json::json!({}),
    ] {
        let changed = policy(serde_json::json!({
            "repository": { "kind": "ClusterRepository", "name": "shared" },
            "sources": [ { "pvc": { "name": "data" } } ],
            "mover": mover,
        }));
        assert_eq!(
            policy_use(&changed, Some("shared"), Some(&central), None),
            PolicyUse::NotPersistent,
            "{:?}",
            changed.spec.mover
        );
    }
    // 4. The source PVC of a per-source claim is gone, or not selected any more.
    assert_eq!(
        policy_use(
            &persistent_policy(),
            Some("shared"),
            Some(&central),
            Some(false)
        ),
        PolicyUse::SourceNotSelected
    );
}

/// What cannot be read is never taken as "unused".
#[test]
fn an_unreadable_claim_or_repository_is_unknown() {
    let central = central_repo();
    assert_eq!(
        policy_use(&persistent_policy(), None, Some(&central), None),
        PolicyUse::Unknown
    );
    // The repository does not exist right now (deleted and about to be
    // re-created by GitOps, say). Its cache is still valid when it returns.
    assert_eq!(
        policy_use(&persistent_policy(), Some("shared"), None, None),
        PolicyUse::Unknown
    );
}

#[test]
fn the_in_namespace_predecessor_is_the_claim_the_policy_used_beside_itself() {
    assert_eq!(
        in_namespace_predecessor(&persistent_policy(), "shared").as_deref(),
        Some("kopiur-cache-pg")
    );
    assert_eq!(
        in_namespace_predecessor(&persistent_policy(), "other"),
        None
    );
    // A multi-repository policy had one claim for each repository.
    let multi = policy(serde_json::json!({
        "repositories": [
            { "kind": "ClusterRepository", "name": "shared" },
            { "kind": "ClusterRepository", "name": "offsite" },
        ],
        "sources": [ { "pvc": { "name": "data" } } ],
    }));
    let shared = in_namespace_predecessor(&multi, "shared").expect("a name");
    assert_eq!(
        shared,
        kopiur_api::expand::cache_pvc_name(
            "pg",
            "billing",
            Some(&kopiur_api::common::RepositoryRef {
                kind: RepositoryKind::ClusterRepository,
                name: "shared".into(),
                namespace: None,
            })
        )
    );
    assert_ne!(Some(shared), in_namespace_predecessor(&multi, "offsite"));
}

/// The predecessor is deleted by name, and a cache reset deletes every claim
/// this selects, so it must select this policy's own cache and nothing else:
/// never a source PVC, a staged PVC, or another owner's cache.
#[test]
fn only_this_policys_owner_referenced_cache_is_a_predecessor() {
    let pvc = |labels: serde_json::Value, kind: &str, uid: &str| {
        serde_json::from_value::<PersistentVolumeClaim>(serde_json::json!({
            "apiVersion": "v1", "kind": "PersistentVolumeClaim",
            "metadata": {
                "name": "kopiur-cache-pg", "labels": labels,
                "ownerReferences": [{
                    "apiVersion": crate::consts::API_VERSION, "kind": kind,
                    "name": "pg", "uid": uid,
                }],
            },
        }))
        .expect("PVC")
    };
    let cache = serde_json::json!({ CACHE_PVC_COMPONENT_LABEL: CACHE_PVC_COMPONENT });
    assert!(is_in_namespace_cache_of(
        &pvc(cache.clone(), "SnapshotPolicy", "uid-pg"),
        "uid-pg"
    ));
    // A user's own PVC that happens to carry the name, another policy's cache,
    // and a Maintenance cache (`kopiur-cache-<maintenance>`).
    assert!(!is_in_namespace_cache_of(
        &pvc(serde_json::json!({}), "SnapshotPolicy", "uid-pg"),
        "uid-pg"
    ));
    assert!(!is_in_namespace_cache_of(
        &pvc(cache.clone(), "SnapshotPolicy", "uid-other"),
        "uid-pg"
    ));
    assert!(!is_in_namespace_cache_of(
        &pvc(cache, "Maintenance", "uid-pg"),
        "uid-pg"
    ));
}

// --- one pass against a mock apiserver ----------------------------------------

/// One pass against a mock apiserver that holds two central claims: one of a
/// policy that is gone, and one of the live policy `billing/pg`, which still
/// uses it and left the claim `kopiur-cache-pg` beside itself. `runs` are the
/// `Snapshot`s of that policy. Returns the outcome and every request as
/// `METHOD path body`.
async fn one_pass(runs: serde_json::Value) -> (CacheSweepOutcome, Vec<String>) {
    use http::{Request, Response, StatusCode};
    use kube::client::Body;
    use std::sync::{Arc, Mutex};

    let central_claim = |name: &str, pvc_uid: &str, policy: &str, policy_uid: &str| {
        serde_json::json!({
            "metadata": {
                "name": name, "namespace": "kopiur-system", "uid": pvc_uid,
                "creationTimestamp": "2020-01-01T00:00:00Z",
                "labels": {
                    CACHE_PVC_COMPONENT_LABEL: CACHE_PVC_COMPONENT,
                    CENTRAL_POLICY_UID_LABEL: policy_uid,
                },
                "annotations": {
                    CENTRAL_POLICY_ANNOTATION: format!("billing/{policy}"),
                    CACHE_REPOSITORY_ANNOTATION: "shared",
                },
            },
        })
    };
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let svc = {
        let log = log.clone();
        tower::service_fn(move |req: Request<Body>| {
            let (log, runs) = (log.clone(), runs.clone());
            let listed = serde_json::json!({
                "apiVersion": "v1", "kind": "PersistentVolumeClaimList", "metadata": {},
                "items": [
                    central_claim("kopiur-cache-gone-1", "pvc-orphan", "gone", "uid-gone"),
                    central_claim("kopiur-cache-pg-2", "pvc-in-use", "pg", "uid-pg"),
                ],
            });
            async move {
                let method = req.method().as_str().to_string();
                let path = req.uri().path().to_string();
                let bytes = http_body_util::BodyExt::collect(req.into_body())
                    .await
                    .expect("collect request body")
                    .to_bytes();
                log.lock().unwrap().push(format!(
                    "{method} {path} {}",
                    String::from_utf8_lossy(&bytes)
                ));
                let ok = |v: serde_json::Value| (StatusCode::OK, v);
                let (status, json) = match (method.as_str(), path.as_str()) {
                    ("GET", "/api/v1/namespaces/kopiur-system/persistentvolumeclaims") => {
                        ok(listed)
                    }
                    ("GET", p) if p.ends_with("/namespaces/billing/snapshotpolicies/pg") => {
                        ok(serde_json::to_value(persistent_policy()).unwrap())
                    }
                    ("GET", p) if p.ends_with("/namespaces/billing/snapshots") => {
                        ok(serde_json::json!({
                            "apiVersion": crate::consts::API_VERSION, "kind": "SnapshotList",
                            "metadata": {}, "items": runs,
                        }))
                    }
                    ("GET", p) if p.ends_with("/clusterrepositories/shared") => {
                        ok(serde_json::to_value(central_repo()).unwrap())
                    }
                    // The claim the policy used beside itself before the
                    // repository ran centrally.
                    (
                        "GET",
                        "/api/v1/namespaces/billing/persistentvolumeclaims/kopiur-cache-pg",
                    ) => ok(serde_json::json!({
                        "apiVersion": "v1", "kind": "PersistentVolumeClaim",
                        "metadata": {
                            "name": "kopiur-cache-pg", "namespace": "billing",
                            "uid": "pvc-predecessor",
                            "labels": { CACHE_PVC_COMPONENT_LABEL: CACHE_PVC_COMPONENT },
                            "ownerReferences": [{
                                "apiVersion": crate::consts::API_VERSION,
                                "kind": "SnapshotPolicy", "name": "pg", "uid": "uid-pg",
                            }],
                        },
                    })),
                    ("DELETE", _) => ok(serde_json::json!({
                        "apiVersion": "v1", "kind": "PersistentVolumeClaim",
                        "metadata": { "name": "deleted" },
                    })),
                    // The policy `gone`, and anything else.
                    _ => (
                        StatusCode::NOT_FOUND,
                        serde_json::json!({
                            "kind": "Status", "apiVersion": "v1", "status": "Failure",
                            "reason": "NotFound", "code": 404,
                        }),
                    ),
                };
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&json).unwrap()))
                        .unwrap(),
                )
            }
        })
    };
    let client = kube::Client::new(svc, "kopiur-system");
    let outcome = run_cache_sweep(&client, "kopiur-system", 0)
        .await
        .expect("the pass");
    let log = log.lock().unwrap().clone();
    (outcome, log)
}

fn deletes(log: &[String]) -> Vec<&String> {
    log.iter().filter(|l| l.starts_with("DELETE")).collect()
}

/// A pass deletes the claim of an owner that is gone, keeps the claim of a
/// policy that still uses it, and deletes the claim that policy left beside
/// itself. Every delete carries the UID of the claim that was read.
#[tokio::test]
async fn a_pass_deletes_the_orphan_and_the_predecessor_and_keeps_the_claim_in_use() {
    let finished = serde_json::json!([{
        "metadata": { "name": "pg-1", "namespace": "billing" },
        "spec": { "policyRef": { "name": "pg" } },
        "status": { "phase": "Succeeded" },
    }]);
    let (outcome, log) = one_pass(finished).await;
    assert_eq!(
        outcome,
        CacheSweepOutcome {
            central: 1,
            predecessors: 1
        }
    );
    let deletes = deletes(&log);
    assert_eq!(deletes.len(), 2, "{log:?}");
    assert!(
        deletes[0].contains("/namespaces/kopiur-system/persistentvolumeclaims/kopiur-cache-gone-1")
            && deletes[0].contains(r#""preconditions":{"uid":"pvc-orphan"}"#),
        "the orphan is deleted, by UID: {}",
        deletes[0]
    );
    assert!(
        deletes[1].contains("/namespaces/billing/persistentvolumeclaims/kopiur-cache-pg")
            && deletes[1].contains(r#""preconditions":{"uid":"pvc-predecessor"}"#),
        "the in-namespace predecessor is deleted, by UID: {}",
        deletes[1]
    );
    assert!(
        !log.iter()
            .any(|l| l.starts_with("DELETE") && l.contains("kopiur-cache-pg-2")),
        "a claim in use is never deleted: {log:?}"
    );
    assert!(
        log.iter()
            .any(|l| l.starts_with("GET") && l.contains("/namespaces/billing/snapshots")),
        "the policy's runs are read before its predecessor is deleted: {log:?}"
    );
}

/// A run that began beside the policy may be about to mount the predecessor.
/// Nothing of a policy is deleted while one of its runs is in flight. The
/// orphan has no runs to wait for.
#[tokio::test]
async fn a_pass_deletes_nothing_of_a_policy_with_a_run_in_flight() {
    let running = serde_json::json!([{
        "metadata": { "name": "pg-2", "namespace": "billing" },
        "spec": { "policyRef": { "name": "pg" } },
        "status": { "phase": "Running" },
    }]);
    let (outcome, log) = one_pass(running).await;
    assert_eq!(
        outcome,
        CacheSweepOutcome {
            central: 1,
            predecessors: 0
        }
    );
    let deletes = deletes(&log);
    assert_eq!(deletes.len(), 1, "{log:?}");
    assert!(deletes[0].contains("kopiur-cache-gone-1"), "{}", deletes[0]);
}

/// A PVC that still exists but that no `pvcSelector` of the policy matches any
/// more gets no further runs, so its claim has no further use.
#[test]
fn a_source_counts_only_while_a_selector_of_the_policy_matches_it() {
    let source = |labels: serde_json::Value| -> PersistentVolumeClaim {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "v1", "kind": "PersistentVolumeClaim",
            "metadata": { "name": "data-a", "namespace": "billing", "labels": labels },
        }))
        .expect("PVC")
    };
    let selecting = |selector: serde_json::Value| {
        policy(serde_json::json!({
            "repository": { "kind": "ClusterRepository", "name": "shared" },
            "sources": [ { "pvcSelector": selector } ],
        }))
    };
    let by_label = selecting(serde_json::json!({
        "labelSelector": { "matchLabels": { "backup": "yes" } }
    }));
    assert!(policy_selects(
        &by_label,
        &source(serde_json::json!({ "backup": "yes" }))
    ));
    // The label was removed from the PVC, or the selector was changed.
    assert!(!policy_selects(&by_label, &source(serde_json::json!({}))));
    assert!(!policy_selects(
        &by_label,
        &source(serde_json::json!({ "backup": "no" }))
    ));
    // A selector with no labelSelector takes every PVC of the namespace.
    assert!(policy_selects(
        &selecting(serde_json::json!({})),
        &source(serde_json::json!({}))
    ));
    // The policy now names one PVC and has no selector at all.
    assert!(!policy_selects(
        &persistent_policy(),
        &source(serde_json::json!({ "backup": "yes" }))
    ));
}

/// A run picks its claim and then creates its Job. A claim deleted in between
/// leaves that run's pod waiting on a PVC that does not exist. So a reset and
/// the sweep wait for every run that is not finished.
#[test]
fn a_run_is_in_flight_until_it_is_terminal_or_being_deleted() {
    let run = |status: serde_json::Value, deleting: bool| -> kopiur_api::Snapshot {
        let mut v = serde_json::json!({
            "apiVersion": crate::consts::API_VERSION, "kind": "Snapshot",
            "metadata": { "name": "s", "namespace": "billing" },
            "spec": { "policyRef": { "name": "pg" } },
            "status": status,
        });
        if deleting {
            v["metadata"]["deletionTimestamp"] = "2026-01-01T00:00:00Z".into();
        }
        serde_json::from_value(v).expect("Snapshot")
    };
    for phase in ["Pending", "Running", "SomethingNew"] {
        assert!(
            run_in_flight(&run(serde_json::json!({ "phase": phase }), false)),
            "{phase}"
        );
    }
    // Created a moment ago: no phase yet.
    assert!(run_in_flight(&run(serde_json::json!({}), false)));
    for phase in ["Succeeded", "Failed", "Unchanged", "Discovered"] {
        assert!(
            !run_in_flight(&run(serde_json::json!({ "phase": phase }), false)),
            "{phase}"
        );
    }
    assert!(!run_in_flight(&run(
        serde_json::json!({ "phase": "Running" }),
        true
    )));
}
