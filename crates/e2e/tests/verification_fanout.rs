//! e2e: #456 — verification fans out per matched PVC.
//!
//! The reported symptom was `deep verify found no snapshot to restore for
//! source path ""` on a `SnapshotPolicy` whose source is a `pvcSelector`. The
//! verify run resolved its identity from `sources[0]`'s own `pvc`/`nfs`, and a
//! selector source has neither, so the kopia source path came out EMPTY.
//!
//! The quick tier was broken the same way but silently: kopia's
//! `ParseSourceInfo` treats `user@host:` as a relative filesystem path, matches
//! zero manifests and exits 0 — a false pass that still stamped
//! `status.lastVerified`. That silence is why this scenario asserts the
//! POSITIVE shape (one verify Job and one stamp PER member, and a flat
//! `lastVerified` equal to their MIN) rather than merely "a verify succeeded":
//! the pre-fix operator passes the latter.
//!
//! Lives in the `multi-pvc` CI shard, not `reshape-async`, because it needs TWO
//! labelled source PVCs and `reshape-async` provisions one.
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; driven by
//! `mise run //crates/e2e:test`. Skips gracefully without a cluster.

#![cfg(all(unix, feature = "e2e"))]

mod common;
use common::*;

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::PersistentVolumeClaim;
use kopiur_api::{Repository, Snapshot, SnapshotPolicy, SnapshotSchedule};
use kopiur_e2e::{E2E_NAMESPACE, Need, World, default_timeout, poll_interval, wait_until};
use kube::Api;
use kube::api::{DeleteParams, ListParams, PostParams};

/// The `backup=` label value this file owns.
///
/// DISTINCT from every other scenario's on purpose: the shard shares one
/// namespace, none of these tests deletes its PVCs, and a shared value would
/// make this policy's `pvcSelector` match a sibling's volumes — failing a count
/// assertion that has nothing to do with #456, and only in whichever order
/// nextest happened to pick. `multi_pvc_group.rs` uses `fanout`/`group`.
const LABEL_VALUE: &str = "vfyfanout";
/// The isolated kopia repo subpath this file owns. LOCKSTEP with
/// `kopiur_e2e::consts::REPO_SUBPATHS` and the `node-seed` list in
/// `crates/e2e/mise.toml` (a unit test enforces the pair).
const REPO_SUBPATH: &str = "vfyfanout";

const POLICY: &str = "e2e-vfyfan-policy";
const SCHEDULE: &str = "e2e-vfyfan-schedule";
const REPO: &str = "e2e-repo-vfyfanout";

/// The single-flight LIST selector the operator itself uses for this policy's
/// verify Jobs (component + instance), WITHOUT the member label — exactly as
/// `has_active_verify_job` spells it.
fn verify_selector() -> String {
    format!("app.kubernetes.io/component=verify,kopiur.home-operations.com/verify={POLICY}")
}

/// The distinct `verify-member` label values across this policy's verify Jobs.
async fn verify_member_labels(client: &kube::Client) -> Vec<String> {
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let mut found: Vec<String> = jobs
        .list(&ListParams::default().labels(&verify_selector()))
        .await
        .expect("list verify Jobs")
        .items
        .iter()
        .filter_map(|j| {
            j.metadata
                .labels
                .as_ref()
                .and_then(|l| l.get("kopiur.home-operations.com/verify-member"))
                .cloned()
        })
        .collect();
    found.sort();
    found.dedup();
    found
}

/// Two labelled hostPath-backed source PVCs. `copyMethod: Direct` reads them
/// live, so no CSI provisioner is needed for the fan-out itself.
async fn ensure_two_labelled_pvcs(client: &kube::Client) {
    use kopiur_e2e::apply::{Fixture, apply_all};
    use kopiur_e2e::builders;
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    for (pv, pvc) in [
        ("e2e-vfyfan-pv-a", "e2e-vfyfan-a"),
        ("e2e-vfyfan-pv-b", "e2e-vfyfan-b"),
    ] {
        let fixtures: Vec<Fixture> = vec![
            builders::hostpath_pv(pv, kopiur_e2e::consts::HOSTPATH_SRC, "1Gi").into(),
            builders::static_pvc(E2E_NAMESPACE, pvc, pv, "1Gi").into(),
        ];
        apply_all(client, &fixtures)
            .await
            .expect("verification fan-out source PVCs");
        // The selector matches on this label; the builder does not set it. A
        // plain merge patch, NOT `PatchParams::apply(..).force()` — kube
        // rejects `force` on anything but `Patch::Apply`.
        let patch = serde_json::json!({
            "metadata": { "labels": { BACKUP_LABEL_KEY: LABEL_VALUE } }
        });
        pvcs.patch(pvc, &Default::default(), &kube::api::Patch::Merge(&patch))
            .await
            .expect("label the source PVC");
    }
}

/// A `pvcSelector` policy verifies EACH matched PVC's own kopia source path:
/// one verify Job and one `verificationStamps` entry per member, and the flat
/// `status.lastVerified` is the MIN across them.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn verification_fans_out_per_matched_pvc() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();
    ensure_repo(&client, REPO_SUBPATH).await;
    ensure_two_labelled_pvcs(&client).await;

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let schedules: Api<SnapshotSchedule> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    clear_scenario_leftovers(&client, SCHEDULE, POLICY).await;

    create_idempotent(
        &repos,
        &cr(repository_json(REPO, REPO_SUBPATH, serde_json::json!({}))),
        "create the verification fan-out Repository",
    )
    .await;
    wait_phase(&repos, REPO, "Ready").await.expect("repo Ready");

    // Quick verify every minute, with a successExpr that a PATHLESS run cannot
    // satisfy: the pre-fix operator matched zero manifests, so `stats.files`
    // fell back to 0 and this predicate would fail rather than false-pass.
    policies
        .create(
            &PostParams::default(),
            &cr(snapshot_policy_json(
                E2E_NAMESPACE,
                POLICY,
                "Repository",
                REPO,
                serde_json::json!({
                    "sources": [{
                        "pvcSelector": {
                            "labelSelector": { "matchLabels": { BACKUP_LABEL_KEY: LABEL_VALUE } }
                        },
                        "sourcePathStrategy": "PvcName"
                    }],
                    "groupBy": "None",
                    "copyMethod": "Direct",
                    "identity": { "username": "vfyfan", "hostname": "e2e" },
                    "verification": {
                        "quick": { "schedule": { "cron": "* * * * *" } },
                        "successExpr": "stats.files > 0 && stats.errors == 0"
                    }
                }),
            )),
        )
        .await
        .expect("a pvcSelector policy with verification must be ADMITTED");
    let landed = policies.get(POLICY).await.expect("read back the policy");
    assert!(
        landed.spec.sources.iter().any(|s| s.pvc_selector.is_some()),
        "the policy must carry a pvcSelector source or this test proves nothing: {:?}",
        landed.spec.sources
    );

    // The #168 gate defers verification until a backup exists, so fire one now.
    create_idempotent(
        &schedules,
        &cr(serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "SnapshotSchedule",
            "metadata": { "name": SCHEDULE, "namespace": E2E_NAMESPACE },
            "spec": {
                "policyRef": { "name": POLICY },
                "schedule": { "cron": "0 3 * * *", "runOnCreate": true }
            }
        })),
        "create the SnapshotSchedule",
    )
    .await;
    wait_until(
        "two fanned-out Snapshots",
        default_timeout(),
        poll_interval(),
        || async { Ok((children_of(&client, POLICY).await.len() == 2).then_some(())) },
    )
    .await
    .expect("a 2-PVC selector must produce exactly 2 Snapshots");
    for child in children_of(&client, POLICY).await {
        let name = child.metadata.name.clone().expect("named");
        wait_phase(&backups, &name, "Succeeded")
            .await
            .unwrap_or_else(|e| panic!("fanned-out Snapshot {name} must succeed: {e}"));
    }

    // ONE verify Job per member, each carrying its own member label. Before the
    // fix there was exactly one Job for the whole policy, with no member label
    // at all and a pathless `user@host:` identity.
    let members = wait_until(
        "two verify Jobs with distinct member labels",
        default_timeout(),
        poll_interval(),
        || async {
            let m = verify_member_labels(&client).await;
            Ok((m.len() == 2).then_some(m))
        },
    )
    .await
    .expect(
        "verification must fan out to ONE Job per matched PVC, each labelled with its own \
         member tag — a single unlabelled Job is the #456 pathless run",
    );

    // Each Job's embedded work spec carries that member's DERIVED source path,
    // not an empty one, and its own stamp key.
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let mut paths: Vec<String> = Vec::new();
    for member in &members {
        let selector = format!(
            "{},kopiur.home-operations.com/verify-member={member}",
            verify_selector()
        );
        let job = jobs
            .list(&ListParams::default().labels(&selector))
            .await
            .expect("list this member's verify Job")
            .items
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("no verify Job for member {member}"));
        let raw = job
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .and_then(|p| p.containers.first())
            .and_then(|c| c.env.as_ref())
            .and_then(|env| env.iter().find(|e| e.name == "KOPIUR_WORK_SPEC"))
            .and_then(|e| e.value.clone())
            .expect("the verify Job carries the inline work-spec env");
        let spec: serde_json::Value = serde_json::from_str(&raw).expect("work spec parses");
        let path = spec["identity"]["sourcePath"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(
            path.starts_with("/pvc/e2e-vfyfan-"),
            "member {member}'s verify identity must carry ITS OWN derived kopia source path, \
             got {path:?} (an EMPTY path is exactly #456)"
        );
        assert_eq!(
            spec["operation"]["verify"]["stampKey"].as_str(),
            Some(format!("#{member}").as_str()),
            "a single-repository fan-out stamps `#<member6>`"
        );
        paths.push(path);
    }
    paths.sort();
    paths.dedup();
    assert_eq!(
        paths.len(),
        2,
        "the two members must verify DIFFERENT kopia source paths: {paths:?}"
    );

    // Both members stamp, and the flat `lastVerified` is their MIN — "everything
    // is verified as of T". A partially-verified policy must never display a
    // reassuring timestamp.
    let status = wait_until(
        "both member stamps plus the folded lastVerified",
        default_timeout(),
        poll_interval(),
        || async {
            let s = status_json(&policies, POLICY).await;
            let stamped = s
                .get("verificationStamps")
                .and_then(|v| v.as_object())
                .map(|m| m.len())
                .unwrap_or(0);
            let flat = s
                .get("lastVerified")
                .and_then(|v| v.as_str())
                .is_some_and(|v| !v.is_empty());
            Ok((stamped == 2 && flat).then_some(s))
        },
    )
    .await
    .expect(
        "each member must stamp its own verificationStamps entry and the controller must fold \
         them into the flat lastVerified",
    );
    let stamps = status["verificationStamps"]
        .as_object()
        .expect("the stamp map");
    for member in &members {
        assert!(
            stamps.contains_key(&format!("#{member}")),
            "member {member} has no stamp: {stamps:?}"
        );
    }
    let mut values: Vec<&str> = stamps.values().filter_map(|v| v.as_str()).collect();
    values.sort();
    assert_eq!(
        status["lastVerified"].as_str(),
        values.first().copied(),
        "the flat lastVerified must be the MIN across member stamps, not the newest: {stamps:?}"
    );

    let _ = schedules.delete(SCHEDULE, &DeleteParams::default()).await;
    let _ = policies.delete(POLICY, &DeleteParams::default()).await;
    for child in children_of(&client, POLICY).await {
        if let Some(n) = child.metadata.name {
            let _ = backups.delete(&n, &DeleteParams::default()).await;
        }
    }
}
