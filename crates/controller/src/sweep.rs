//! Periodic sweep for **LEGACY per-run work-spec ConfigMaps** (#224).
//!
//! Current operator versions embed the mover work spec in the Job's pod env —
//! a run is exactly ONE object, cleaned up by its `ttlSecondsAfterFinished`,
//! and no per-run ConfigMap exists at all. Earlier versions instead mounted
//! the spec from a sidecar ConfigMap owner-referenced to a long-lived CR
//! (`Snapshot`/`SnapshotPolicy`/…) with no delete path: the Job self-reaped
//! via its TTL, the ConfigMap had no TTL mechanism, and one accumulated per
//! run, forever — clusters held hundreds (one per historical backup).
//!
//! This module heals upgraded clusters: a leader-only background task
//! periodically lists the kopiur-managed ConfigMaps and deletes the stale
//! work-spec ones. The decision kernel ([`sweep_candidates`]) is pure and
//! conservative — a ConfigMap is only an orphan when ALL of these hold:
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
//!
//! # Legacy per-run projected credential Secrets (#231)
//!
//! The same structural bug had a sibling: pre-#231 versions named projected
//! credential copies after the per-run mover Job (`{job}-creds-{idx}`), so
//! every recurring run (Maintenance, verification) minted a NEW Secret owned
//! by its long-lived CR — accumulating live credential copies forever. Current
//! versions use a stable per-CR name (refreshed in place; marked with
//! [`crate::consts::CREDS_SCOPE_LABEL`]). The same sweep pass heals upgraded
//! clusters via a second conservative kernel ([`legacy_creds_candidates`]):
//! a Secret is a legacy copy only when ALL of these hold:
//!
//! 1. labelled `app.kubernetes.io/managed-by=kopiur` AND
//!    `app.kubernetes.io/component=credentials` (server-side pre-filtered,
//!    re-checked),
//! 2. it carries [`crate::consts::PROJECTED_FROM_ANNOTATION`] — positively a
//!    projected copy (spares the kopia-UI credential mirrors and any user
//!    Secret),
//! 3. it does NOT carry the [`crate::consts::CREDS_SCOPE_LABEL`] marker — the
//!    marker identifies stable-named copies, which are refreshed in place (a
//!    stable copy's creationTimestamp never resets, so min-age alone could not
//!    protect it; the marker + the delete's resourceVersion precondition do),
//! 4. its name parses as `{job}-creds-<digits>` (the per-run copy shape) and
//!    no live kopiur-managed Job in its namespace still loads it via `envFrom`
//!    — an exact in-use test read from the listed Jobs' pod templates, which
//!    covers movers whose Job name differs from the copy's prefix (a populate
//!    Restore's Job `{restore}-populate` loads `{restore}-creds-0`), and
//! 5. it is older than `min_age_secs`.
//!
//! Deleting Secrets needs the `secrets` delete verb, which the chart grants
//! under `features.credentialProjection.enabled`. A 403 (the flag was turned
//! off after projection was used) degrades to a warning naming the flag.

use std::collections::HashSet;

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{ConfigMap, Secret};
use kube::api::{DeleteParams, ListParams, Preconditions};
use kube::{Api, Resource, ResourceExt};

use kopiur_mover::bootstrap::RESULT_CONFIGMAP_KEY;

use crate::config;
use crate::consts::{
    COMPONENT_LABEL, CREDS_COMPONENT, CREDS_SCOPE_LABEL, MANAGED_BY_LABEL, MANAGED_BY_VALUE,
    PROJECTED_FROM_ANNOTATION,
};
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

/// The mover-Job name a legacy projected credential Secret was minted for —
/// its name minus a trailing `-creds-<digits>` — or `None` when the name does
/// not match the projected-copy shape at all. The sweep uses it purely as the
/// shape guard; in-use protection comes from the Jobs' actual `envFrom` refs.
pub fn legacy_creds_source_job(secret_name: &str) -> Option<&str> {
    let (job, idx) = secret_name.rsplit_once("-creds-")?;
    (!job.is_empty() && !idx.is_empty() && idx.bytes().all(|b| b.is_ascii_digit())).then_some(job)
}

/// Select the legacy per-run projected credential Secrets out of `secrets`
/// (see the module doc for the guard set). `live_creds_refs` is the
/// `(namespace, secret-name)` set of every credential Secret a live
/// kopiur-managed Job still loads via `envFrom` (see
/// [`job_env_from_secret_names`]) — the exact in-use test. `now_unix` is the
/// injected decision clock, mirroring [`sweep_candidates`].
pub fn legacy_creds_candidates<'a>(
    secrets: &'a [Secret],
    live_creds_refs: &HashSet<(String, String)>,
    min_age_secs: i64,
    now_unix: i64,
) -> Vec<&'a Secret> {
    secrets
        .iter()
        .filter(|s| is_legacy_creds_candidate(s, live_creds_refs, min_age_secs, now_unix))
        .collect()
}

/// The Secret names a Job's pods load via `envFrom` (containers and
/// initContainers). Pure so the extraction is unit-testable.
pub fn job_env_from_secret_names(job: &Job) -> Vec<String> {
    let Some(pod) = job.spec.as_ref().map(|s| &s.template.spec) else {
        return Vec::new();
    };
    let Some(pod) = pod.as_ref() else {
        return Vec::new();
    };
    pod.containers
        .iter()
        .chain(pod.init_containers.iter().flatten())
        .flat_map(|c| c.env_from.iter().flatten())
        .filter_map(|e| e.secret_ref.as_ref().map(|r| r.name.clone()))
        .collect()
}

/// Whether ONE Secret passes every legacy-copy guard (module doc). Early
/// returns keep each guard independently readable.
fn is_legacy_creds_candidate(
    s: &Secret,
    live_creds_refs: &HashSet<(String, String)>,
    min_age_secs: i64,
    now_unix: i64,
) -> bool {
    let Some(labels) = s.metadata.labels.as_ref() else {
        return false;
    };
    if labels.get(MANAGED_BY_LABEL).map(String::as_str) != Some(MANAGED_BY_VALUE)
        || labels.get(COMPONENT_LABEL).map(String::as_str) != Some(CREDS_COMPONENT)
        || labels.contains_key(CREDS_SCOPE_LABEL)
    {
        return false;
    }
    let projected = s
        .metadata
        .annotations
        .as_ref()
        .is_some_and(|a| a.contains_key(PROJECTED_FROM_ANNOTATION));
    if !projected {
        return false;
    }
    let name = s.name_any();
    if legacy_creds_source_job(&name).is_none() {
        return false;
    }
    let ns = s.namespace().unwrap_or_default();
    if live_creds_refs.contains(&(ns, name)) {
        return false;
    }
    let age_secs = s
        .metadata
        .creation_timestamp
        .as_ref()
        .map(|t| now_unix - t.0.as_second())
        .unwrap_or(0);
    age_secs >= min_age_secs
}

/// What one guarded delete did (see [`delete_with_preconditions`]).
pub(crate) enum DeleteOutcome {
    /// The object was deleted.
    Deleted,
    /// 404 (already gone) or 409 (changed since classification — a live run
    /// reclaimed it): spared, re-evaluated next pass.
    Spared,
    /// 403: the operator lacks the delete verb for this resource.
    Forbidden,
}

/// Delete `name` pinned to the exact `uid`+`resourceVersion` the sweep
/// classified, tolerating the benign outcomes (module doc: the TOCTOU guard).
pub(crate) async fn delete_with_preconditions<K>(
    api: &Api<K>,
    name: &str,
    uid: Option<String>,
    resource_version: Option<String>,
) -> Result<DeleteOutcome>
where
    K: Resource + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    let dp = DeleteParams {
        preconditions: Some(Preconditions {
            uid,
            resource_version,
        }),
        ..DeleteParams::default()
    };
    match api.delete(name, &dp).await {
        Ok(_) => Ok(DeleteOutcome::Deleted),
        Err(kube::Error::Api(ae)) if ae.code == 404 || ae.code == 409 => Ok(DeleteOutcome::Spared),
        Err(kube::Error::Api(ae)) if ae.code == 403 => Ok(DeleteOutcome::Forbidden),
        Err(e) => Err(crate::error::Error::Kube(e)),
    }
}

/// What one sweep pass deleted, per victim kind.
pub struct SweepOutcome {
    /// Orphaned mover work-spec ConfigMaps (#224).
    pub work_spec_cms: usize,
    /// Legacy per-run projected credential Secrets (#231).
    pub projected_secrets: usize,
}

/// One sweep pass: list the kopiur-managed ConfigMaps, projected credential
/// Secrets, and Jobs within the install scope, select the orphans with the two
/// pure kernels, and delete them (per-item errors are logged and skipped —
/// degrade, don't crash). Returns the counts deleted.
///
/// Two guards close the classify→delete TOCTOU window for BOTH victim kinds:
/// - ConfigMaps and Secrets are listed BEFORE Jobs: a run spawned between the
///   lists shows up only in the Job list (keeping its objects), never the
///   reverse.
/// - Each delete carries a `resourceVersion` PRECONDITION pinned to the exact
///   object version the sweep classified. A run re-spawned during the delete
///   loop server-side-applies the SAME-NAMED object (same UID, same old
///   creationTimestamp — min-age can't help), but that write bumps the
///   resourceVersion, so the delete fails 409 and the live run is spared;
///   the next pass re-evaluates.
pub async fn run_sweep(
    client: &kube::Client,
    scope: &config::WatchScope,
    min_age_secs: i64,
) -> Result<SweepOutcome> {
    let managed = format!("{MANAGED_BY_LABEL}={MANAGED_BY_VALUE}");
    let cm_api: Api<ConfigMap> = scoped_api(client, scope);
    let cms = cm_api
        .list(&ListParams::default().labels(&managed))
        .await?
        .items;
    let secret_api: Api<Secret> = scoped_api(client, scope);
    let secrets = secret_api
        .list(
            &ListParams::default()
                .labels(&format!("{managed},{COMPONENT_LABEL}={CREDS_COMPONENT}")),
        )
        .await?
        .items;
    let job_api: Api<Job> = scoped_api(client, scope);
    let jobs = job_api
        .list(&ListParams::default().labels(&managed))
        .await?
        .items;
    let live_set: HashSet<(String, String)> = jobs
        .iter()
        .map(|j| (j.namespace().unwrap_or_default(), j.name_any()))
        .collect();
    let live_creds_refs: HashSet<(String, String)> = jobs
        .iter()
        .flat_map(|j| {
            let ns = j.namespace().unwrap_or_default();
            job_env_from_secret_names(j)
                .into_iter()
                .map(move |name| (ns.clone(), name))
        })
        .collect();
    let now = chrono::Utc::now().timestamp();

    Ok(SweepOutcome {
        work_spec_cms: delete_cm_victims(
            client,
            sweep_candidates(&cms, &live_set, min_age_secs, now),
        )
        .await,
        projected_secrets: delete_secret_victims(
            client,
            legacy_creds_candidates(&secrets, &live_creds_refs, min_age_secs, now),
        )
        .await,
    })
}

/// Delete the classified work-spec ConfigMap orphans; returns the count
/// deleted. Per-item failures warn and skip (degrade, don't crash).
async fn delete_cm_victims(client: &kube::Client, victims: Vec<&ConfigMap>) -> usize {
    let mut deleted = 0;
    for cm in victims {
        let ns = cm.namespace().unwrap_or_default();
        let name = cm.name_any();
        let api: Api<ConfigMap> = Api::namespaced(client.clone(), &ns);
        match delete_with_preconditions(&api, &name, cm.uid(), cm.resource_version()).await {
            Ok(DeleteOutcome::Deleted) => deleted += 1,
            Ok(DeleteOutcome::Spared) => {}
            // ConfigMap delete is an unconditional chart grant; a 403 here
            // means a hand-trimmed Role — surface it like any other failure.
            Ok(DeleteOutcome::Forbidden) => {
                tracing::warn!(configmap = %name, namespace = %ns,
                    "orphaned work-spec ConfigMap delete forbidden (skipped)");
            }
            Err(e) => {
                tracing::warn!(configmap = %name, namespace = %ns, error = %e,
                    "orphaned work-spec ConfigMap delete failed (skipped)");
            }
        }
    }
    deleted
}

/// Delete the classified legacy projected-credentials Secrets; returns the
/// count deleted. A 403 names the Helm flag whose grant carries the delete
/// verb (degrade, don't crash).
async fn delete_secret_victims(client: &kube::Client, victims: Vec<&Secret>) -> usize {
    let mut deleted = 0;
    for s in victims {
        let ns = s.namespace().unwrap_or_default();
        let name = s.name_any();
        let api: Api<Secret> = Api::namespaced(client.clone(), &ns);
        match delete_with_preconditions(&api, &name, s.uid(), s.resource_version()).await {
            Ok(DeleteOutcome::Deleted) => deleted += 1,
            Ok(DeleteOutcome::Spared) => {}
            Ok(DeleteOutcome::Forbidden) => {
                tracing::warn!(secret = %name, namespace = %ns,
                    flag = crate::consts::CREDENTIAL_PROJECTION_FLAG,
                    "legacy projected credentials Secret delete forbidden: the operator \
                     lacks the `secrets` delete verb (the credentialProjection flag was \
                     likely disabled after projection was used). Re-enable the flag or \
                     delete the legacy copies by hand (skipped)");
            }
            Err(e) => {
                tracing::warn!(secret = %name, namespace = %ns, error = %e,
                    "legacy projected credentials Secret delete failed (skipped)");
            }
        }
    }
    deleted
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
        tracing::info!("orphaned-object sweep disabled (interval 0)");
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
                Ok(outcome) => {
                    if outcome.work_spec_cms > 0 {
                        metrics.inc_work_spec_cms_swept(outcome.work_spec_cms as u64);
                        tracing::info!(
                            deleted = outcome.work_spec_cms,
                            "swept orphaned work-spec ConfigMaps"
                        );
                    }
                    if outcome.projected_secrets > 0 {
                        metrics.inc_projected_secrets_swept(outcome.projected_secrets as u64);
                        tracing::info!(
                            deleted = outcome.projected_secrets,
                            "swept legacy per-run projected credential Secrets"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "orphaned-object sweep failed; will retry next interval");
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

    // --- legacy projected credential Secrets (#231) --------------------------

    use crate::consts::PROJECTED_FROM_ANNOTATION;
    use crate::consts::{COMPONENT_LABEL, CREDS_COMPONENT, CREDS_SCOPE_CR, CREDS_SCOPE_LABEL};
    use k8s_openapi::api::core::v1::Secret;

    /// A fully legacy-shaped projected credential Secret in `ns` created
    /// `age_secs` ago: managed-by + component labels, projected-from
    /// annotation, NO scope marker. Tests mutate it into the control shapes.
    fn legacy_secret(ns: &str, name: &str, age_secs: i64) -> Secret {
        let labels = BTreeMap::from([
            (MANAGED_BY_LABEL.to_string(), MANAGED_BY_VALUE.to_string()),
            (COMPONENT_LABEL.to_string(), CREDS_COMPONENT.to_string()),
        ]);
        let annotations = BTreeMap::from([(
            PROJECTED_FROM_ANNOTATION.to_string(),
            "kopiur-system/repo-pw".to_string(),
        )]);
        Secret {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some(ns.to_string()),
                labels: Some(labels),
                annotations: Some(annotations),
                creation_timestamp: Some(Time(
                    k8s_openapi::jiff::Timestamp::from_second(NOW - age_secs).unwrap(),
                )),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn secret_names(picked: &[&Secret]) -> Vec<String> {
        picked.iter().map(|s| s.name_any()).collect()
    }

    fn creds_refs(pairs: &[(&str, &str)]) -> HashSet<(String, String)> {
        pairs
            .iter()
            .map(|(ns, n)| (ns.to_string(), n.to_string()))
            .collect()
    }

    #[test]
    fn legacy_creds_source_job_parses_only_the_projected_shape() {
        assert_eq!(
            legacy_creds_source_job("app-q-1751831000-creds-0"),
            Some("app-q-1751831000")
        );
        assert_eq!(legacy_creds_source_job("a-creds-12"), Some("a"));
        assert_eq!(legacy_creds_source_job("foo-creds"), None);
        assert_eq!(legacy_creds_source_job("foo-creds-x"), None);
        assert_eq!(legacy_creds_source_job("foo-creds-"), None);
        assert_eq!(legacy_creds_source_job("-creds-0"), None);
        assert_eq!(legacy_creds_source_job("foocreds-0"), None);
    }

    #[test]
    fn aged_legacy_creds_copy_is_selected() {
        let secrets = vec![legacy_secret("media", "app-q-1751831000-creds-0", 2 * HOUR)];
        let picked = legacy_creds_candidates(&secrets, &HashSet::new(), HOUR, NOW);
        assert_eq!(secret_names(&picked), vec!["app-q-1751831000-creds-0"]);
    }

    #[test]
    fn marker_labeled_copy_is_never_selected() {
        // The scope marker is what makes a STABLE-named copy off-limits: its
        // creationTimestamp never resets on re-apply, so min-age is no shield.
        let mut s = legacy_secret("media", "app-maint-creds-0", 2 * HOUR);
        s.metadata
            .labels
            .as_mut()
            .unwrap()
            .insert(CREDS_SCOPE_LABEL.to_string(), CREDS_SCOPE_CR.to_string());
        assert!(legacy_creds_candidates(&[s], &HashSet::new(), HOUR, NOW).is_empty());
    }

    #[test]
    fn live_env_from_reference_shields_the_secret() {
        // A live Job that loads `app-creds-0` via envFrom shields it — the
        // exact in-use test. Covers ANY consuming Job name (backup Job `app`,
        // a populate Restore's `app-populate`, …) without name algebra.
        let s = legacy_secret("media", "app-creds-0", 2 * HOUR);
        let live = creds_refs(&[("media", "app-creds-0")]);
        assert!(legacy_creds_candidates(std::slice::from_ref(&s), &live, HOUR, NOW).is_empty());
        // A reference from ANOTHER namespace is a different run's Secret.
        let live = creds_refs(&[("automation", "app-creds-0")]);
        assert_eq!(
            secret_names(&legacy_creds_candidates(
                std::slice::from_ref(&s),
                &live,
                HOUR,
                NOW
            )),
            vec!["app-creds-0"]
        );
        // A busy sibling Job that does NOT reference the copy never shields it
        // (the old name-prefix heuristic would have deferred healing forever).
        let live = creds_refs(&[("media", "app-hourly-q-1751831000-creds-0")]);
        assert_eq!(
            secret_names(&legacy_creds_candidates(&[s], &live, HOUR, NOW)),
            vec!["app-creds-0"]
        );
    }

    #[test]
    fn job_env_from_secret_names_reads_containers_and_init_containers() {
        use k8s_openapi::api::batch::v1::JobSpec;
        use k8s_openapi::api::core::v1::{
            Container, EnvFromSource, PodSpec, PodTemplateSpec, SecretEnvSource,
        };
        let env_from = |name: &str| EnvFromSource {
            secret_ref: Some(SecretEnvSource {
                name: name.to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let job = Job {
            spec: Some(JobSpec {
                template: PodTemplateSpec {
                    spec: Some(PodSpec {
                        containers: vec![Container {
                            env_from: Some(vec![env_from("app-creds-0")]),
                            ..Default::default()
                        }],
                        init_containers: Some(vec![Container {
                            env_from: Some(vec![env_from("app-creds-1")]),
                            ..Default::default()
                        }]),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            job_env_from_secret_names(&job),
            vec!["app-creds-0".to_string(), "app-creds-1".to_string()]
        );
        assert!(job_env_from_secret_names(&Job::default()).is_empty());
    }

    #[test]
    fn young_legacy_copy_is_skipped_until_min_age() {
        let s = legacy_secret("media", "app-creds-0", HOUR - 1);
        assert!(legacy_creds_candidates(&[s], &HashSet::new(), HOUR, NOW).is_empty());
    }

    #[test]
    fn non_projected_secrets_are_never_selected() {
        // Unmanaged (defense in depth against an unfiltered list).
        let mut unmanaged = legacy_secret("media", "user-creds-0", 2 * HOUR);
        unmanaged
            .metadata
            .labels
            .as_mut()
            .unwrap()
            .remove(MANAGED_BY_LABEL);
        // Managed but a different component (e.g. the kopia-UI mirror).
        let mut other_component = legacy_secret("media", "srv-kopia-ui-repo-creds-0", 2 * HOUR);
        other_component
            .metadata
            .labels
            .as_mut()
            .unwrap()
            .insert(COMPONENT_LABEL.to_string(), "kopia-server".to_string());
        // No projected-from annotation: not a projected copy at all.
        let mut unannotated = legacy_secret("media", "app-creds-0", 2 * HOUR);
        unannotated.metadata.annotations = None;
        // Name not matching the `-creds-<digits>` shape.
        let odd_name = legacy_secret("media", "app-creds-final", 2 * HOUR);
        let secrets = vec![unmanaged, other_component, unannotated, odd_name];
        assert!(legacy_creds_candidates(&secrets, &HashSet::new(), HOUR, NOW).is_empty());
    }

    #[test]
    fn unmanaged_cm_is_never_selected() {
        // Defense in depth: the server-side label selector already filters,
        // but the kernel re-checks so it is safe against an unfiltered list.
        let cms = vec![cm("media", "user-cm", 2 * HOUR, &[WORK_SPEC_FILE], false)];
        assert!(sweep_candidates(&cms, &HashSet::new(), HOUR, NOW).is_empty());
    }
}
