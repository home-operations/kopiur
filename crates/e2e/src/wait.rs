//! Condition-based waits over `kube::runtime::wait`, each bounded by
//! [`crate::default_timeout`]. These replace the shell `kubectl rollout status` /
//! `kubectl wait` the old harness used. The CR-phase polling the scenarios do is
//! still served by [`crate::wait_until`]/`wait_phase` (unchanged).

use anyhow::{Context, Result, anyhow};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Pod;
use kube::api::LogParams;
use kube::runtime::wait::{Condition, await_condition, conditions};
use kube::{Api, Client};

use crate::default_timeout;

/// Wait until a `Deployment` has completed its rollout (the `kubectl rollout
/// status` equivalent), failing with a tagged error on timeout.
pub async fn deployment_ready(client: &Client, ns: &str, name: &str) -> Result<()> {
    let api: Api<Deployment> = Api::namespaced(client.clone(), ns);
    let cond = await_condition(api, name, conditions::is_deployment_completed());
    match tokio::time::timeout(default_timeout(), cond).await {
        Ok(res) => {
            res.with_context(|| format!("watching deployment {ns}/{name}"))?;
            Ok(())
        }
        Err(_) => Err(anyhow!(
            "deployment {ns}/{name} did not become ready within {:?}",
            default_timeout()
        )),
    }
}

/// True once a Pod reaches a terminal phase (`Succeeded` or `Failed`).
fn is_pod_terminal() -> impl Condition<Pod> {
    |obj: Option<&Pod>| {
        matches!(
            obj.and_then(|p| p.status.as_ref())
                .and_then(|s| s.phase.as_deref()),
            Some("Succeeded") | Some("Failed")
        )
    }
}

/// Wait for a one-shot Pod to finish and assert it `Succeeded`. On `Failed` (or
/// timeout) the error carries the Pod's logs so a CI failure is debuggable.
pub async fn pod_succeeded(client: &Client, ns: &str, name: &str) -> Result<()> {
    let api: Api<Pod> = Api::namespaced(client.clone(), ns);
    let cond = await_condition(api.clone(), name, is_pod_terminal());
    if tokio::time::timeout(default_timeout(), cond).await.is_err() {
        let logs = pod_logs(client, ns, name)
            .await
            .unwrap_or_else(|e| format!("<logs unavailable: {e}>"));
        return Err(anyhow!(
            "pod {ns}/{name} did not finish within {:?}; logs:\n{logs}",
            default_timeout()
        ));
    }
    let pod = api
        .get(name)
        .await
        .with_context(|| format!("get pod {ns}/{name}"))?;
    let phase = pod
        .status
        .and_then(|s| s.phase)
        .unwrap_or_else(|| "<unknown>".to_string());
    if phase == "Succeeded" {
        return Ok(());
    }
    let logs = pod_logs(client, ns, name)
        .await
        .unwrap_or_else(|e| format!("<logs unavailable: {e}>"));
    Err(anyhow!("pod {ns}/{name} ended {phase}; logs:\n{logs}"))
}

/// Concatenated stdout of every non-terminating Pod matching `selector` in `ns`.
///
/// Mover Jobs carry their component/instance labels on the POD template as well
/// as the Job, so the same selector a scenario uses to find a mover Job finds
/// the pod that ran it. Pods that have not produced logs yet (still `Pending`,
/// or already reaped) are skipped rather than failing the call, so this composes
/// with [`crate::wait_until`]: poll until the line you are looking for shows up.
///
/// Every matching pod is concatenated on purpose — a retried Job has several,
/// and the assertion is about what the mover DID, not about which attempt did it.
pub async fn pod_logs_for_selector(
    client: &Client,
    ns: &str,
    selector: &str,
) -> std::result::Result<String, kube::Error> {
    let api: Api<Pod> = Api::namespaced(client.clone(), ns);
    let list = api
        .list(&kube::api::ListParams::default().labels(selector))
        .await?;
    let mut out = String::new();
    for pod in list
        .items
        .into_iter()
        .filter(|p| p.metadata.deletion_timestamp.is_none())
    {
        let Some(name) = pod.metadata.name else {
            continue;
        };
        if let Ok(logs) = api.logs(&name, &LogParams::default()).await {
            out.push_str(&logs);
        }
    }
    Ok(out)
}

/// Poll until some pod matching `selector` has logged `needle`.
///
/// The assertion form for "the mover actually did X" when X leaves no trace in
/// any CR status — e.g. the repository throttle (#374), whose only observable is
/// [`crate::consts::THROTTLE_APPLIED_LOG`]. Polls rather than reading once,
/// because the pod may not exist (or may still be starting) when the caller
/// first looks.
pub async fn wait_for_pod_log(
    client: &Client,
    ns: &str,
    selector: &str,
    needle: &str,
) -> Result<()> {
    crate::wait_until(
        &format!("a pod matching `{selector}` logs `{needle}`"),
        default_timeout(),
        crate::poll_interval(),
        || async {
            Ok(pod_logs_for_selector(client, ns, selector)
                .await?
                .contains(needle)
                .then_some(()))
        },
    )
    .await
}

/// Fetch a Pod's logs (best-effort context for failure messages).
pub async fn pod_logs(client: &Client, ns: &str, name: &str) -> Result<String> {
    let api: Api<Pod> = Api::namespaced(client.clone(), ns);
    api.logs(name, &LogParams::default())
        .await
        .with_context(|| format!("fetch logs for pod {ns}/{name}"))
}
