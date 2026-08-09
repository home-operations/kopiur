//! e2e: `SnapshotPolicy.spec.repositories` multi-repository fan-out (issue
//! #368 Feature B) — one recipe, N repositories, one `Snapshot` CR + one mover
//! Job per (slot × repository), its own CI shard (`bins: "multi_repository"`).
//!
//! What these scenarios prove against a live operator:
//!
//! * the flagship fan-out: a schedule slot against a two-repository policy
//!   mints TWO children carrying the `-repo-<rslug>-` name marker and a
//!   NORMALIZED `spec.repository` pin, each reaching `Succeeded` with its own
//!   kopia manifest in its own repository (verifier-proven), plus the policy's
//!   multi-repo status surface (`repositorySummary`, per-repo
//!   `resolved.repositories` identities, per-repo verification entries and the
//!   MIN-folded flat `lastVerified`);
//! * per-repo GFS retention: `keepLatest: 1` keeps one CR + one manifest PER
//!   repository (2 total), never one overall — the silent-data-loss guard;
//! * partial progress: with repository B broken, its child parks non-terminal
//!   (`Pending` / `RepositoryNotReady`) while repository A's child succeeds,
//!   the policy raises `RepositoriesReady=False` naming B, and recovery drains
//!   the parked child and clears the gate;
//! * `fromPolicy` restore selection: refused without `spec.repository`,
//!   completes against the selected member's OWN manifest, refused for a
//!   non-member;
//! * admission refusals: an unpinned manual child of a multi-repo policy, a
//!   non-member pin, and `hooks` × `repositories` (which names
//!   `SnapshotReplication` as the supported alternative).
//!
//! NOT asserted here, deliberately: `kubectl kopiur`-level views (`snapshot
//! now`, `snapshots list --repository`) — the plugin flows are covered by
//! tests/cli.rs's shard; these scenarios assert the CR fields the CLI renders.
//! Produced fan-out children carry NO `REPOSITORY_UID_LABEL` (only
//! discovered/adopted/replicated rows do — verified against the mint site,
//! `create_scheduled_backup`); per-repo addressing is the `spec.repository`
//! pin, and that is what is asserted.
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`; driven by
//! `mise run //crates/e2e:test`. Skips gracefully without a cluster.

#![cfg(all(unix, feature = "e2e"))]

mod common;
use common::*;

use std::collections::{BTreeMap, BTreeSet};

use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::{Api, Client, ResourceExt};

use k8s_openapi::api::batch::v1::Job;

use kopiur_api::consts::{CONFIG_LABEL, ORIGIN_LABEL};
use kopiur_api::{Repository, Restore, Snapshot, SnapshotPolicy, SnapshotSchedule};
use kopiur_e2e::{E2E_NAMESPACE, Need, World, default_timeout, poll_interval, wait_until};

/// The per-(policy, repository) verify single-flight labels — wire contracts
/// with the CONTROLLER crate (`VERIFY_LABEL` / `VERIFY_REPO_LABEL`), deliberate
/// literals here like `common::WORK_SPEC_ENV`: an accidental rename must fail
/// this suite.
const VERIFY_LABEL: &str = "kopiur.home-operations.com/verify";
const VERIFY_REPO_LABEL: &str = "kopiur.home-operations.com/verify-repo";
/// The schedule-provenance label a `SnapshotSchedule` stamps on its children.
const SCHEDULE_LABEL: &str = "kopiur.home-operations.com/schedule";

/// A cron that never fires inside a test window; paired with `runOnCreate` it
/// yields EXACTLY ONE slot, so per-repo child/manifest counts stay exact.
const NEVER_CRON: &str = "0 0 1 1 *";

/// A `SnapshotSchedule` for `policy` that fires exactly once (`runOnCreate` +
/// a Jan-1-only cron).
fn one_shot_schedule_json(name: &str, policy: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotSchedule",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "policyRef": { "name": policy },
            "schedule": { "cron": NEVER_CRON, "runOnCreate": true }
        }
    })
}

/// The `Snapshot` CRs a policy produced, by its config label.
async fn children_of(client: &Client, policy: &str) -> Vec<Snapshot> {
    let api: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    api.list(&ListParams::default().labels(&format!("{CONFIG_LABEL}={policy}")))
        .await
        .map(|l| l.items)
        .unwrap_or_default()
}

/// The kopia snapshot id recorded on a `Snapshot` (`status.snapshot.kopiaSnapshotID`).
fn kopia_id(s: &Snapshot) -> String {
    serde_json::to_value(s)
        .unwrap_or_default()
        .pointer("/status/snapshot/kopiaSnapshotID")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// A child's `spec.repository` pin name (`""` when unpinned).
fn pin_name(s: &Snapshot) -> String {
    s.spec
        .repository
        .as_ref()
        .map(|r| r.name.clone())
        .unwrap_or_default()
}

/// Assert an error is an admission-webhook denial whose message carries
/// `needle` — the same denial-source discipline as tests/webhook.rs, plus the
/// actionable-fragment check so the what/why/fix text cannot silently rot.
fn assert_admission_denied(err: &kube::Error, needle: &str, ctx: &str) {
    let msg = err.to_string();
    assert!(
        msg.contains("denied the request") || msg.to_lowercase().contains("admission"),
        "{ctx}: the rejection should come from the admission webhook, got: {msg}"
    );
    assert!(
        msg.contains(needle),
        "{ctx}: the denial must contain {needle:?}, got: {msg}"
    );
}

/// Create a Ready namespaced filesystem `Repository` over its own isolated
/// `subpath` (shared `kopia-creds` password).
async fn ensure_ready_repo(client: &Client, name: &str, subpath: &str) {
    ensure_repo(client, subpath).await;
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    create_idempotent(
        &repos,
        &cr(repository_json(name, subpath, serde_json::json!({}))),
        "create Repository",
    )
    .await;
    wait_phase(&repos, name, "Ready")
        .await
        .unwrap_or_else(|e| panic!("repository {name} should bootstrap to Ready: {e}"));
}

/// Create a multi-repo policy over `[repo_a, repo_b]` and READ IT BACK,
/// asserting the `repositories` field actually landed on the stored CR — the
/// E2e-overlay/pruned-schema guard (a policy that quietly lost the field would
/// mint ONE child and fail far from the real fault).
async fn ensure_multi_policy(
    client: &Client,
    name: &str,
    repo_a: &str,
    repo_b: &str,
    extra_spec: serde_json::Value,
) -> Api<SnapshotPolicy> {
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    create_idempotent(
        &policies,
        &cr(multi_repo_policy_json(
            E2E_NAMESPACE,
            name,
            &[repo_a, repo_b],
            extra_spec,
        )),
        "create multi-repository SnapshotPolicy",
    )
    .await;
    let stored = policies.get(name).await.expect("read back the policy");
    let names: Vec<&str> = stored
        .spec
        .repositories
        .iter()
        .map(|r| r.name.as_str())
        .collect();
    assert_eq!(
        names,
        vec![repo_a, repo_b],
        "the stored policy must carry spec.repositories in order (field pruned or \
         overlay dropped otherwise)"
    );
    policies
}

/// A pinned manual `Snapshot` against `policy`: [`snapshot_json`] (config label
/// included, so retention sees it) plus the `spec.repository` member pin the
/// webhook requires for a multi-repo child.
fn pinned_snapshot_json(
    name: &str,
    policy: &str,
    repo: &str,
    extra: serde_json::Value,
) -> Snapshot {
    let mut spec = serde_json::json!({ "repository": { "kind": "Repository", "name": repo } });
    if let (serde_json::Value::Object(s), serde_json::Value::Object(more)) = (&mut spec, extra) {
        s.extend(more);
    }
    cr(snapshot_json(E2E_NAMESPACE, name, policy, spec))
}

/// The flagship: one policy, two repositories, one schedule slot ⇒ TWO
/// children, each pinned, marked, succeeded, and landed in its OWN repository;
/// then the manual-mint admission rules and the per-repo verification fold.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn multi_repository_policy_backs_up_into_both_repos() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();

    const REPO_A: &str = "e2e-mra";
    const REPO_B: &str = "e2e-mrb";
    const POLICY: &str = "e2e-mr-pol";
    const SCHED: &str = "e2e-mr-sched";

    ensure_ready_repo(&client, REPO_A, "mrepo-a").await;
    ensure_ready_repo(&client, REPO_B, "mrepo-b").await;
    let policies =
        ensure_multi_policy(&client, POLICY, REPO_A, REPO_B, serde_json::json!({})).await;

    // One slot (runOnCreate + never-firing cron) ⇒ exactly one fan-out wave.
    let schedules: Api<SnapshotSchedule> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    create_idempotent(
        &schedules,
        &cr(one_shot_schedule_json(SCHED, POLICY)),
        "create one-shot SnapshotSchedule",
    )
    .await;

    // TWO children per slot — one per member repository.
    let children = wait_until(
        "the slot fans out into two Snapshot children",
        default_timeout(),
        poll_interval(),
        || {
            let client = client.clone();
            async move {
                let rows = children_of(&client, POLICY).await;
                Ok((rows.len() == 2).then_some(rows))
            }
        },
    )
    .await
    .expect("a 2-repository policy must mint exactly 2 children per slot");

    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let mut ids_by_repo: BTreeMap<String, String> = BTreeMap::new();
    for child in &children {
        let name = child.name_any();
        // Name: the schedule base plus the `-repo-<rslug>-` fan-out marker
        // (rslug = the repository CR name; the trailing `-<h8>` keeps
        // marker-ambiguous tuples injective).
        assert!(
            name.starts_with(&format!("{SCHED}-")),
            "{name}: child name must be rooted at the schedule slot base"
        );
        let pin = pin_name(child);
        assert!(
            name.contains(&format!("-repo-{pin}-")),
            "{name}: child name must carry the -repo-<rslug>- marker for its pinned \
             repository {pin}"
        );
        // Pin: stamped NORMALIZED at mint — kind + name + an EXPLICIT namespace
        // (never elided for a same-namespace Repository ref).
        let v = serde_json::to_value(child).unwrap_or_default();
        assert_eq!(
            v.pointer("/spec/repository/kind").and_then(|x| x.as_str()),
            Some("Repository"),
            "{name}: {v}"
        );
        assert_eq!(
            v.pointer("/spec/repository/namespace")
                .and_then(|x| x.as_str()),
            Some(E2E_NAMESPACE),
            "{name}: the mint-time pin must be normalized (explicit namespace): {v}"
        );
        // Provenance labels: scheduled origin + the schedule's own label.
        assert_eq!(
            child.labels().get(ORIGIN_LABEL).map(String::as_str),
            Some("scheduled"),
            "{name}: fan-out children are origin=scheduled"
        );
        assert_eq!(
            child.labels().get(SCHEDULE_LABEL).map(String::as_str),
            Some(SCHED),
            "{name}: fan-out children carry the schedule label"
        );

        wait_phase(&backups, &name, "Succeeded")
            .await
            .unwrap_or_else(|e| panic!("fan-out child {name} should Succeed: {e}"));
        let id = kopia_id(&backups.get(&name).await.expect("get child"));
        assert!(!id.is_empty(), "{name}: a Succeeded child owns a kopia id");
        ids_by_repo.insert(pin, id);
    }
    // The two pins cover exactly the member set, with DISTINCT manifests.
    assert_eq!(
        ids_by_repo.keys().map(String::as_str).collect::<Vec<_>>(),
        vec![REPO_A, REPO_B],
        "the slot must pin one child to EACH member repository"
    );
    assert_eq!(
        ids_by_repo.values().collect::<BTreeSet<_>>().len(),
        2,
        "independent captures must yield distinct kopia manifests: {ids_by_repo:?}"
    );

    // Fan-out suppresses the single-child ref: the slot is recorded, but
    // `snapshotRef` (which could only name ONE of the two children) is not.
    let sched_status = status_json(&schedules, SCHED).await;
    assert!(
        sched_status
            .pointer("/lastSchedule/at")
            .and_then(|v| v.as_str())
            .is_some_and(|t| !t.is_empty()),
        "the fired slot must be recorded in status.lastSchedule.at: {sched_status}"
    );
    assert!(
        sched_status.pointer("/lastSchedule/snapshotRef").is_none(),
        "a fanned-out slot must NOT pin a single snapshotRef: {sched_status}"
    );

    // Policy multi-repo status surface: the print-column summary lists both
    // members, and `resolved.repositories` carries one per-repo identity per
    // member (the (repository, identity) pair is the unit of identity), with
    // the flat single-repo `resolved.identity` deliberately absent.
    let pol_status = wait_until(
        "policy status resolves both repositories",
        default_timeout(),
        poll_interval(),
        || {
            let policies = policies.clone();
            async move {
                let s = status_json(&policies, POLICY).await;
                let n = s
                    .pointer("/resolved/repositories")
                    .and_then(|v| v.as_array())
                    .map(Vec::len)
                    .unwrap_or(0);
                Ok((n == 2).then_some(s))
            }
        },
    )
    .await
    .expect("status.resolved.repositories must gain one entry per member");
    let summary = pol_status
        .get("repositorySummary")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        summary.contains(REPO_A) && summary.contains(REPO_B),
        "status.repositorySummary must list both members, got {summary:?}"
    );
    let resolved_repos = pol_status
        .pointer("/resolved/repositories")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for entry in &resolved_repos {
        assert!(
            entry
                .pointer("/identity/username")
                .and_then(|v| v.as_str())
                .is_some_and(|u| !u.is_empty()),
            "each member must carry an identity resolved under ITS repository: {entry}"
        );
    }
    assert!(
        pol_status.pointer("/resolved/identity").is_none(),
        "a multi-repo policy has no flat resolved.identity (per-repo entries only): {pol_status}"
    );

    // kopia-side truth per repository: each isolated repo holds exactly its
    // own child's manifest.
    for (verifier, subpath) in [("e2e-mra-verify", "mrepo-a"), ("e2e-mrb-verify", "mrepo-b")] {
        let count = observed_snapshot_count(&client, verifier, subpath).await;
        assert_eq!(
            count, 1,
            "{subpath}: each member repository must hold exactly its own child's manifest"
        );
    }

    // --- Manual mint (the unpinned-child rule; `kubectl kopiur snapshot now`
    // --- is the CLI face of the same contract, asserted here at the CR level).
    // Unpinned manual child of a multi-repo policy ⇒ webhook-refused.
    let err = backups
        .create(
            &PostParams::default(),
            &cr(snapshot_json(
                E2E_NAMESPACE,
                "e2e-mr-manual-unpinned",
                POLICY,
                serde_json::json!({}),
            )),
        )
        .await
        .expect_err("an UNPINNED manual Snapshot against a multi-repo policy must be refused");
    assert_admission_denied(&err, "must pin exactly one member", "unpinned manual mint");

    // A pin outside the member set ⇒ refused too (the typo guard).
    let err = backups
        .create(
            &PostParams::default(),
            &pinned_snapshot_json(
                "e2e-mr-manual-ghost",
                POLICY,
                "e2e-mr-ghost",
                serde_json::json!({}),
            ),
        )
        .await
        .expect_err("a NON-MEMBER pin must be refused");
    assert_admission_denied(
        &err,
        "does not list that repository",
        "non-member manual pin",
    );

    // A member pin ⇒ admitted, and it really lands in that member's repo.
    const MANUAL: &str = "e2e-mr-manual-a";
    create_idempotent(
        &backups,
        &pinned_snapshot_json(MANUAL, POLICY, REPO_A, serde_json::json!({})),
        "create pinned manual Snapshot",
    )
    .await;
    wait_phase(&backups, MANUAL, "Succeeded")
        .await
        .expect("a member-pinned manual Snapshot must Succeed");
    let count = observed_snapshot_count(&client, "e2e-mra-verify2", "mrepo-a").await;
    assert_eq!(
        count, 2,
        "the manual pinned run must add a manifest to repository A (and only A)"
    );
    let count = observed_snapshot_count(&client, "e2e-mrb-verify2", "mrepo-b").await;
    assert_eq!(count, 1, "repository B must be untouched by the manual run");

    // --- Per-repo verification: opt-in (no default tier), so enable an
    // --- every-minute quick verify and prove the per-repo machinery.
    policies
        .patch(
            POLICY,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({
                "spec": { "verification": {
                    "quick": { "schedule": { "cron": "* * * * *" } },
                    "successExpr": "stats.errors == 0"
                } }
            })),
        )
        .await
        .expect("patch quick verification onto the policy");

    // `status.verification` gains one entry PER member (normalized repository
    // ref + its own lastVerified), and the flat `lastVerified` is the MIN fold
    // — present only once EVERY member verified.
    let verified = wait_until(
        "both members gain a per-repo verification entry",
        default_timeout(),
        poll_interval(),
        || {
            let policies = policies.clone();
            async move {
                let s = status_json(&policies, POLICY).await;
                let entries = s
                    .get("verification")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let all_stamped = entries.len() == 2
                    && entries.iter().all(|e| {
                        e.get("lastVerified")
                            .and_then(|v| v.as_str())
                            .is_some_and(|t| !t.is_empty())
                    });
                Ok(all_stamped.then_some(s))
            }
        },
    )
    .await
    .expect("per-repo verify runs must stamp one verification entry per member");
    let entries = verified
        .get("verification")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let entry_repos: BTreeSet<&str> = entries
        .iter()
        .filter_map(|e| e.pointer("/repository/name").and_then(|v| v.as_str()))
        .collect();
    assert_eq!(
        entry_repos,
        BTreeSet::from([REPO_A, REPO_B]),
        "verification entries must be keyed by the member repositories: {entries:?}"
    );
    for e in &entries {
        assert_eq!(
            e.pointer("/repository/namespace").and_then(|v| v.as_str()),
            Some(E2E_NAMESPACE),
            "verification entries carry the NORMALIZED repository ref: {e}"
        );
    }
    assert!(
        verified
            .get("lastVerified")
            .and_then(|v| v.as_str())
            .is_some_and(|t| !t.is_empty()),
        "the flat lastVerified (MIN across members) must be stamped once both verified: \
         {verified}"
    );

    // The verify Jobs themselves are per-repo: distinct `-<r6>-` name segments
    // and distinct per-repo single-flight label values. ACCUMULATED over polls
    // (a finished slot's Job can be TTL-reaped between listings; the
    // every-minute cron keeps minting fresh ones), so a momentary one-repo
    // listing cannot flake this.
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let verify_selector = format!("app.kubernetes.io/component=verify,{VERIFY_LABEL}={POLICY}");
    let deadline = std::time::Instant::now() + default_timeout();
    let mut repo_tags: BTreeSet<String> = BTreeSet::new();
    loop {
        for j in jobs
            .list(&ListParams::default().labels(&verify_selector))
            .await
            .expect("list verify Jobs")
            .items
        {
            let name = j.name_any();
            assert!(
                name.contains("-vfy-q-"),
                "verify Job {name} must be a quick-tier slot"
            );
            let tag = j
                .labels()
                .get(VERIFY_REPO_LABEL)
                .cloned()
                .unwrap_or_else(|| panic!("verify Job {name} must carry the per-repo label"));
            assert_eq!(
                tag.len(),
                6,
                "{name}: the repo tag is 6 hex chars, got {tag:?}"
            );
            assert!(
                name.contains(&format!("-vfy-q-{tag}-")),
                "{name}: the Job name embeds its own repo tag {tag}"
            );
            repo_tags.insert(tag);
        }
        if repo_tags.len() >= 2 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "verify Jobs must cover BOTH member repositories (2 distinct repo tags); \
             saw only {repo_tags:?}"
        );
        tokio::time::sleep(poll_interval()).await;
    }
    assert_eq!(
        repo_tags.len(),
        2,
        "a policy with two members runs exactly two per-repo verify streams: {repo_tags:?}"
    );

    // Quiet down: stop the every-minute verify before the shard's next test.
    let _ = policies
        .patch(
            POLICY,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "spec": { "verification": null } })),
        )
        .await;
    let _ = schedules.delete(SCHED, &DeleteParams::default()).await;
}

/// Per-repo GFS retention (the audit's silent-data-loss guard): `keepLatest: 1`
/// over a two-repository policy keeps ONE CR + ONE manifest PER repository — 2
/// CRs total, never a flat newest-overall single survivor.
///
/// Timing note (m:72761a34): per-repo buckets prune CONCURRENTLY — as soon as a
/// repo's second snapshot succeeds, its first is prunable, independent of the
/// other repo. So this converges on the FINAL state and never asserts a
/// simultaneous backlog.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn multi_repository_retention_keeps_n_per_repo() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();

    const REPO_A: &str = "e2e-mr-ret-a";
    const REPO_B: &str = "e2e-mr-ret-b";
    const POLICY: &str = "e2e-mr-ret-pol";

    ensure_ready_repo(&client, REPO_A, "mrepo-ret-a").await;
    ensure_ready_repo(&client, REPO_B, "mrepo-ret-b").await;
    let policies = ensure_multi_policy(
        &client,
        POLICY,
        REPO_A,
        REPO_B,
        serde_json::json!({ "retention": { "keepLatest": 1 } }),
    )
    .await;

    // Two rounds of one pinned child per repo. `deletionPolicy: Delete` so the
    // prune really removes the kopia manifest, not just the CR.
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let rounds = [
        [("e2e-mr-ret-1a", REPO_A), ("e2e-mr-ret-1b", REPO_B)],
        [("e2e-mr-ret-2a", REPO_A), ("e2e-mr-ret-2b", REPO_B)],
    ];
    for round in &rounds {
        for (name, repo) in round {
            create_idempotent(
                &backups,
                &pinned_snapshot_json(
                    name,
                    POLICY,
                    repo,
                    serde_json::json!({ "deletionPolicy": "Delete" }),
                ),
                "create pinned round Snapshot",
            )
            .await;
            wait_phase(&backups, name, "Succeeded")
                .await
                .unwrap_or_else(|e| panic!("round Snapshot {name} should Succeed: {e}"));
        }
    }

    // Converge: exactly the two round-2 survivors, one per repository, fully
    // drained (no deletion in flight). A FLAT keepLatest: 1 would keep only the
    // newest overall (round-2's LAST create) and delete the other repo's round-2
    // row too — losing that repository's only tracked backup.
    wait_until(
        "per-repo keepLatest: 1 converges to one survivor PER repository",
        default_timeout(),
        poll_interval(),
        || {
            let client = client.clone();
            async move {
                let rows = children_of(&client, POLICY).await;
                let survivors: BTreeMap<String, String> = rows
                    .iter()
                    .filter(|b| b.metadata.deletion_timestamp.is_none())
                    .map(|b| (b.name_any(), pin_name(b)))
                    .collect();
                let expect: BTreeMap<String, String> = [
                    ("e2e-mr-ret-2a".to_string(), REPO_A.to_string()),
                    ("e2e-mr-ret-2b".to_string(), REPO_B.to_string()),
                ]
                .into();
                Ok((rows.len() == 2 && survivors == expect).then_some(()))
            }
        },
    )
    .await
    .expect(
        "retention must keep exactly one CR per repository (a single flat survivor here \
         means per-repo bucketing regressed — silent data loss for the other repository)",
    );

    // kopia-side truth: each repository retains exactly ONE manifest (the CR
    // finalizer really deleted the pruned round-1 manifests).
    for (verifier, subpath) in [
        ("e2e-mr-ret-a-verify", "mrepo-ret-a"),
        ("e2e-mr-ret-b-verify", "mrepo-ret-b"),
    ] {
        let count = observed_snapshot_count(&client, verifier, subpath).await;
        assert_eq!(
            count, 1,
            "{subpath}: keepLatest: 1 must leave exactly one manifest in EACH repository"
        );
    }

    // The flat bookkeeping sums the per-repo buckets: 2 active CRs.
    wait_until(
        "policy stamps activeSnapshotCount == 2 (one per repo)",
        default_timeout(),
        poll_interval(),
        || {
            let policies = policies.clone();
            async move {
                let s = status_json(&policies, POLICY).await;
                Ok((s
                    .pointer("/retention/activeSnapshotCount")
                    .and_then(|v| v.as_i64())
                    == Some(2))
                .then_some(()))
            }
        },
    )
    .await
    .expect("status.retention.activeSnapshotCount must converge to 2 (1 per repository)");
}

/// One repository down ⇒ partial progress, an actionable gate, and recovery.
///
/// Mechanism for "down" (the least flaky honest lever with precedent): swap
/// repository B's `passwordSecretRef` to a WRONG-password Secret. The spec edit
/// bumps `generation`, the reconciler re-connects, the connect fails, and the
/// phase leaves `Ready` (`Failed`/`Degraded`) — the same failing-connect path
/// the safe-create-guard scenarios exercise. (Deleting the Secret outright does
/// NOT work: a missing Secret is a Transient reconcile error that never patches
/// status, so the phase would stay `Ready` — verified against
/// `io::read_repo_credential` + `error_policy_for`.) Recovery is the reverse
/// patch: generation bumps again, the connect succeeds, `Ready` returns.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn multi_repository_one_repo_down_partial_progress() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();

    const REPO_A: &str = "e2e-mr-down-a";
    const REPO_B: &str = "e2e-mr-down-b";
    const POLICY: &str = "e2e-mr-down-pol";
    const SCHED: &str = "e2e-mr-down-sched";
    const BADPW_SECRET: &str = "kopia-mr-badpw";

    ensure_ready_repo(&client, REPO_A, "mrepo-down-a").await;
    ensure_ready_repo(&client, REPO_B, "mrepo-down-b").await;
    ensure_multi_policy(&client, POLICY, REPO_A, REPO_B, serde_json::json!({})).await;

    // Break repository B: a wrong-password Secret + the spec swap.
    {
        use kopiur_e2e::apply::{Fixture, apply_all};
        use kopiur_e2e::builders;
        let fixtures: Vec<Fixture> = vec![
            builders::opaque_secret(
                E2E_NAMESPACE,
                BADPW_SECRET,
                &[("KOPIA_PASSWORD", kopiur_e2e::consts::KOPIA_BADPW)],
            )
            .into(),
        ];
        apply_all(&client, &fixtures)
            .await
            .expect("provision the wrong-password Secret");
    }
    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let swap_secret = |secret: &str| {
        serde_json::json!({ "spec": { "encryption": {
            "passwordSecretRef": { "name": secret, "key": "KOPIA_PASSWORD" }
        } } })
    };
    repos
        .patch(
            REPO_B,
            &PatchParams::default(),
            &Patch::Merge(swap_secret(BADPW_SECRET)),
        )
        .await
        .expect("swap repository B to the wrong-password Secret");
    wait_until(
        "repository B leaves Ready (failing connect)",
        default_timeout(),
        poll_interval(),
        || {
            let repos = repos.clone();
            async move {
                let s = status_json(&repos, REPO_B).await;
                let phase = s.get("phase").and_then(|v| v.as_str()).unwrap_or("");
                Ok((!phase.is_empty() && phase != "Ready").then_some(phase.to_string()))
            }
        },
    )
    .await
    .expect("a wrong password must take repository B out of Ready");

    // Fire ONE slot: both children are minted, but only A's can launch.
    let schedules: Api<SnapshotSchedule> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    create_idempotent(
        &schedules,
        &cr(one_shot_schedule_json(SCHED, POLICY)),
        "create one-shot SnapshotSchedule",
    )
    .await;
    let children = wait_until(
        "the slot still fans out into two children (mint is repo-agnostic)",
        default_timeout(),
        poll_interval(),
        || {
            let client = client.clone();
            async move {
                let rows = children_of(&client, POLICY).await;
                Ok((rows.len() == 2).then_some(rows))
            }
        },
    )
    .await
    .expect("a down repository must not stop the slot from minting BOTH children");
    let child = |repo: &str| {
        children
            .iter()
            .find(|c| pin_name(c) == repo)
            .map(|c| c.name_any())
            .unwrap_or_else(|| panic!("a child pinned to {repo} must exist"))
    };
    let (child_a, child_b) = (child(REPO_A), child(REPO_B));

    // Repo A's child succeeds — the ready subset keeps processing.
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    wait_phase(&backups, &child_a, "Succeeded")
        .await
        .expect("repository A's child must Succeed while B is down");

    // Repo B's child parks NON-terminal: Pending + Ready=False/RepositoryNotReady,
    // with the actionable waiting message naming its repository.
    let cond = wait_condition(&backups, &child_b, "Ready", "False")
        .await
        .expect("repository B's child must surface Ready=False while parked");
    assert_eq!(
        cond.get("reason").and_then(|v| v.as_str()),
        Some("RepositoryNotReady"),
        "the parked child's Ready reason must be RepositoryNotReady: {cond}"
    );
    assert!(
        cond.get("message")
            .and_then(|v| v.as_str())
            .is_some_and(|m| m.contains(REPO_B)),
        "the parked child's message must name the waiting repository: {cond}"
    );
    let s = status_json(&backups, &child_b).await;
    assert_eq!(
        s.get("phase").and_then(|v| v.as_str()),
        Some("Pending"),
        "a parked child holds Pending (non-terminal — it must drain on recovery): {s}"
    );

    // The policy raises the RepositoriesReady gate naming repo B (by its
    // normalized key), while keeping the ready subset flowing.
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let gate = wait_condition(&policies, POLICY, "RepositoriesReady", "False")
        .await
        .expect("the policy must surface RepositoriesReady=False while B is down");
    assert_eq!(
        gate.get("reason").and_then(|v| v.as_str()),
        Some("RepositoryNotReady"),
        "gate: {gate}"
    );
    let gate_msg = gate.get("message").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        gate_msg.contains(&format!("Repository/{E2E_NAMESPACE}/{REPO_B}")),
        "the gate must name the not-ready repository by key; got: {gate_msg}"
    );
    assert!(
        !gate_msg.contains(&format!("Repository/{E2E_NAMESPACE}/{REPO_A}")),
        "the gate must NOT implicate the healthy repository; got: {gate_msg}"
    );

    // Recovery: restore the good Secret ref; B reconnects, the parked child
    // drains to Succeeded, and the gate clears.
    repos
        .patch(
            REPO_B,
            &PatchParams::default(),
            &Patch::Merge(swap_secret(CREDS_SECRET)),
        )
        .await
        .expect("restore repository B's good Secret ref");
    wait_phase(&repos, REPO_B, "Ready")
        .await
        .expect("repository B must return to Ready after the credential fix");
    wait_phase(&backups, &child_b, "Succeeded")
        .await
        .expect("the parked child must DRAIN to Succeeded once its repository recovers");
    assert!(
        !kopia_id(&backups.get(&child_b).await.expect("get child B")).is_empty(),
        "the drained child must own a real kopia id"
    );
    let cleared = wait_condition(&policies, POLICY, "RepositoriesReady", "True")
        .await
        .expect("the gate must clear once every repository is Ready again");
    assert_eq!(
        cleared.get("reason").and_then(|v| v.as_str()),
        Some("AllRepositoriesReady"),
        "cleared gate: {cleared}"
    );

    // Both repositories really hold their child's manifest now.
    for (verifier, subpath) in [
        ("e2e-mr-down-a-verify", "mrepo-down-a"),
        ("e2e-mr-down-b-verify", "mrepo-down-b"),
    ] {
        let count = observed_snapshot_count(&client, verifier, subpath).await;
        assert_eq!(
            count, 1,
            "{subpath}: after recovery each repository holds exactly its child's manifest"
        );
    }
    let _ = schedules.delete(SCHED, &DeleteParams::default()).await;
}

/// `fromPolicy` restore against a multi-repo policy: fail-closed without a
/// selection, complete against the SELECTED member's own manifest, and refuse a
/// non-member — the N repositories are independent captures that can diverge,
/// so the operator must never guess.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn multi_repository_restore_from_policy_requires_selection() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();

    const REPO_A: &str = "e2e-mr-rest-a";
    const REPO_B: &str = "e2e-mr-rest-b";
    const POLICY: &str = "e2e-mr-rest-pol";

    ensure_ready_repo(&client, REPO_A, "mrepo-rest-a").await;
    ensure_ready_repo(&client, REPO_B, "mrepo-rest-b").await;
    ensure_multi_policy(&client, POLICY, REPO_A, REPO_B, serde_json::json!({})).await;

    // Seed one pinned child per repository; the two manifests are the
    // discriminator for the positive restore below.
    let backups: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    for (name, repo) in [("e2e-mr-rest-sa", REPO_A), ("e2e-mr-rest-sb", REPO_B)] {
        create_idempotent(
            &backups,
            &pinned_snapshot_json(name, POLICY, repo, serde_json::json!({})),
            "create pinned seed Snapshot",
        )
        .await;
        wait_phase(&backups, name, "Succeeded")
            .await
            .unwrap_or_else(|e| panic!("seed Snapshot {name} should Succeed: {e}"));
    }
    let sb_id = kopia_id(&backups.get("e2e-mr-rest-sb").await.expect("get sb"));
    assert!(!sb_id.is_empty(), "seed B must own a kopia id");

    let restore_json = |name: &str, repository: Option<serde_json::Value>| {
        let mut spec = serde_json::json!({
            "source": { "fromPolicy": { "name": POLICY } },
            "target": { "pvc": {
                "name": format!("{name}-dst"), "capacity": "1Gi",
                "accessModes": ["ReadWriteOnce"]
            } }
        });
        if let Some(r) = repository {
            spec["repository"] = r;
        }
        serde_json::json!({
            "apiVersion": "kopiur.home-operations.com/v1alpha1",
            "kind": "Restore",
            "metadata": { "name": name, "namespace": E2E_NAMESPACE },
            "spec": spec
        })
    };
    let restores: Api<Restore> = Api::namespaced(client.clone(), E2E_NAMESPACE);

    // (a) No selection ⇒ webhook-refused, and the denial LISTS the members so
    // the fix is copy-pasteable.
    let err = restores
        .create(
            &PostParams::default(),
            &cr(restore_json("e2e-mr-rest-none", None)),
        )
        .await
        .expect_err(
            "a fromPolicy restore against a multi-repo policy must fail without a selection",
        );
    assert_admission_denied(
        &err,
        "must say which one to read",
        "fromPolicy restore without spec.repository",
    );
    let msg = err.to_string();
    assert!(
        msg.contains(REPO_A) && msg.contains(REPO_B),
        "the refusal must list the selectable repositories; got: {msg}"
    );

    // (b) A non-member selection ⇒ refused (the typo guard).
    let err = restores
        .create(
            &PostParams::default(),
            &cr(restore_json(
                "e2e-mr-rest-ghost",
                Some(serde_json::json!({ "kind": "Repository", "name": "e2e-mr-rest-nope" })),
            )),
        )
        .await
        .expect_err("a non-member spec.repository must be refused");
    assert_admission_denied(
        &err,
        "is not a repository of SnapshotPolicy",
        "fromPolicy restore with a non-member repository",
    );

    // (c) Selecting member B ⇒ Completed, restoring B's OWN capture: the
    // resolution pins exactly B's manifest (A's would be the newest-overall
    // trap if selection were ignored).
    const RESTORE_B: &str = "e2e-mr-rest-b-sel";
    create_idempotent(
        &restores,
        &cr(restore_json(
            RESTORE_B,
            Some(serde_json::json!({ "kind": "Repository", "name": REPO_B })),
        )),
        "create member-selected fromPolicy Restore",
    )
    .await;
    wait_phase(&restores, RESTORE_B, "Completed")
        .await
        .expect("the member-selected restore must Complete");
    let s = status_json(&restores, RESTORE_B).await;
    assert_eq!(
        s.pointer("/resolved/kopiaSnapshotID")
            .and_then(|v| v.as_str()),
        Some(sb_id.as_str()),
        "the restore must resolve the SELECTED member's manifest (repository B's), \
         status: {s}"
    );
    let _ = restores.delete(RESTORE_B, &DeleteParams::default()).await;
}

/// `hooks` × `repositories` is an unsatisfiable consistency contract (the first
/// fan-out finisher would thaw while siblings still read) ⇒ refused at
/// admission, and the denial names the supported alternative:
/// single-repo policy + `SnapshotReplication`.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn multi_repository_policy_with_hooks_is_refused() {
    let Some(world) = World::connect().await else {
        return;
    };
    world.ensure(&[Need::Filesystem]).await.expect("fixtures");
    let client = world.client().clone();

    // A pure spec rule: the referenced repositories need not exist.
    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let err = policies
        .create(
            &PostParams::default(),
            &cr(multi_repo_policy_json(
                E2E_NAMESPACE,
                "e2e-mr-hooks-pol",
                &["e2e-mr-hooks-a", "e2e-mr-hooks-b"],
                serde_json::json!({
                    "hooks": { "beforeSnapshot": [ { "workloadExec": {
                        "podSelector": { "matchLabels": { "app": "e2e-mr-hooks" } },
                        "command": ["sh", "-c", "true"]
                    } } ] }
                }),
            )),
        )
        .await
        .expect_err("hooks + repositories must be refused at admission");
    assert_admission_denied(&err, "SnapshotReplication", "hooks x repositories policy");
}
