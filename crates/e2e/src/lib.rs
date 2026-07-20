#![warn(missing_docs)]
#![doc = include_str!("../README.md")]

use std::time::{Duration, Instant};

use kube::{Client, Error};

pub mod apply;
pub mod builders;
pub mod cli;
pub mod consts;
pub mod wait;
pub mod world;

pub use world::{Need, World};

/// The namespace the e2e harness installs the operator and runs scenarios in.
/// Back-compat alias for [`consts::OPERATOR_NS`].
pub const E2E_NAMESPACE: &str = consts::OPERATOR_NS;

/// Try to connect to a cluster, probing the API so a stale kubeconfig skips
/// rather than hangs. Returns `None` (printing a skip notice) when no cluster is
/// reachable, so an e2e test compiled with `--features e2e` still passes as a
/// no-op off-cluster.
pub async fn try_client() -> Option<Client> {
    match Client::try_default().await {
        Ok(c) => match c.apiserver_version().await {
            Ok(_) => Some(c),
            Err(e) => {
                eprintln!("SKIP: cluster unreachable ({e}); e2e test is a no-op");
                None
            }
        },
        Err(e) => {
            eprintln!("SKIP: no kube client ({e}); e2e test is a no-op");
            None
        }
    }
}

/// Poll `f` every `interval` until it returns `Ok(Some(value))`, giving up after
/// `timeout`. `f` returning `Ok(None)` means "not ready yet, keep waiting".
///
/// An `Err` from `f` is treated as "not ready yet" too, NOT as a hard failure:
/// on a loaded kind node (WSL2 / CI) the apiserver connection drops for a beat
/// every so often, and one transient `Connect` blip mid-poll used to kill a
/// random test that would have passed two seconds later. The poll keeps going
/// until the deadline; if the condition never holds, the timeout error carries
/// the LAST poll error so a *persistent* API failure (RBAC, bad kubeconfig) is
/// still fully diagnosable — it just costs the timeout instead of failing fast.
pub async fn wait_until<T, F, Fut>(
    what: &str,
    timeout: Duration,
    interval: Duration,
    mut f: F,
) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Option<T>, Error>>,
{
    let deadline = Instant::now() + timeout;
    #[allow(unused_assignments)]
    let mut last_err: Option<Error> = None;
    loop {
        match f().await {
            Ok(Some(v)) => return Ok(v),
            Ok(None) => last_err = None,
            Err(e) => {
                eprintln!("[wait_until] {what}: poll error (retrying until deadline): {e}");
                last_err = Some(e);
            }
        }
        if Instant::now() >= deadline {
            return match last_err {
                Some(e) => Err(anyhow::anyhow!(
                    "{what}: condition not met within {timeout:?}; last poll error: {e}"
                )),
                None => Err(anyhow::anyhow!(
                    "{what}: condition not met within {timeout:?}"
                )),
            };
        }
        tokio::time::sleep(interval).await;
    }
}

/// Sensible default poll budget for operator-driven state (Jobs scheduling,
/// images pulling, kopia running): generous enough for a cold kind node.
///
/// Must exceed one full kube watch reconnect cycle (the server-side watch
/// timeout is 290s). On a freshly-started kind apiserver under control-plane
/// churn, a watch can be black-holed and the controller only sees a CR when
/// the timeout forces a reconnect and the missed event is replayed — observed
/// in CI as a Repository bootstrapping exactly controller-start + 290s, blowing
/// a 180s wait. 290s cycle + Job schedule/run + margin.
pub fn default_timeout() -> Duration {
    Duration::from_secs(420)
}

/// Default poll interval.
pub fn poll_interval() -> Duration {
    Duration::from_secs(3)
}

/// Scrape the controller's `/metrics` through the API server's Service-proxy
/// subresource (no port-forward / `ws` feature needed). The chart names the
/// controller metrics Service `kopiur-controller-metrics` on port 8081.
pub async fn scrape_controller_metrics(client: &Client) -> anyhow::Result<String> {
    let path = format!(
        "/api/v1/namespaces/{E2E_NAMESPACE}/services/kopiur-controller-metrics:8081/proxy/metrics"
    );
    let req = http::Request::get(path).body(Vec::new())?;
    Ok(client.request_text(req).await?)
}

/// Ensure a `Namespace` named `ns` exists (idempotent: a 409 Conflict is treated
/// as success). Used by the cross-namespace scenarios that run a workload + Snapshot
/// in a namespace separate from the operator's.
pub async fn ensure_namespace(client: &Client, ns: &str) -> anyhow::Result<()> {
    use k8s_openapi::api::core::v1::Namespace;
    use kube::api::PostParams;

    let api: kube::Api<Namespace> = kube::Api::all(client.clone());
    let obj: Namespace = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "Namespace",
        "metadata": { "name": ns },
    }))?;
    match api.create(&PostParams::default(), &obj).await {
        Ok(_) => Ok(()),
        // Already exists from a prior run on a reused cluster.
        Err(kube::Error::Api(e)) if e.code == 409 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Add (or update) a single annotation on a `Namespace` via a strategic-merge
/// patch. Used to flip the privileged-movers opt-in in the privileged-mover
/// scenario.
pub async fn annotate_namespace(
    client: &Client,
    ns: &str,
    key: &str,
    value: &str,
) -> anyhow::Result<()> {
    use k8s_openapi::api::core::v1::Namespace;
    use kube::api::{Patch, PatchParams};

    let api: kube::Api<Namespace> = kube::Api::all(client.clone());
    let patch = serde_json::json!({ "metadata": { "annotations": { key: value } } });
    api.patch(ns, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    Ok(())
}

/// Server-side apply an Opaque `Secret` with the given string data into `ns`
/// (idempotent). Used to place a repository-credentials Secret into a workload
/// namespace so the mover Job's `envFrom` resolves there.
pub async fn apply_secret(
    client: &Client,
    ns: &str,
    name: &str,
    string_data: &[(&str, &str)],
) -> anyhow::Result<()> {
    use k8s_openapi::api::core::v1::Secret;
    use kube::api::{Patch, PatchParams};

    let data: serde_json::Map<String, serde_json::Value> = string_data
        .iter()
        .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.to_string())))
        .collect();
    let secret: Secret = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": { "name": name, "namespace": ns },
        "type": "Opaque",
        "stringData": data,
    }))?;
    let api: kube::Api<Secret> = kube::Api::namespaced(client.clone(), ns);
    api.patch(
        name,
        &PatchParams::apply("kopiur-e2e").force(),
        &Patch::Apply(&secret),
    )
    .await?;
    Ok(())
}

/// Guard: every e2e test binary must actually be RUN by CI.
///
/// This exists because 11 of 37 binaries were wired into no shard at all —
/// including `health_probe` and `steady_state`, the two most relevant to #273,
/// which is exactly why that bug shipped: the regression test for it had been
/// written, committed, and never once executed.
///
/// A *misspelled* `--test <name>` fails loudly (cargo errors on an unknown
/// target). A binary in NO shard fails silently, forever. This is the only thing
/// that catches the silent case.
///
/// Deliberately hermetic and in `src/` rather than `tests/`: it must run under the
/// ordinary `cargo test --workspace` job, with no cluster and no `feature = "e2e"`.
/// Putting it in `tests/` would make the coverage guard itself need wiring — the
/// exact trap it exists to prevent.
#[cfg(test)]
mod shard_coverage {
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn repo_root() -> PathBuf {
        // CARGO_MANIFEST_DIR is <root>/crates/e2e.
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("workspace root resolves")
    }

    /// Every `--test` target: `crates/e2e/tests/*.rs` (a directory like `common/`
    /// is a shared module, not a binary).
    fn test_binaries() -> BTreeSet<String> {
        let dir = repo_root().join("crates/e2e/tests");
        std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                (path.extension()? == "rs")
                    .then(|| path.file_stem()?.to_str().map(str::to_string))
                    .flatten()
            })
            .collect()
    }

    /// Every binary named in a `bins: "..."` value in the e2e workflow matrix.
    /// The values are single-line and double-quoted, so this needs no YAML parser.
    fn sharded_binaries() -> BTreeSet<String> {
        let path = repo_root().join(".github/workflows/e2e.yaml");
        let yaml = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        yaml.lines()
            .filter_map(|line| {
                let rest = line.trim().strip_prefix("bins:")?.trim();
                let inner = rest.strip_prefix('"')?.strip_suffix('"')?;
                Some(inner.split_whitespace().map(str::to_string))
            })
            .flatten()
            .collect()
    }

    #[test]
    fn every_e2e_test_binary_runs_in_some_ci_shard() {
        let binaries = test_binaries();
        assert!(
            binaries.len() > 10,
            "sanity: expected to discover many test binaries, found {binaries:?}"
        );
        let sharded = sharded_binaries();

        let orphans: Vec<_> = binaries.difference(&sharded).cloned().collect();
        assert!(
            orphans.is_empty(),
            "these e2e test binaries are in NO shard's `bins:`, so CI never runs them \
             — they are dead code and cannot catch a regression: {orphans:?}\n\
             Fix: add each to a `matrix.include` entry's `bins:` in \
             .github/workflows/e2e.yaml. Set `backends: true` if the binary uses \
             Need::Minio/WorkloadNs/ProjectionNs (WorkloadNs and ProjectionNs imply \
             Minio), and `csi: true` if it needs VolumeSnapshots. Give it its own \
             shard if it restarts or reshapes the operator."
        );

        // The reverse direction: a `bins:` entry naming a binary that no longer
        // exists makes `cargo test --test <name>` fail the whole shard.
        let phantom: Vec<_> = sharded.difference(&binaries).cloned().collect();
        assert!(
            phantom.is_empty(),
            "these names appear in a shard's `bins:` but have no \
             crates/e2e/tests/<name>.rs — `cargo test --test <name>` will fail the \
             shard: {phantom:?}"
        );
    }
}
