//! End-to-end scenarios for the `kopiur-ui` web console, driven as a real person
//! would be driven through it: identity headers set by a proxy, a shared token
//! proving the request came from that proxy, and Kubernetes RBAC — not the console
//! — deciding what the impersonated user may see and do.
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`, skipping gracefully without a
//! cluster. Driven by `KOPIUR_E2E_UI=1 mise run //crates/e2e:test` (the console is
//! not installed by the default e2e values; see `deploy/e2e/values-ui.yaml`).
//!
//! # Why this file exists
//!
//! Everything else about the console is unit-tested against fixtures or checked in
//! a browser. Three things had never run against a real apiserver before this
//! suite, and each of them is a place where being wrong is silent:
//!
//! 1. **Identity resolution from a live kubeconfig.** The console turns headers
//!    into `Impersonate-User`/`Impersonate-Group` and lets the apiserver authorize.
//!    A unit test can prove the headers are built; only a cluster proves the
//!    apiserver accepts them and answers as that person.
//! 2. **The impersonation self-check.** At startup the console asks, as its own
//!    ServiceAccount, whether it may impersonate `users` and `groups`, and holds
//!    itself out of the Service when it may not. Nothing but a real authorizer can
//!    answer that `SelfSubjectAccessReview`.
//! 3. **The browse session's pod exec.** Reading a file out of a snapshot means
//!    creating a Job, waiting for its pod, and `pods/exec`ing kopia into it over a
//!    websocket — as the browsing user, not as the console.
//!
//! # Both directions, always
//!
//! Every authorization scenario asserts what `e2e-editor` (bound to
//! `kopiur-ui-user` cluster-wide and `kopiur-ui-browse` in one namespace) *can* do
//! **and** that `e2e-nobody` (bound to nothing) is refused with a problem document
//! that names the role to bind. A suite that only walks the happy path proves half
//! an authorization system, and it is the wrong half: nobody files a bug when a
//! console shows them too much.
//!
//! # Transport
//!
//! Requests go through the apiserver's `services/proxy` subresource — see
//! `kopiur_e2e::ui` for why that is the honest transport here rather than a
//! convenience, and for which headers survive it.

#![cfg(all(unix, feature = "e2e"))]

use std::collections::BTreeSet;

use k8s_openapi::api::batch::v1::Job;
use kube::api::{Api, DeleteParams, ListParams};
use kube::{Client, ResourceExt};

use kopiur_api::consts::{ORIGIN_LABEL, SESSION_BROWSE, SESSION_LABEL};
use kopiur_api::{ClusterRepository, Repository, Snapshot, SnapshotPolicy};
use kopiur_e2e::ui::{Subject, Ui, UiRequest};
use kopiur_e2e::{E2E_NAMESPACE, Need, World, consts, default_timeout, poll_interval, wait_until};
use kopiur_ui_model::graph::{NodeKind, RepositoryGraph};
use kopiur_ui_model::identity::Me;
use kopiur_ui_model::views::{
    ActionReceipt, DirListing, DoctorReportView, EntryKind, Page, RepositoryDetail, SessionInfo,
    SnapshotRow,
};

mod common;

/// The namespaced repository every scenario reads, browses and snapshots into.
const REPO: &str = "e2e-ui";
/// Its node-side repo dir (`REPO_SUBPATHS`).
const REPO_SUBPATH: &str = "ui";
/// The cluster-scoped repository, so `/repositories/cluster-repository/{name}`
/// resolves against a real cluster-scoped object rather than a second namespaced one.
const CLUSTER_REPO: &str = "e2e-ui-cluster";
/// Its own node-side repo dir — two CRs must not share one kopia repo.
const CLUSTER_REPO_SUBPATH: &str = "ui-cluster";
/// The policy whose snapshots the filter assertions expect to see.
const POLICY: &str = "e2e-ui-pol";
/// A second policy in the same namespace and repository, so `?policy=` filtering
/// has something it must LEAVE OUT. Without it the filter would pass by returning
/// everything.
const OTHER_POLICY: &str = "e2e-ui-other";
/// The fixture snapshot the browse and read scenarios use.
const SNAPSHOT: &str = "e2e-ui-snap";
/// The fixture snapshot under [`OTHER_POLICY`].
const OTHER_SNAPSHOT: &str = "e2e-ui-other-snap";

/// The file the node-seed task writes into the backup source, byte for byte.
/// `crates/e2e/mise.toml` writes `printf "hello kopiur e2e\n"`.
const A_TXT: &[u8] = b"hello kopiur e2e\n";

// --- fixtures ------------------------------------------------------------------

/// Bring up everything the console scenarios read: both repositories, both
/// policies, and one succeeded snapshot under each — plus the RBAC bindings that
/// make `e2e-editor` a permitted user and leave `e2e-nobody` unable to do anything.
///
/// Idempotent, and called by every scenario rather than relying on test ordering:
/// nextest gives no cross-test ordering guarantee, and a browse scenario that
/// silently depended on an actions scenario having run first would fail as
/// "browse is broken" the day someone ran one test on its own.
async fn setup() -> Option<(Client, Ui)> {
    let world = World::connect().await?;
    world
        .ensure(&[Need::UiSubjects])
        .await
        .expect("provision the console's repo PVCs and RBAC bindings");
    let client = world.client().clone();

    common::ensure_repo(&client, REPO_SUBPATH).await;
    common::ensure_repo(&client, CLUSTER_REPO_SUBPATH).await;

    let repos: Api<Repository> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    common::create_idempotent(
        &repos,
        &common::cr(common::repository_json(
            REPO,
            REPO_SUBPATH,
            serde_json::json!({}),
        )),
        "create the console e2e Repository",
    )
    .await;

    let cluster_repos: Api<ClusterRepository> = Api::all(client.clone());
    common::create_idempotent(
        &cluster_repos,
        &common::cr(common::cluster_repository_json(
            CLUSTER_REPO,
            CLUSTER_REPO_SUBPATH,
            serde_json::json!({}),
        )),
        "create the console e2e ClusterRepository",
    )
    .await;

    let policies: Api<SnapshotPolicy> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    for policy in [POLICY, OTHER_POLICY] {
        common::create_idempotent(
            &policies,
            &common::cr(policy_json(policy)),
            &format!("create SnapshotPolicy {policy}"),
        )
        .await;
    }

    let snapshots: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    for (name, policy) in [(SNAPSHOT, POLICY), (OTHER_SNAPSHOT, OTHER_POLICY)] {
        common::create_idempotent(
            &snapshots,
            &common::cr(snapshot_json(name, policy)),
            &format!("create Snapshot {name}"),
        )
        .await;
    }
    for name in [SNAPSHOT, OTHER_SNAPSHOT] {
        common::wait_phase(&snapshots, name, "Succeeded")
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "fixture Snapshot {E2E_NAMESPACE}/{name} never reached Succeeded, so the \
                     console scenarios have nothing real to read: {e}"
                )
            });
    }

    // The ClusterRepository is only ever read through the console, never backed up
    // into, but a detail view of a repository still bootstrapping would assert
    // against a half-built object.
    common::wait_phase(&cluster_repos, CLUSTER_REPO, "Ready")
        .await
        .unwrap_or_else(|e| {
            panic!("fixture ClusterRepository {CLUSTER_REPO} never reached Ready: {e}")
        });

    Some((client.clone(), Ui::new(client)))
}

fn policy_json(name: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "SnapshotPolicy",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": {
            "repository": { "kind": "Repository", "name": REPO },
            "sources": [ { "pvc": { "name": consts::PVC_SRC } } ],
            // PVC_SRC is a statically-provisioned hostPath PVC; the default
            // copyMethod (Snapshot) would fail preflight against it.
            "copyMethod": "Direct",
            "retention": { "keepLatest": 5 }
        }
    })
}

fn snapshot_json(name: &str, policy: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kopiur.home-operations.com/v1alpha1",
        "kind": "Snapshot",
        "metadata": { "name": name, "namespace": E2E_NAMESPACE },
        "spec": { "policyRef": { "name": policy } }
    })
}

/// Every live browse-session Job in the operator namespace, found by the exact
/// label the session builder stamps.
///
/// The selector is built from `kopiur_api::consts` rather than written out, because
/// a typo here is invisible in the worst possible way: a wrong key matches nothing,
/// so "exactly one session Job exists" reads as zero, and "the Job is gone" passes
/// while a mover pod is still holding the repository open. (That is not
/// hypothetical — the first draft of this file selected on `session=browse`, and
/// this suite's first real run against a cluster is what caught it.)
async fn session_jobs(client: &Client) -> Vec<Job> {
    let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let selector = format!("{SESSION_LABEL}={SESSION_BROWSE}");
    jobs.list(&ListParams::default().labels(&selector))
        .await
        .unwrap_or_else(|e| panic!("list browse session Jobs with selector {selector}: {e}"))
        .items
}

// --- scenario 1: identity, the proxy secret, and the deny-list ------------------

/// The trust boundary, end to end.
///
/// Four refusals and one confirmation, all against the *live* console:
///
/// * no identity headers at all → `401 no-identity`;
/// * identity with a wrong proxy token → `401 proxy-secret`, and the body names
///   the secret rather than the user, because the user is not what is wrong;
/// * identity with no proxy token at all → the same;
/// * `X-Forwarded-User: system:masters` **with** the right token →
///   `403 forbidden-principal`. This is the one that could only ever be proven
///   here: the deny-list is a unit test's assertion about a function, and this
///   asserts it survives a real request all the way to a real apiserver.
///
/// Plus `/readyz` on the ops port, which is how the console reports the
/// impersonation self-check. A `200 ok` there means a real authorizer answered a
/// real `SelfSubjectAccessReview` "yes" for `users` and `groups` — the one
/// permission the whole design rests on, and the one a security-conscious operator
/// trims first.
#[tokio::test]
#[ignore = "requires the e2e harness (KOPIUR_E2E_UI=1 mise run //crates/e2e:test)"]
async fn console_trusts_only_a_proven_proxy_and_refuses_system_principals() {
    let Some((_client, ui)) = setup().await else {
        return;
    };

    // The console is in the Service at all, which means its readiness gate passed.
    // Assert the body too: `ok` is what a passing self-check prints, and every
    // failing subsystem names itself there instead (`impersonation-unavailable`,
    // `cache-syncing`, …). A pod that is Ready for the wrong reason is exactly the
    // failure /readyz exists to make visible.
    let ready = ui
        .send(UiRequest::ops("/readyz"))
        .await
        .expect("scrape the console's ops port")
        .expect_status(200);
    let body = ready.text();
    // `starts_with`, not equality: the body is `ok` plus any non-blocking NOTES in
    // parentheses (a placeholder SPA bundle is reported there and is never worth
    // refusing traffic over). Pinning the exact string would fail this scenario for
    // a reason that has nothing to do with identity.
    assert!(
        body.trim_end().starts_with("ok"),
        "/readyz must report `ok` once the console's ServiceAccount has been confirmed able to \
         impersonate; it reported {body:?}. `impersonation-unavailable` there means the \
         chart's ui.rbac ClusterRole did not grant `impersonate` on users/groups, and every \
         console request would 403 with something the caller cannot fix."
    );

    let anonymous = ui
        .send(UiRequest::get(&Subject::editor(), "/api/v1/me").without_identity())
        .await
        .expect("call /me with no identity headers")
        .expect_status(401)
        .expect_problem("urn:kopiur:problem:no-identity");
    assert!(
        !anonymous.fix.is_empty(),
        "a 401 must tell the caller what to do; fix was empty"
    );

    for (label, token) in [("a wrong", Some("not-the-shared-token")), ("no", None)] {
        let problem = ui
            .send(UiRequest::get(&Subject::editor(), "/api/v1/me").with_token(token))
            .await
            .unwrap_or_else(|e| panic!("call /me with {label} proxy token: {e}"))
            .expect_status(401)
            .expect_problem("urn:kopiur:problem:proxy-secret");
        assert!(
            problem.detail.contains("X-Kopiur-Proxy-Token")
                || problem.why.contains("X-Kopiur-Proxy-Token")
                || problem.fix.contains("X-Kopiur-Proxy-Token"),
            "a {label} proxy token must be refused by naming the SECRET, not the user — the \
             caller's identity is not what is wrong. Got detail={:?} why={:?} fix={:?}",
            problem.detail,
            problem.why,
            problem.fix
        );
    }

    // The deny-list, through the whole stack. A correct token is sent on purpose:
    // this must be refused because of WHO it claims to be, not because the request
    // was unproven — otherwise the assertion would pass with the deny-list removed.
    for principal in ["system:masters", "system:kube-controller-manager"] {
        ui.send(UiRequest::get(&Subject::bare(principal), "/api/v1/me"))
            .await
            .unwrap_or_else(|e| panic!("call /me as {principal}: {e}"))
            .expect_status(403)
            .expect_problem("urn:kopiur:problem:forbidden-principal");
    }

    // And the identity that IS proven resolves to the person the proxy asserted.
    let me: Me = ui
        .send(UiRequest::get(&Subject::editor(), "/api/v1/me"))
        .await
        .expect("call /me as the bound editor")
        .expect_status(200)
        .json();
    assert_eq!(
        me.user,
        kopiur_e2e::ui::EDITOR_USER,
        "the console must impersonate the user the proxy asserted, not its own \
         ServiceAccount; /me reported {:?}",
        me.user
    );
    assert!(
        me.groups.contains(&kopiur_e2e::ui::E2E_GROUP.to_string()),
        "the groups header must survive the apiserver's Service proxy and reach \
         Impersonate-Group; /me reported groups {:?}",
        me.groups
    );
}

// --- scenario 2: reads, as the person the bindings describe ---------------------

/// What a bound user reads, and what `/me` promises them.
///
/// The capability flags are the interesting half. They are answered by
/// `SubjectAccessReview`s about the *impersonated* user, so they are the console
/// telling us what it believes Kubernetes will allow — and every later scenario
/// then checks Kubernetes actually does. A `/me` that promised more than the
/// bindings grant would be a greyed-out button that should have been enabled, or
/// worse, an enabled one that 403s on click.
#[tokio::test]
#[ignore = "requires the e2e harness (KOPIUR_E2E_UI=1 mise run //crates/e2e:test)"]
async fn console_reads_the_fleet_as_the_bound_user() {
    let Some((_client, ui)) = setup().await else {
        return;
    };
    let editor = Subject::editor();

    // /me, scoped to the namespace the browse RoleBinding covers.
    let me: Me = ui
        .send(UiRequest::get(
            &editor,
            format!("/api/v1/me?namespace={E2E_NAMESPACE}"),
        ))
        .await
        .expect("call /me")
        .expect_status(200)
        .json();
    for (granted, what) in [
        (me.can.create_snapshots, "create Snapshots"),
        (me.can.delete_snapshots, "delete Snapshots"),
        (me.can.create_restores, "create Restores"),
        (me.can.patch_policies, "patch SnapshotPolicies"),
        (me.can.create_session_jobs, "create browse session Jobs"),
        (me.can.exec_sessions, "exec into browse session pods"),
    ] {
        assert!(
            granted,
            "/me?namespace={E2E_NAMESPACE} must report that {} may {what}: it is bound to \
             {} cluster-wide and {} in that namespace. A false here greys out a control the \
             apiserver would in fact allow.",
            kopiur_e2e::ui::EDITOR_USER,
            consts::UI_USER_ROLE,
            consts::UI_BROWSE_ROLE,
        );
    }

    // The graph: the fixture repository, Healthy, with an edge to its policy.
    let graph: RepositoryGraph = ui
        .send(UiRequest::get(&editor, "/api/v1/graph"))
        .await
        .expect("call /graph")
        .expect_status(200)
        .json();
    let repo_node = graph
        .nodes
        .iter()
        .find(|n| n.kind == NodeKind::Repository && n.name == REPO)
        .unwrap_or_else(|| {
            panic!(
                "/graph must contain the fixture Repository {REPO}; it returned nodes {:?}",
                graph.nodes.iter().map(|n| &n.name).collect::<Vec<_>>()
            )
        });
    assert!(
        !repo_node.missing,
        "/graph drew Repository {REPO} as a dangling reference, but the object exists and is \
         Ready — node was {repo_node:?}"
    );
    let policy_node = graph
        .nodes
        .iter()
        .find(|n| n.kind == NodeKind::Policy && n.name == POLICY)
        .unwrap_or_else(|| panic!("/graph must contain the fixture SnapshotPolicy {POLICY}"));
    assert!(
        graph
            .edges
            .iter()
            .any(|e| e.from == policy_node.id && e.to == repo_node.id),
        "/graph must draw an edge from SnapshotPolicy {POLICY} to Repository {REPO} — that \
         edge IS the answer to \"where do these backups go\". Edges were {:?}",
        graph
            .edges
            .iter()
            .map(|e| format!("{}->{}", e.from, e.to))
            .collect::<Vec<_>>()
    );

    // The policy filter must EXCLUDE, not merely include.
    let filtered: Page<SnapshotRow> = ui
        .send(UiRequest::get(
            &editor,
            format!("/api/v1/snapshots?namespace={E2E_NAMESPACE}&policy={POLICY}"),
        ))
        .await
        .expect("call /snapshots?policy=")
        .expect_status(200)
        .json();
    let names: BTreeSet<&str> = filtered.items.iter().map(|r| r.name.as_str()).collect();
    assert!(
        names.contains(SNAPSHOT),
        "/snapshots?policy={POLICY} must include {SNAPSHOT}; it returned {names:?}"
    );
    assert!(
        !names.contains(OTHER_SNAPSHOT),
        "/snapshots?policy={POLICY} returned {OTHER_SNAPSHOT}, which belongs to \
         {OTHER_POLICY}. A filter that does not exclude is a snapshots table showing another \
         policy's rows under this policy's heading. Rows: {names:?}"
    );
    for row in &filtered.items {
        assert_eq!(
            row.policy.as_deref(),
            Some(POLICY),
            "/snapshots?policy={POLICY} returned row {} whose policy is {:?}",
            row.name,
            row.policy
        );
    }
    let snapshot_row = filtered
        .items
        .iter()
        .find(|r| r.name == SNAPSHOT)
        .expect("the fixture snapshot is in the filtered page");
    assert!(
        snapshot_row
            .kopia_snapshot_id
            .as_deref()
            .is_some_and(|id| !id.is_empty()),
        "a Succeeded snapshot row must carry the real kopia manifest ID the mover reported; \
         {SNAPSHOT} carried {:?}",
        snapshot_row.kopia_snapshot_id
    );

    // Both repository CRDs resolve through the SAME `{kindPath}` segment shape the
    // API itself mints into `RepositorySummary.kindPath`.
    for (kind_path, name) in [("repository", REPO), ("cluster-repository", CLUSTER_REPO)] {
        let route = match kind_path {
            "repository" => {
                format!("/api/v1/repositories/{kind_path}/{name}?namespace={E2E_NAMESPACE}")
            }
            _ => format!("/api/v1/repositories/{kind_path}/{name}"),
        };
        let detail: RepositoryDetail = ui
            .send(UiRequest::get(&editor, route))
            .await
            .unwrap_or_else(|e| panic!("call the {kind_path} detail for {name}: {e}"))
            .expect_status(200)
            .json();
        assert_eq!(
            detail.summary.name, name,
            "the {kind_path} detail for {name} answered about {:?} instead",
            detail.summary.name
        );
        assert_eq!(
            detail.summary.kind_path, kind_path,
            "the {kind_path} detail for {name} reported kindPath {:?}; the SPA builds every \
             link from this field, so a wrong value is a dead link on every row",
            detail.summary.kind_path
        );
    }
}

// --- scenario 3: browse, which is a pod exec -----------------------------------

/// The browse session lifecycle: the one path that reaches a pod.
///
/// Asserted in the order a user meets it — a `GET` that refuses to spend cluster
/// resources, a deliberate `POST` that starts exactly one Job, a listing, a
/// byte-exact download, and a `DELETE` that takes the pod away again.
#[tokio::test]
#[ignore = "requires the e2e harness (KOPIUR_E2E_UI=1 mise run //crates/e2e:test)"]
async fn console_browses_a_snapshot_through_one_session_pod() {
    let Some((client, ui)) = setup().await else {
        return;
    };
    let editor = Subject::editor();
    let base = format!("/api/v1/snapshots/{E2E_NAMESPACE}/{SNAPSHOT}");

    // Start from no session, whatever a previous attempt left behind (the e2e
    // nextest profile retries in place).
    for job in session_jobs(&client).await {
        let jobs: Api<Job> = Api::namespaced(client.clone(), E2E_NAMESPACE);
        let _ = jobs
            .delete(&job.name_any(), &DeleteParams::background())
            .await;
    }
    wait_until(
        "leftover browse session Jobs are gone",
        default_timeout(),
        poll_interval(),
        || async { Ok(session_jobs(&client).await.is_empty().then_some(())) },
    )
    .await
    .expect("a previous attempt's browse session should be reapable");

    // A GET never starts a session: creating a Job is a write to the cluster and a
    // real cost, so it takes a deliberate action.
    ui.send(UiRequest::get(&editor, format!("{base}/tree")))
        .await
        .expect("call /tree with no session running")
        .expect_status(409)
        .expect_problem("urn:kopiur:problem:session-required");
    assert!(
        session_jobs(&client).await.is_empty(),
        "GET …/tree created a browse session Job. A read must never start a mover pod — that \
         is the whole reason session creation is a separate POST."
    );

    // The deliberate action. 201 means the pod is already Ready, so the SPA's next
    // GET cannot arrive before the pod can serve it.
    let session: SessionInfo = ui
        .send(UiRequest::post(
            &editor,
            format!("{base}/session"),
            &serde_json::json!({}),
        ))
        .await
        .expect("POST …/session")
        .expect_status(201)
        .json();
    assert!(
        session.job.starts_with("kopiur-browse-"),
        "a session Job must be named kopiur-browse-* — the optional exec admission policy \
         narrows `pods/exec` by exactly that prefix, so a different name would silently \
         defeat it. Got {:?}",
        session.job
    );
    assert!(
        session.pod.is_some(),
        "POST …/session answers 201 only once it has resolved the pod it will exec into; \
         SessionInfo.pod was absent: {session:?}"
    );

    let jobs = session_jobs(&client).await;
    assert_eq!(
        jobs.len(),
        1,
        "exactly one browse session Job must exist after one POST …/session — a session is \
         keyed by repository and reused, so more than one means the pool's single-flight \
         attach is not holding. Jobs: {:?}",
        jobs.iter().map(ResourceExt::name_any).collect::<Vec<_>>()
    );

    // A second POST must ATTACH, not start a second pod.
    let reused: SessionInfo = ui
        .send(UiRequest::post(
            &editor,
            format!("{base}/session"),
            &serde_json::json!({}),
        ))
        .await
        .expect("POST …/session a second time")
        .expect_status(201)
        .json();
    assert!(
        reused.reused,
        "a second POST …/session must report reused=true; a fresh pod per browse tab would \
         multiply mover pods and repository connections. Got {reused:?}"
    );
    assert_eq!(
        session_jobs(&client).await.len(),
        1,
        "a second POST …/session started another Job"
    );

    // The listing. This is a real `pods/exec` of kopia, as the browsing user.
    let listing: DirListing = ui
        .send(UiRequest::get(&editor, format!("{base}/tree")))
        .await
        .expect("GET …/tree with a live session")
        .expect_status(200)
        .json();
    let entries: Vec<(&str, &EntryKind)> = listing
        .entries
        .iter()
        .map(|e| (e.name.as_str(), &e.kind))
        .collect();
    assert!(
        listing
            .entries
            .iter()
            .any(|e| e.name == "a.txt" && e.kind == EntryKind::File),
        "the snapshot root must list a.txt as a file — it is what the node-seed task wrote \
         into the backup source. Entries: {entries:?}"
    );
    assert!(
        listing
            .entries
            .iter()
            .any(|e| e.name == "sub" && e.kind == EntryKind::Dir),
        "the snapshot root must list sub as a directory. Entries: {entries:?}"
    );

    // The download. Byte-exact, because a browse that returns *nearly* the file is
    // a restore tool that silently corrupts.
    let download = ui
        .send(UiRequest::get(&editor, format!("{base}/file?path=a.txt")))
        .await
        .expect("GET …/file?path=a.txt")
        .expect_status(200)
        .expect_header("content-disposition", "attachment; filename*=UTF-8''a.txt");
    assert_eq!(
        download.body,
        A_TXT,
        "…/file?path=a.txt returned {:?}, not the {} bytes the node-seed task wrote",
        download.text(),
        A_TXT.len()
    );
    assert_eq!(
        download.header("content-length"),
        Some(A_TXT.len().to_string()),
        "…/file must declare Content-Length so the browser can show progress and detect a \
         truncated transfer; header was {:?}",
        download.header("content-length")
    );

    // Stopping the session takes the pod away — a browse that leaked a mover pod
    // per visit would hold a repository connection open forever.
    ui.send(UiRequest::delete(&editor, format!("{base}/session")))
        .await
        .expect("DELETE …/session")
        .expect_status(204);
    wait_until(
        "the browse session Job is deleted",
        default_timeout(),
        poll_interval(),
        || async { Ok(session_jobs(&client).await.is_empty().then_some(())) },
    )
    .await
    .expect("DELETE …/session must actually remove the Job, not just answer 204");

    // Idempotent: stopping an already-stopped session is the requested state.
    ui.send(UiRequest::delete(&editor, format!("{base}/session")))
        .await
        .expect("DELETE …/session again")
        .expect_status(204);
}

// --- scenario 4: actions, and the two ways they are refused --------------------

/// A write through the console, and both refusals that guard it.
///
/// The permitted user's `snapshot-now` must reach `Succeeded` as a real backup —
/// a 201 that never becomes a snapshot is a button that lies. The same POST
/// without the SPA's marker header must be refused, because that header is the one
/// thing a cross-origin page cannot set. And the unbound user must be refused by
/// the *apiserver*, with its own words preserved and a fix naming the role to bind.
#[tokio::test]
#[ignore = "requires the e2e harness (KOPIUR_E2E_UI=1 mise run //crates/e2e:test)"]
async fn console_actions_are_authorized_by_kubernetes_and_gated_against_csrf() {
    let Some((client, ui)) = setup().await else {
        return;
    };
    let editor = Subject::editor();
    let nobody = Subject::nobody();
    let created_name = "e2e-ui-action-snap";

    let snapshots: Api<Snapshot> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    // A retry must not 409 on its own leftovers.
    let _ = snapshots
        .delete(created_name, &DeleteParams::background())
        .await;
    wait_until(
        "a previous attempt's action Snapshot is gone",
        default_timeout(),
        poll_interval(),
        || async {
            Ok(snapshots
                .get_opt(created_name)
                .await?
                .is_none()
                .then_some(()))
        },
    )
    .await
    .expect("leftover action Snapshot should delete");

    let body = serde_json::json!({
        "namespace": E2E_NAMESPACE,
        "policy": POLICY,
        "name": created_name,
    });

    // (a) No CSRF marker: refused before anything is created.
    ui.send(UiRequest::post(&editor, "/api/v1/actions/snapshot-now", &body).without_csrf())
        .await
        .expect("POST snapshot-now without X-Kopiur-Request")
        .expect_status(403)
        .expect_problem("urn:kopiur:problem:csrf");
    assert!(
        snapshots
            .get_opt(created_name)
            .await
            .expect("query the action Snapshot")
            .is_none(),
        "a CSRF-refused POST created {created_name} anyway — the guard must run BEFORE the \
         handler, not alongside it"
    );

    // (b) The unbound user: refused by Kubernetes, not by the console's own idea
    // of who may act. Both the action and the plain list.
    for (what, request) in [
        (
            "the snapshot-now action",
            UiRequest::post(&nobody, "/api/v1/actions/snapshot-now", &body),
        ),
        (
            "the snapshots list",
            UiRequest::get(
                &nobody,
                format!("/api/v1/snapshots?namespace={E2E_NAMESPACE}"),
            ),
        ),
    ] {
        let problem = ui
            .send(request)
            .await
            .unwrap_or_else(|e| panic!("call {what} as {}: {e}", kopiur_e2e::ui::NOBODY_USER))
            .expect_status(403)
            .expect_problem("urn:kopiur:problem:forbidden");
        assert_eq!(
            problem.kube_reason.as_deref(),
            Some("Forbidden"),
            "{what} as {} must be refused by the APISERVER (kubeReason=Forbidden), not by a \
             console-side check — otherwise the console, not Kubernetes, is the authorization \
             boundary. Problem: {problem:?}",
            kopiur_e2e::ui::NOBODY_USER
        );
        assert!(
            problem.why.contains(kopiur_e2e::ui::NOBODY_USER)
                || problem.why.contains("forbidden")
                || problem.why.contains("cannot"),
            "{what} as {}: the apiserver's own words must survive into `why` so the reader \
             can see which verb on which resource was refused. why={:?}",
            kopiur_e2e::ui::NOBODY_USER,
            problem.why
        );
        assert!(
            problem.fix.contains(consts::UI_USER_ROLE),
            "{what} as {}: the fix must name the role to bind ({}), because binding it is the \
             only thing that resolves this — a browser cannot change kubeconfig. fix={:?}",
            kopiur_e2e::ui::NOBODY_USER,
            consts::UI_USER_ROLE,
            problem.fix
        );
    }
    assert!(
        snapshots
            .get_opt(created_name)
            .await
            .expect("query the action Snapshot")
            .is_none(),
        "the unbound user's refused snapshot-now created {created_name} anyway"
    );

    // (c) The permitted user: 201, a real CR, and a real backup.
    let receipt: ActionReceipt = ui
        .send(UiRequest::post(
            &editor,
            "/api/v1/actions/snapshot-now",
            &body,
        ))
        .await
        .expect("POST snapshot-now as the bound editor")
        .expect_status(201)
        .json();
    assert!(
        receipt.created.iter().any(|r| r.name == created_name),
        "the snapshot-now receipt must name the Snapshot it created; it listed {:?}",
        receipt.created.iter().map(|r| &r.name).collect::<Vec<_>>()
    );

    let created = snapshots
        .get(created_name)
        .await
        .expect("the console's snapshot-now must have created a real Snapshot CR");
    assert_eq!(
        created.labels().get(ORIGIN_LABEL).map(String::as_str),
        Some("manual"),
        "a console-initiated backup must carry origin=manual, so retention and the snapshots \
         table can tell it apart from a scheduled run. Labels: {:?}",
        created.labels()
    );

    common::wait_phase(&snapshots, created_name, "Succeeded")
        .await
        .unwrap_or_else(|e| {
            panic!(
                "the Snapshot the console created never reached Succeeded — a 201 from \
                 snapshot-now that does not become a backup is a button that lies: {e}"
            )
        });
}

// --- scenario 5: doctor, as a user who cannot read Secrets ---------------------

/// The doctor report, rendered for someone the console impersonates.
///
/// `credentials-present` must be a **Warn**, and that is the point: the human roles
/// grant no `secrets` read, the check therefore cannot verify the credential
/// Secrets resolve, and an RBAC-degraded check warns rather than failing. A `Fail`
/// here would tell every console user their healthy cluster is broken; a `Pass`
/// would mean the console can read Secrets, which it must never be able to do.
#[tokio::test]
#[ignore = "requires the e2e harness (KOPIUR_E2E_UI=1 mise run //crates/e2e:test)"]
async fn console_doctor_degrades_the_checks_the_user_may_not_run() {
    let Some((_client, ui)) = setup().await else {
        return;
    };

    let report: DoctorReportView = ui
        .send(UiRequest::get(
            &Subject::editor(),
            format!("/api/v1/doctor?namespace={E2E_NAMESPACE}"),
        ))
        .await
        .expect("call /doctor")
        .expect_status(200)
        .json();

    assert_eq!(
        report.checks.len(),
        10,
        "the console must render every doctor check, so a reader is never silently shown a \
         partial report. Checks: {:?}",
        report.checks.iter().map(|c| &c.check).collect::<Vec<_>>()
    );

    let creds = report
        .checks
        .iter()
        .find(|c| c.check == "credentials-present")
        .expect("the doctor report must include credentials-present");
    assert_eq!(
        creds.outcome, "Warn",
        "credentials-present must degrade to Warn for a console user: the human roles grant \
         no `secrets` read, so the check cannot verify and must say so rather than claim a \
         verdict. It reported {:?} ({:?}). A Pass would mean the console CAN read Secrets.",
        creds.outcome, creds.what
    );
    assert_eq!(
        report.exit_code,
        0,
        "a warning is not a failure — a console user's restricted RBAC must not report a \
         broken cluster. Failing checks: {:?}",
        report
            .checks
            .iter()
            .filter(|c| c.outcome == "Fail")
            .map(|c| (&c.check, &c.what))
            .collect::<Vec<_>>()
    );
}
