//! Periodic sweep for **orphaned mover work-spec ConfigMaps**.
//!
//! Every mover run applies a work-spec `ConfigMap` + `Job` pair
//! ([`crate::io::apply_mover_objects`]). The Job self-reaps via
//! `ttlSecondsAfterFinished`, and the reconcilers now delete the ConfigMap as
//! soon as they observe the Job terminal — but a ConfigMap whose Job is already
//! gone is invisible to those transition paths: nothing knows its name anymore
//! (per-slot verify/replication names, or a controller that was down across the
//! whole Job lifetime), and its owner reference points at a long-lived CR
//! (`Snapshot`/`SnapshotPolicy`/…) that keeps it alive indefinitely. Clusters
//! upgraded from operator versions that never deleted work-spec ConfigMaps hold
//! hundreds of such orphans (one per historical run).
//!
//! This module heals them: a leader-only background task periodically lists the
//! kopiur-managed ConfigMaps and deletes the stale work-spec ones. The decision
//! kernel ([`sweep_candidates`]) is pure and conservative — a ConfigMap is only
//! an orphan when ALL of these hold:
//!
//! 1. labelled `app.kubernetes.io/managed-by=kopiur` (server-side pre-filtered,
//!    re-checked here),
//! 2. its `data` carries the work-spec key ([`crate::jobs::WORK_SPEC_FILE`]) —
//!    positively identifying a mover work-spec against every other
//!    kopiur-managed ConfigMap (projected creds, dashboards, …),
//! 3. its `data` does NOT carry a bootstrap result
//!    ([`kopiur_mover::bootstrap::RESULT_CONFIGMAP_KEY`]) — the bootstrap/probe
//!    flow writes `result.json` into its work-spec ConfigMap and consumes it
//!    after the Job is gone; racing that read could make a completed bootstrap
//!    look result-less,
//! 4. no same-named Job exists in its namespace — a pending/running run always
//!    has its Job (the Job is applied immediately after the ConfigMap), and
//! 5. it is older than `min_age_secs` — closing the ConfigMap-applied-before-Job
//!    window and any controller-crash-mid-spawn gap.
//!
//! The CLI's browse-session ConfigMap is additionally safe by construction: it
//! is owned by its session Job, so it either has a live Job (guard 4) or is
//! already being cascade-deleted with it.

use std::collections::HashSet;

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::{DeleteParams, ListParams};
use kube::{Api, ResourceExt};

use kopiur_mover::bootstrap::RESULT_CONFIGMAP_KEY;

use crate::config;
use crate::consts::{MANAGED_BY_LABEL, MANAGED_BY_VALUE};
use crate::controllers::scoped_api;
use crate::error::Result;
use crate::jobs::WORK_SPEC_FILE;
use crate::metrics::Metrics;

/// Select the orphaned work-spec ConfigMaps out of `cms` (see the module doc
/// for the guard set). `live_jobs` is the `(namespace, name)` set of every
/// kopiur-managed Job; `now_unix` is the injected decision clock (unix seconds)
/// so the kernel unit-tests without a wall clock, mirroring
/// [`crate::io::classify_wedged_pods`].
pub fn sweep_candidates<'a>(
    cms: &'a [ConfigMap],
    live_jobs: &HashSet<(String, String)>,
    min_age_secs: i64,
    now_unix: i64,
) -> Vec<&'a ConfigMap> {
    cms.iter()
        .filter(|cm| {
            let managed = cm.metadata.labels.as_ref().is_some_and(|l| {
                l.get(MANAGED_BY_LABEL).map(String::as_str) == Some(MANAGED_BY_VALUE)
            });
            let Some(data) = cm.data.as_ref() else {
                return false;
            };
            let is_work_spec =
                data.contains_key(WORK_SPEC_FILE) && !data.contains_key(RESULT_CONFIGMAP_KEY);
            let key = (cm.namespace().unwrap_or_default(), cm.name_any());
            let age_secs = cm
                .metadata
                .creation_timestamp
                .as_ref()
                .map(|t| now_unix - t.0.as_second())
                // No creationTimestamp (only possible for hand-built test
                // objects; the apiserver always stamps one) → treat as brand
                // new, never eligible.
                .unwrap_or(0);
            managed && is_work_spec && !live_jobs.contains(&key) && age_secs >= min_age_secs
        })
        .collect()
}

/// One sweep pass: list the kopiur-managed ConfigMaps and Jobs within the
/// install scope, select the orphans, and delete them (404-tolerated;
/// per-item errors are logged and skipped — degrade, don't crash). Returns the
/// number deleted.
///
/// Two guards close the classify→delete TOCTOU window:
/// - ConfigMaps are listed BEFORE Jobs: a run spawned between the two lists
///   shows up only in the Job list (keeping its ConfigMap), never the reverse.
/// - Each delete carries a `resourceVersion` PRECONDITION pinned to the exact
///   object version the sweep classified. A run re-spawned during the delete
///   loop server-side-applies the SAME-NAMED ConfigMap (same UID, same old
///   creationTimestamp — min-age can't help), but that write bumps the
///   resourceVersion, so the delete fails 409 and the live run is spared;
///   the next pass re-evaluates.
pub async fn run_sweep(
    client: &kube::Client,
    scope: &config::WatchScope,
    min_age_secs: i64,
) -> Result<usize> {
    let selector = format!("{MANAGED_BY_LABEL}={MANAGED_BY_VALUE}");
    let lp = ListParams::default().labels(&selector);
    let cm_api: Api<ConfigMap> = scoped_api(client, scope);
    let cms = cm_api.list(&lp).await?.items;
    let job_api: Api<Job> = scoped_api(client, scope);
    let live_jobs: HashSet<(String, String)> = job_api
        .list(&lp)
        .await?
        .items
        .iter()
        .map(|j| (j.namespace().unwrap_or_default(), j.name_any()))
        .collect();

    let candidates = sweep_candidates(
        &cms,
        &live_jobs,
        min_age_secs,
        chrono::Utc::now().timestamp(),
    );
    let mut deleted = 0usize;
    for cm in candidates {
        let ns = cm.namespace().unwrap_or_default();
        let name = cm.name_any();
        let api: Api<ConfigMap> = Api::namespaced(client.clone(), &ns);
        let dp = DeleteParams {
            preconditions: Some(kube::api::Preconditions {
                uid: cm.uid(),
                resource_version: cm.resource_version(),
            }),
            ..DeleteParams::default()
        };
        match api.delete(&name, &dp).await {
            Ok(_) => deleted += 1,
            // 404: already gone; 409: the object changed since classification
            // (a re-spawned run reclaimed it) — spare it, re-evaluate next pass.
            Err(kube::Error::Api(ae)) if ae.code == 404 || ae.code == 409 => {}
            Err(e) => {
                tracing::warn!(configmap = %name, namespace = %ns, error = %e,
                    "orphaned work-spec ConfigMap delete failed (skipped)");
            }
        }
    }
    Ok(deleted)
}

/// Spawn the periodic sweep as a background task. Call ONLY after leader
/// election is won (a single writer — mirroring the reconcilers). The first
/// pass runs shortly after startup so upgraded clusters heal promptly; later
/// passes run every `interval_secs`. `interval_secs == 0` disables the sweep
/// entirely (the config layer documents the knob).
pub fn spawn_sweep(
    client: kube::Client,
    scope: config::WatchScope,
    metrics: Metrics,
    interval_secs: u64,
    min_age_secs: i64,
) {
    if interval_secs == 0 {
        tracing::info!("work-spec ConfigMap sweep disabled (interval 0)");
        return;
    }
    tokio::spawn(async move {
        // Short initial delay: let the watches/caches warm up and avoid piling
        // the sweep's LISTs onto the startup burst.
        let mut delay = std::time::Duration::from_secs(60);
        loop {
            tokio::time::sleep(delay).await;
            delay = std::time::Duration::from_secs(interval_secs);
            match run_sweep(&client, &scope, min_age_secs).await {
                Ok(0) => {}
                Ok(n) => {
                    metrics.inc_work_spec_cms_swept(n as u64);
                    tracing::info!(deleted = n, "swept orphaned work-spec ConfigMaps");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "work-spec ConfigMap sweep failed; will retry next interval");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
    use kube::core::ObjectMeta;
    use std::collections::BTreeMap;

    const NOW: i64 = 1_700_000_000;
    const HOUR: i64 = 3600;

    /// A kopiur-managed ConfigMap in `ns` created `age_secs` ago whose data
    /// holds exactly `keys`.
    fn cm(ns: &str, name: &str, age_secs: i64, keys: &[&str], managed: bool) -> ConfigMap {
        let mut labels = BTreeMap::new();
        if managed {
            labels.insert(MANAGED_BY_LABEL.to_string(), MANAGED_BY_VALUE.to_string());
        }
        ConfigMap {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some(ns.to_string()),
                labels: Some(labels),
                creation_timestamp: Some(Time(
                    k8s_openapi::jiff::Timestamp::from_second(NOW - age_secs).unwrap(),
                )),
                ..Default::default()
            },
            data: Some(
                keys.iter()
                    .map(|k| (k.to_string(), "{}".to_string()))
                    .collect(),
            ),
            ..Default::default()
        }
    }

    fn names(picked: &[&ConfigMap]) -> Vec<String> {
        picked.iter().map(|c| c.name_any()).collect()
    }

    #[test]
    fn old_orphaned_work_spec_cm_is_selected() {
        let cms = vec![cm(
            "media",
            "qui-20260708155044",
            2 * HOUR,
            &[WORK_SPEC_FILE],
            true,
        )];
        let picked = sweep_candidates(&cms, &HashSet::new(), HOUR, NOW);
        assert_eq!(names(&picked), vec!["qui-20260708155044"]);
    }

    #[test]
    fn young_cm_is_skipped_until_min_age() {
        // Closes the ConfigMap-applied-before-Job window: a freshly-applied
        // work-spec whose Job hasn't landed yet must never be reaped.
        let cms = vec![cm("media", "qui-fresh", HOUR - 1, &[WORK_SPEC_FILE], true)];
        assert!(sweep_candidates(&cms, &HashSet::new(), HOUR, NOW).is_empty());
    }

    #[test]
    fn cm_with_live_same_named_job_is_skipped() {
        let cms = vec![cm(
            "media",
            "qui-running",
            2 * HOUR,
            &[WORK_SPEC_FILE],
            true,
        )];
        let live: HashSet<_> = [("media".to_string(), "qui-running".to_string())].into();
        assert!(sweep_candidates(&cms, &live, HOUR, NOW).is_empty());
    }

    #[test]
    fn job_in_another_namespace_does_not_shield_a_same_named_cm() {
        // The live-Job guard keys on (namespace, name) — a Job named like the
        // ConfigMap but in a different namespace is a different run.
        let cms = vec![cm("media", "qui-x", 2 * HOUR, &[WORK_SPEC_FILE], true)];
        let live: HashSet<_> = [("automation".to_string(), "qui-x".to_string())].into();
        assert_eq!(
            names(&sweep_candidates(&cms, &live, HOUR, NOW)),
            vec!["qui-x"]
        );
    }

    #[test]
    fn non_work_spec_managed_cm_is_never_selected() {
        // kopiur manages other ConfigMaps (dashboards, …) under the same
        // label; only the work-spec data key marks a sweep target.
        let cms = vec![cm("media", "other", 2 * HOUR, &["dashboard.json"], true)];
        assert!(sweep_candidates(&cms, &HashSet::new(), HOUR, NOW).is_empty());
    }

    #[test]
    fn bootstrap_result_cm_is_never_selected() {
        // The bootstrap/probe flow PATCHes result.json INTO its work-spec
        // ConfigMap and reads it back after the Job is gone — sweeping it
        // would make a completed bootstrap look result-less.
        let cms = vec![cm(
            "kopiur-system",
            "repo-bootstrap",
            2 * HOUR,
            &[WORK_SPEC_FILE, RESULT_CONFIGMAP_KEY],
            true,
        )];
        assert!(sweep_candidates(&cms, &HashSet::new(), HOUR, NOW).is_empty());
    }

    #[test]
    fn unmanaged_cm_is_never_selected() {
        // Defense in depth: the server-side label selector already filters,
        // but the kernel re-checks so it is safe against an unfiltered list.
        let cms = vec![cm("media", "user-cm", 2 * HOUR, &[WORK_SPEC_FILE], false)];
        assert!(sweep_candidates(&cms, &HashSet::new(), HOUR, NOW).is_empty());
    }
}
