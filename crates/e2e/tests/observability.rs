//! End-to-end guard for operator observability against the Helm-deployed
//! operator in kind.
//!
//! Gated by `#[cfg(feature = "e2e")]` + `#[ignore]`, skipping gracefully without
//! a cluster (`mise run //crates/e2e:test`). Run with `mise run //crates/e2e:test`.
//!
//! The headline assertion is the regression guard for the silent-logs bug: the
//! controller and the mover Jobs used to emit **zero** bytes to stdout because
//! `init_tracing` attached an empty `Vec` of layers (which returns
//! `Interest::never()` and disables the whole subscriber) on the default no-OTLP
//! path. The unit test `kopiur_telemetry::tests::no_otlp_layer_stack_still_emits`
//! covers the layer assembly; this proves the *deployed, no-OTLP* operator
//! actually writes logs that `kubectl logs` can see — for both the long-running
//! controller and a short-lived mover Job.

#![cfg(all(unix, feature = "e2e"))]

use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use kube::api::{ListParams, LogParams};

use kopiur_e2e::{
    E2E_NAMESPACE, World, default_timeout, poll_interval, scrape_controller_metrics, wait_until,
};

/// Read the logs of the first non-terminating pod matching a label `selector`.
/// Returns `Ok(None)` when no such pod exists yet (so callers can poll). The
/// error type is `kube::Error` so this composes directly inside `wait_until`.
async fn pod_logs_for(
    client: &kube::Client,
    selector: &str,
) -> Result<Option<String>, kube::Error> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), E2E_NAMESPACE);
    let list = pods.list(&ListParams::default().labels(selector)).await?;
    let Some(name) = list
        .items
        .into_iter()
        .filter(|p| p.metadata.deletion_timestamp.is_none())
        .find_map(|p| p.metadata.name)
    else {
        return Ok(None);
    };
    Ok(Some(pods.logs(&name, &LogParams::default()).await?))
}

/// Regression guard: the deployed operator on the **default no-OTLP path** must
/// produce stdout logs. Before the empty-layer-`Vec` fix this returned an empty
/// string for every operator binary.
///
/// The webhook is disabled in the harness, so we assert on the two binaries that
/// actually run: the long-running controller, and a short-lived **mover** Job
/// pod (a backup mover) — the mover/bootstrap silence was the worst case, since
/// its only other output was a result ConfigMap.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn operator_binaries_emit_logs() {
    let Some(world) = World::connect().await else {
        return;
    };
    let client = world.client().clone();

    // Controller: present from install, logs continuously.
    let controller = wait_until(
        "controller pod has logs",
        default_timeout(),
        poll_interval(),
        || async {
            Ok(
                pod_logs_for(&client, "app.kubernetes.io/component=controller")
                    .await?
                    .filter(|l| !l.trim().is_empty()),
            )
        },
    )
    .await
    .expect("controller should produce stdout logs (empty-layer-Vec regression)");
    assert!(
        !controller.trim().is_empty(),
        "controller produced ZERO stdout — the tracing subscriber is silent"
    );

    // Mover: a backup Job's pod carries the per-Snapshot mover label. Any mover pod
    // (from the lifecycle scenarios) carries the origin label, proving the mover
    // binary logs to stdout too. Best-effort: only assert when one exists, so
    // this test does not depend on run ordering or Job GC — but if a mover pod IS
    // present it MUST have logged.
    if let Some(mover) = pod_logs_for(&client, "kopiur.home-operations.com/origin")
        .await
        .ok()
        .flatten()
    {
        assert!(
            !mover.trim().is_empty(),
            "a mover Job pod produced ZERO stdout — the mover tracing subscriber is silent"
        );
    }
}

/// Sum every series of the client-side kube request counter, optionally
/// filtered to samples whose label set contains all of `labels`. Test-local
/// Prometheus-text parsing, same pattern as
/// `steady_state.rs::reconciliations_by_kind` / `repo_breaker.rs::metric_sum`.
fn kube_client_requests_sum(text: &str, labels: &[(&str, &str)]) -> Option<f64> {
    let mut sum = None;
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("kopiur_kube_client_requests_total{") else {
            continue;
        };
        let Some((label_str, value)) = rest.rsplit_once("} ") else {
            continue;
        };
        if !labels
            .iter()
            .all(|(k, v)| label_str.contains(&format!("{k}=\"{v}\"")))
        {
            continue;
        }
        if let Ok(v) = value.trim().parse::<f64>() {
            *sum.get_or_insert(0.0) += v;
        }
    }
    sum
}

/// The client-side kube request counter (`kopiur_kube_client_requests_total`,
/// issue #382) must exist on the deployed operator's `/metrics` with the
/// expected attribution labels, and must keep rising across ordinary operator
/// activity — the controllers' watches recycle and requeue, and the leader
/// election renews its Lease every few seconds, so a live operator can never
/// hold the counter flat for long.
#[tokio::test]
#[ignore = "requires the e2e harness (mise run //crates/e2e:test): kind + built images + helm install"]
async fn kube_client_request_counter_exists_and_rises() {
    let Some(world) = World::connect().await else {
        return;
    };
    let client = world.client().clone();

    // 1. The counter exists, and the `main` client has already issued watches
    //    against kopiur's own API group (the controller fan-out registers them
    //    at startup, so this is unconditional on any scenario CR).
    let baseline = wait_until(
        "kopiur_kube_client_requests_total present with main-client kopiur watches",
        default_timeout(),
        poll_interval(),
        || async {
            let text = scrape_controller_metrics(&client)
                .await
                .map_err(|e| kube::Error::Service(e.into()))?;
            let total = kube_client_requests_sum(&text, &[]);
            let kopiur_watches = kube_client_requests_sum(
                &text,
                &[
                    ("client", "main"),
                    ("verb", "watch"),
                    ("group", "kopiur.home-operations.com"),
                ],
            );
            Ok(match (total, kopiur_watches) {
                (Some(total), Some(w)) if w >= 1.0 => Some(total),
                _ => None,
            })
        },
    )
    .await
    .expect(
        "the deployed controller must expose kopiur_kube_client_requests_total with \
         watch-verb series for the kopiur API group on the main client",
    );

    // 2. It rises across operator activity: leader-election Lease renewals
    //    alone (election client, every few seconds) guarantee movement well
    //    inside the poll budget.
    wait_until(
        "kopiur_kube_client_requests_total rises across operator activity",
        default_timeout(),
        poll_interval(),
        || async {
            let text = scrape_controller_metrics(&client)
                .await
                .map_err(|e| kube::Error::Service(e.into()))?;
            let now = kube_client_requests_sum(&text, &[]).unwrap_or(0.0);
            Ok((now > baseline).then_some(()))
        },
    )
    .await
    .expect("the client request counter must increase while the operator is running");
}
