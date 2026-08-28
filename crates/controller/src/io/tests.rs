use super::events::{
    EVENT_NOTE_MAX_BYTES, TRUNCATION_MARKER, backend_failure_event, truncate_for_note,
};
use super::maintenance::is_managed_by;
use super::*;

use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, OwnerReference, Time};

use kopiur_api::Maintenance;
use kopiur_api::backend::Backend;
use kopiur_api::common::{Encryption, RepositoryKind, RepositoryRef};
use kopiur_api::maintenance::{
    MaintenanceSpec, Ownership, RepositoryMaintenanceSpec, default_maintenance_schedule,
};
use kopiur_kopia::KopiaErrorClass;

use crate::consts::{
    API_VERSION, BOOTSTRAP_JOB_FAILED_REASON, CHECK_BACKEND_ACTION, CHECK_CREDENTIALS_ACTION,
    CHECK_PERMISSIONS_ACTION, PRIVILEGED_MOVERS_ANNOTATION, REPOSITORY_NOT_INITIALIZED_REASON,
};
use kopiur_api::backend::FilesystemBackend;
use kopiur_api::common::SecretKeyRef;

/// A representative operator UID for the pure-function tests. Deliberately
/// NOT the old hardcoded 65534, so the assertions prove the UID is now
/// interpolated from the argument rather than baked into the message.
const TEST_UID: u32 = 65532;

// --- backend_failure_event: the typed kopia class drives the Event's
// remediation `action` + human note; the `reason` (asserted at the call site)
// is the class label itself, so it matches the `Bootstrapped=False` condition.
// (regression: S3 Access Denied used to land as Unknown, only visible via
// `kubectl describe`.)
#[test]
fn backend_failure_access_denied_points_at_credentials_and_bucket() {
    let (action, note) = backend_failure_event(
        KopiaErrorClass::AccessDenied,
        "error retrieving storage config from bucket \"kopiur\": Access Denied",
        TEST_UID,
    );
    assert_eq!(action, CHECK_CREDENTIALS_ACTION);
    assert!(note.contains("denied access"));
    assert!(note.contains("credentials Secret"));
    assert!(note.contains("bucket/path"));
}

#[test]
fn backend_failure_permission_denied_points_at_the_live_uid() {
    // Regression: the hint used to hardcode "commonly 65534"; it must now
    // report the operator's actual UID (here the e2e/distroless 65532) so the
    // `chown` advice is correct under any `podSecurityContext.runAsUser`.
    let (action, note) = backend_failure_event(
        KopiaErrorClass::PermissionDenied,
        "unable to create directory /repo: permission denied",
        TEST_UID,
    );
    assert_eq!(action, CHECK_PERMISSIONS_ACTION);
    assert!(note.contains("not writable"));
    assert!(
        note.contains("65532"),
        "note should name the live UID: {note}"
    );
    assert!(
        note.contains("chown -R 65532"),
        "the chown example should use the live UID: {note}"
    );
    assert!(
        !note.contains("65534"),
        "the old hardcoded UID must be gone: {note}"
    );
}

#[test]
fn backend_failure_other_classes_stay_generic_with_class_and_message() {
    let (action, note) = backend_failure_event(
        KopiaErrorClass::RepositoryUnavailable,
        "connection refused",
        TEST_UID,
    );
    assert_eq!(action, CHECK_BACKEND_ACTION);
    assert!(note.contains("RepositoryUnavailable"));
    assert!(note.contains("connection refused"));
}

// --- note truncation: a huge kopia stderr tail must not blow past the
// Kubernetes 1024-byte Event note limit (regression: the apiserver rejected
// the Event with a 422 "can have at most 1024 characters", so the actionable
// PermissionDenied warning never reached `kubectl describe`). ---

#[test]
fn backend_failure_note_is_clamped_to_the_event_limit() {
    // A kopia error several KB long (the /nonexistent cache spam + real error)
    // across every class — none may exceed the Event note limit, and the
    // oversized message must visibly carry the truncation marker.
    let huge = "x".repeat(5000);
    for class in [
        KopiaErrorClass::AccessDenied,
        KopiaErrorClass::PermissionDenied,
        KopiaErrorClass::AuthFailure,
        KopiaErrorClass::RepositoryUnavailable,
        KopiaErrorClass::NotFound,
        KopiaErrorClass::Locked,
        KopiaErrorClass::SourceError,
        KopiaErrorClass::Unknown,
    ] {
        let (_, note) = backend_failure_event(class, &huge, TEST_UID);
        assert!(
            note.len() <= EVENT_NOTE_MAX_BYTES,
            "{class:?} note is {} bytes, exceeds the {EVENT_NOTE_MAX_BYTES}-byte Event limit",
            note.len()
        );
        assert!(
            note.contains(TRUNCATION_MARKER),
            "{class:?} note should carry the truncation marker for the cut message"
        );
    }
}

#[test]
fn backend_failure_truncation_keeps_the_remediation_hint() {
    // Even with an oversized message, the static remediation text (the part a
    // user acts on) must survive — the message budget protects it, not just
    // the final clamp.
    let huge = "x".repeat(5000);
    let (action, note) = backend_failure_event(KopiaErrorClass::PermissionDenied, &huge, TEST_UID);
    assert_eq!(action, CHECK_PERMISSIONS_ACTION);
    assert!(
        note.contains("not writable"),
        "remediation hint lost to truncation: {note}"
    );
    assert!(
        note.contains("65532"),
        "remediation hint lost to truncation: {note}"
    );
}

// --- BootstrapFailure: the typed bootstrap outcome drives both the
// `Bootstrapped=False` condition reason/message and the Warning Event. The two
// terminal modes must stay distinct (a kopia rejection vs. a result-less Job
// failure) and both must produce a non-empty, bounded, actionable note. ---

#[test]
fn bootstrap_failure_backend_reason_is_the_kopia_class_label() {
    let f = BootstrapFailure::Backend {
        class: KopiaErrorClass::AccessDenied,
        message: "Access Denied".to_string(),
    };
    // The Event/condition reason matches the kopia class (so it lines up with
    // the in-process connect path), never the result-less reason.
    assert_eq!(f.reason(), KopiaErrorClass::AccessDenied.as_str());
    assert_ne!(f.reason(), BOOTSTRAP_JOB_FAILED_REASON);
    assert_eq!(f.condition_message(), "Access Denied");
}

// --- regression (apiserver-outage e2e, PR #287): a bootstrap Job that
// straddled the outage failed with NO result (its result-ConfigMap write hit
// the dead apiserver; the Job then blew activeDeadlineSeconds). The reconciler
// parked the Repository at terminal `Failed` and re-read the SAME dead Job
// every 120s until its TTL reaped it — minutes-to-hours of self-heal latency
// for a failure that carries no backend verdict at all. A result-less Job
// failure is infrastructure, not a repository verdict: it must recycle the
// Job and retry (phase Degraded, the retryable-class semantics the in-process
// path already uses). Typed backend verdicts keep parking terminally. ---
#[test]
fn only_result_less_job_failures_recycle_for_retry() {
    assert!(
        BootstrapFailure::JobFailedWithoutResult {
            job_name: "flap-repo-bootstrap".to_string(),
        }
        .recycles_for_retry(),
        "no result = no backend verdict = retry, never a terminal park"
    );
    // A typed kopia rejection IS a backend verdict — parking at Failed is right.
    assert!(
        !BootstrapFailure::Backend {
            class: KopiaErrorClass::AccessDenied,
            message: "Access Denied".to_string(),
        }
        .recycles_for_retry()
    );
    // Create-disabled on an absent repo needs a spec change, not a retry loop.
    assert!(!BootstrapFailure::RepositoryNotInitialized.recycles_for_retry());
}

// --- #345 M4: the strict-verdict reroute. A `RepositoryUnavailable` verdict on
// a once-bootstrapped repository recycles-and-retries as `Degraded` (feeding the
// unified backend sensor) instead of parking terminal `Failed` — without it, a
// breaker-opened `Degraded` repository is overwritten to `Failed` one pass later
// by its own strict retry. Everything else keeps its pre-M4 route. ---
#[test]
fn only_a_bootstrapped_backend_outage_reroutes_to_degraded() {
    let backend = |class: KopiaErrorClass| BootstrapFailure::Backend {
        class,
        message: "boom".to_string(),
    };

    // THE reroute: RepositoryUnavailable + bootstrapped.
    assert!(
        backend(KopiaErrorClass::RepositoryUnavailable).retryable_outage_for_bootstrapped(true),
        "an outage on a once-bootstrapped repo must retry as Degraded, not park Failed"
    );
    // A never-bootstrapped repo keeps fail-fast terminal Failed: a
    // first-bootstrap misconfiguration must still fail loudly for GitOps.
    assert!(
        !backend(KopiaErrorClass::RepositoryUnavailable).retryable_outage_for_bootstrapped(false)
    );

    // Deliberately NOT `class.is_retryable()`: Locked (a stale kopia lock) and
    // SourceError are not outages — looping them Degraded forever would hide
    // them (audit 3c). Every other class needs a config change anyway.
    for class in [
        KopiaErrorClass::Locked,
        KopiaErrorClass::SourceError,
        KopiaErrorClass::AuthFailure,
        KopiaErrorClass::AccessDenied,
        KopiaErrorClass::PermissionDenied,
        KopiaErrorClass::NotFound,
        KopiaErrorClass::Unknown,
    ] {
        assert!(
            !backend(class).retryable_outage_for_bootstrapped(true),
            "{class:?} must keep its pre-M4 terminal park"
        );
        assert!(!backend(class).retryable_outage_for_bootstrapped(false));
    }

    // The create-disabled / wiped-backend sentinel NEVER reroutes: while
    // Degraded the strict retry runs with auto-create forbidden, so this
    // verdict is exactly how a real wipe escalates out of the retry loop to a
    // visible terminal Failed.
    assert!(!BootstrapFailure::RepositoryNotInitialized.retryable_outage_for_bootstrapped(true));
    assert!(!BootstrapFailure::RepositoryNotInitialized.retryable_outage_for_bootstrapped(false));

    // A result-less Job failure is JobFailedWithoutResult's own route
    // (recycles_for_retry, flat 120s) — unaffected by M4.
    let result_less = BootstrapFailure::JobFailedWithoutResult {
        job_name: "repo-discovery".to_string(),
    };
    assert!(!result_less.retryable_outage_for_bootstrapped(true));
    assert!(
        result_less.recycles_for_retry(),
        "the result-less recycle route must survive M4 untouched"
    );
}

#[test]
fn bootstrap_failure_job_without_result_has_its_own_reason_and_actionable_message() {
    let f = BootstrapFailure::JobFailedWithoutResult {
        job_name: "e2e-evt-fail-bootstrap".to_string(),
    };
    // Distinct, machine-readable reason — never conflated with a kopia class.
    assert_eq!(f.reason(), BOOTSTRAP_JOB_FAILED_REASON);
    assert_eq!(
        KopiaErrorClass::from_label(f.reason()),
        KopiaErrorClass::Unknown
    );
    let msg = f.condition_message();
    assert!(
        msg.contains("e2e-evt-fail-bootstrap"),
        "names the Job: {msg}"
    );
    assert!(
        msg.contains("ServiceAccount"),
        "explains a likely cause: {msg}"
    );
    assert!(
        msg.contains("kubectl logs"),
        "gives a concrete next step: {msg}"
    );
    assert!(!msg.is_empty());
}

#[test]
fn bootstrap_failure_not_initialized_is_actionable_and_distinct() {
    let f = BootstrapFailure::RepositoryNotInitialized;
    // Its own reason — never a kopia class, never the result-less Job reason.
    assert_eq!(f.reason(), REPOSITORY_NOT_INITIALIZED_REASON);
    assert_ne!(f.reason(), BOOTSTRAP_JOB_FAILED_REASON);
    assert_eq!(
        KopiaErrorClass::from_label(f.reason()),
        KopiaErrorClass::Unknown,
        "the reason must not round-trip to a kopia class (it is a kopiur policy outcome)"
    );
    let msg = f.condition_message();
    assert!(
        msg.contains("spec.create.enabled: true"),
        "message must tell the operator how to fix it: {msg}"
    );
    assert!(!msg.is_empty());
}

#[test]
fn bootstrap_job_failed_message_is_bounded_for_the_event_note() {
    // Even a pathological Job name must yield a note within the apiserver's
    // 1024-byte limit once clamped (regression: the 422 Event bug).
    let long_name = "a".repeat(5000);
    let note = truncate_for_note(
        &bootstrap_job_failed_message(&long_name),
        EVENT_NOTE_MAX_BYTES,
    );
    assert!(
        note.len() <= EVENT_NOTE_MAX_BYTES,
        "note is {} bytes, exceeds the {EVENT_NOTE_MAX_BYTES}-byte Event limit",
        note.len()
    );
}

#[test]
fn truncate_for_note_is_a_noop_under_budget() {
    let s = "short message";
    assert_eq!(truncate_for_note(s, EVENT_NOTE_MAX_BYTES), s);
}

#[test]
fn truncate_for_note_clamps_and_marks_when_over_budget() {
    let s = "x".repeat(5000);
    let out = truncate_for_note(&s, EVENT_NOTE_MAX_BYTES);
    assert_eq!(out.len(), EVENT_NOTE_MAX_BYTES);
    assert!(out.ends_with(TRUNCATION_MARKER));
}

#[test]
fn truncate_for_note_respects_utf8_boundaries() {
    // A multibyte char straddling the cut must not panic or produce invalid
    // UTF-8 — the result is always valid and within budget.
    let s = "é".repeat(100); // each 'é' is 2 bytes
    let out = truncate_for_note(&s, 51);
    assert!(out.len() <= 51);
    assert!(out.ends_with(TRUNCATION_MARKER));
}

fn ref_of(kind: RepositoryKind, name: &str, namespace: Option<&str>) -> RepositoryRef {
    RepositoryRef {
        kind,
        name: name.into(),
        namespace: namespace.map(str::to_string),
    }
}

// --- repo_lookup: the regression guard for "ClusterRepository references are
// ignored" (controller logged `missing dependency: Repository <ns>/<name>`
// for a `kind: ClusterRepository` config). A ClusterRepository ref MUST map
// to a cluster-scoped lookup, never a namespaced Repository get. ---

#[test]
fn repo_lookup_namespaced_uses_ref_namespace() {
    let r = ref_of(RepositoryKind::Repository, "nas", Some("backups"));
    assert_eq!(
        repo_lookup(&r, "consumer-ns"),
        RepoLookup::Namespaced {
            namespace: "backups".into(),
            name: "nas".into(),
        }
    );
}

#[test]
fn repo_lookup_namespaced_defaults_to_consumer_namespace() {
    let r = ref_of(RepositoryKind::Repository, "nas", None);
    assert_eq!(
        repo_lookup(&r, "consumer-ns"),
        RepoLookup::Namespaced {
            namespace: "consumer-ns".into(),
            name: "nas".into(),
        }
    );
}

#[test]
fn repo_lookup_cluster_is_cluster_scoped_not_namespaced() {
    // This is the bug the user hit: a config referencing
    // `{ kind: ClusterRepository, name: hetzner }` was resolved as a
    // namespaced Repository in the consumer's namespace and never found.
    let r = ref_of(RepositoryKind::ClusterRepository, "hetzner", None);
    assert_eq!(
        repo_lookup(&r, "selfhosted"),
        RepoLookup::Cluster {
            name: "hetzner".into(),
        }
    );
}

#[test]
fn repo_lookup_cluster_ignores_a_stray_namespace() {
    // Even if `namespace` somehow slips through (webhook normally forbids it),
    // a ClusterRepository ref still resolves cluster-scoped — never namespaced.
    let r = ref_of(RepositoryKind::ClusterRepository, "hetzner", Some("oops"));
    assert_eq!(
        repo_lookup(&r, "selfhosted"),
        RepoLookup::Cluster {
            name: "hetzner".into(),
        }
    );
}

#[test]
fn repo_credentials_defaults_password_key() {
    let enc = Encryption {
        password_secret_ref: SecretKeyRef {
            name: "creds".into(),
            namespace: None,
            key: None,
        },
    };
    let c = repo_credentials(&enc);
    assert_eq!(c.secret_name, "creds");
    assert_eq!(c.password_key, "KOPIA_PASSWORD");
}

#[test]
fn repo_credentials_honors_explicit_key_and_namespace() {
    let enc = Encryption {
        password_secret_ref: SecretKeyRef {
            name: "creds".into(),
            namespace: Some("kopia-system".into()),
            key: Some("pw".into()),
        },
    };
    let c = repo_credentials(&enc);
    assert_eq!(c.password_key, "pw");
    assert_eq!(c.namespace.as_deref(), Some("kopia-system"));
}

// The filesystem_repo_path / filesystem_repo_mount_source tests moved with the
// fns to `kopiur_mover::repo_meta`.

#[test]
fn backend_auth_secret_for_s3_and_none_for_filesystem() {
    use kopiur_api::backend::{BackendAuth, S3Backend};
    use kopiur_api::common::SecretRef;
    let s3 = Backend::S3(S3Backend {
        bucket: "b".into(),
        prefix: None,
        endpoint: None,
        region: None,
        auth: Some(BackendAuth {
            secret_ref: Some(SecretRef {
                name: "s3-creds".into(),
                namespace: Some("kopiur-system".into()),
            }),
            workload_identity: None,
        }),
        tls: None,
    });
    assert_eq!(
        backend_auth_secret_ref(&s3).map(|s| s.name.as_str()),
        Some("s3-creds")
    );
    let fs = Backend::Filesystem(FilesystemBackend {
        path: "/repo".into(),
        volume: None,
    });
    assert!(backend_auth_secret_ref(&fs).is_none());
}

#[test]
fn mover_creds_dedupe_when_password_and_backend_share_a_secret() {
    use kopiur_api::backend::{BackendAuth, S3Backend};
    use kopiur_api::common::{SecretKeyRef, SecretRef};
    let enc = Encryption {
        password_secret_ref: SecretKeyRef {
            name: "kopia-rustfs-creds".into(),
            namespace: Some("kopiur-system".into()),
            key: None,
        },
    };
    // Same secret holds password + AWS keys (the homelab layout) -> one entry.
    let same = Backend::S3(S3Backend {
        bucket: "b".into(),
        prefix: None,
        endpoint: None,
        region: None,
        auth: Some(BackendAuth {
            secret_ref: Some(SecretRef {
                name: "kopia-rustfs-creds".into(),
                namespace: Some("kopiur-system".into()),
            }),
            workload_identity: None,
        }),
        tls: None,
    });
    assert_eq!(mover_creds_secrets(&same, &enc), vec!["kopia-rustfs-creds"]);

    // Separate secrets -> both, password first.
    let split = Backend::S3(S3Backend {
        bucket: "b".into(),
        prefix: None,
        endpoint: None,
        region: None,
        auth: Some(BackendAuth {
            secret_ref: Some(SecretRef {
                name: "s3-creds".into(),
                namespace: Some("kopiur-system".into()),
            }),
            workload_identity: None,
        }),
        tls: None,
    });
    assert_eq!(
        mover_creds_secrets(&split, &enc),
        vec!["kopia-rustfs-creds", "s3-creds"]
    );
}

#[test]
fn child_meta_omits_empty_labels() {
    let m = child_meta("n", "ns", BTreeMap::new(), None);
    assert_eq!(m.name.as_deref(), Some("n"));
    assert!(m.labels.is_none());
}

// --- child_labels always carries managed-by (§14(c)) --------------------

#[test]
fn child_labels_always_includes_managed_by() {
    // Empty extra → still has managed-by=kopiur.
    let l = child_labels(&[]);
    assert_eq!(
        l.get(crate::consts::MANAGED_BY_LABEL).map(String::as_str),
        Some("kopiur")
    );
    // Extra labels are merged in alongside managed-by.
    let l2 = child_labels(&[("kopiur.home-operations.com/config", "pg")]);
    assert_eq!(
        l2.get(crate::consts::MANAGED_BY_LABEL).map(String::as_str),
        Some("kopiur")
    );
    assert_eq!(
        l2.get("kopiur.home-operations.com/config")
            .map(String::as_str),
        Some("pg")
    );
}

// --- set_ready kstatus conditions (§2) ----------------------------------

#[test]
fn set_ready_emits_ready_reconciling_stalled_per_outcome() {
    // Ready → Ready=True, Reconciling=False, Stalled=False, with observedGeneration.
    let out = set_ready(&[], Some(7), ReadyOutcome::Ready, "Reconciled", "all good");
    let find = |t: &str| out.iter().find(|c| c.type_ == t).unwrap();
    assert_eq!(find("Ready").status, "True");
    assert_eq!(find("Ready").observed_generation, Some(7));
    assert_eq!(find("Reconciling").status, "False");
    assert_eq!(find("Stalled").status, "False");

    // Stalled (terminal) → Ready=False, Stalled=True.
    let out = set_ready(&[], Some(7), ReadyOutcome::Stalled, "Failed", "bad creds");
    let find = |t: &str| out.iter().find(|c| c.type_ == t).unwrap();
    assert_eq!(find("Ready").status, "False");
    assert_eq!(find("Stalled").status, "True");
    assert_eq!(find("Reconciling").status, "False");
}

#[test]
fn ready_outcome_for_phase_maps_every_phase() {
    // Issue #245: the phase→kstatus mapping used at every repository status write.
    use kopiur_api::RepositoryPhase;
    assert_eq!(
        ready_outcome_for_phase(&RepositoryPhase::Ready),
        ReadyOutcome::Ready
    );
    assert_eq!(
        ready_outcome_for_phase(&RepositoryPhase::Failed),
        ReadyOutcome::Stalled
    );
    // Reachable-but-unsettled and retryable-failure phases are Reconciling, never
    // a premature Ready and never a hard Stalled.
    for p in [
        RepositoryPhase::Pending,
        RepositoryPhase::Initializing,
        RepositoryPhase::Degraded,
        // Never Ready (a Flux `wait: true` check must not pass on a phase we
        // cannot read) and never Stalled (it may be progressing under a newer
        // operator) — Reconciling keeps the check waiting, the honest answer.
        RepositoryPhase::Unknown("Upgrading".into()),
    ] {
        assert_eq!(ready_outcome_for_phase(&p), ReadyOutcome::Reconciling);
    }
}

#[test]
fn set_ready_preserves_transition_time_when_unchanged_and_flips_on_change() {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
    // Seed Ready=True with a fixed transition time.
    let t0 = Time(k8s_openapi::jiff::Timestamp::from_second(1_700_000_000).unwrap());
    let seeded = vec![Condition {
        type_: "Ready".into(),
        status: "True".into(),
        reason: "Reconciled".into(),
        message: "ok".into(),
        last_transition_time: t0.clone(),
        observed_generation: Some(1),
    }];
    // Still Ready → Ready's transition time is preserved (no flip).
    let same = set_ready(&seeded, Some(2), ReadyOutcome::Ready, "Reconciled", "ok2");
    let ready = same.iter().find(|c| c.type_ == "Ready").unwrap();
    assert_eq!(ready.last_transition_time, t0, "Ready time moved on no-op");
    assert_eq!(ready.observed_generation, Some(2));

    // Flip to Stalled → Ready's status changes to False, so its time advances.
    let flipped = set_ready(&seeded, Some(2), ReadyOutcome::Stalled, "Failed", "boom");
    let ready = flipped.iter().find(|c| c.type_ == "Ready").unwrap();
    assert_ne!(ready.last_transition_time, t0, "Ready time must flip");
    assert_eq!(ready.status, "False");
}

// --- upsert_condition ---------------------------------------------------

#[test]
fn upsert_condition_inserts_new_and_preserves_others() {
    let other = Condition {
        type_: "Ready".into(),
        status: "True".into(),
        reason: "Ok".into(),
        message: "ready".into(),
        last_transition_time: Time(k8s_openapi::jiff::Timestamp::now()),
        observed_generation: Some(1),
    };
    let out = upsert_condition(
        std::slice::from_ref(&other),
        "MaintenanceConfigured",
        false,
        "MaintenanceNotConfigured",
        "no maintenance",
        Some(2),
    );
    assert_eq!(out.len(), 2);
    // Pre-existing condition is untouched.
    assert!(out.iter().any(|c| c.type_ == "Ready" && c.status == "True"));
    let m = out
        .iter()
        .find(|c| c.type_ == "MaintenanceConfigured")
        .unwrap();
    assert_eq!(m.status, "False");
    assert_eq!(m.reason, "MaintenanceNotConfigured");
    assert_eq!(m.observed_generation, Some(2));
}

#[test]
fn upsert_condition_preserves_transition_time_when_status_unchanged() {
    let t0 = Time(k8s_openapi::jiff::Timestamp::from_second(1_700_000_000).unwrap());
    let existing = vec![Condition {
        type_: "MaintenanceConfigured".into(),
        status: "False".into(),
        reason: "MaintenanceNotConfigured".into(),
        message: "old".into(),
        last_transition_time: t0.clone(),
        observed_generation: Some(1),
    }];
    // Same status (still False) -> timestamp must NOT move, but message updates.
    let out = upsert_condition(
        &existing,
        "MaintenanceConfigured",
        false,
        "MaintenanceNotConfigured",
        "new message",
        Some(2),
    );
    let m = &out[0];
    assert_eq!(m.last_transition_time, t0, "timestamp moved on no-op");
    assert_eq!(m.message, "new message");
    assert_eq!(m.observed_generation, Some(2));
}

#[test]
fn upsert_condition_bumps_transition_time_on_flip() {
    let t0 = Time(k8s_openapi::jiff::Timestamp::from_second(1_700_000_000).unwrap());
    let existing = vec![Condition {
        type_: "MaintenanceConfigured".into(),
        status: "False".into(),
        reason: "MaintenanceNotConfigured".into(),
        message: "old".into(),
        last_transition_time: t0.clone(),
        observed_generation: Some(1),
    }];
    // Flip False -> True: timestamp must advance.
    let out = upsert_condition(
        &existing,
        "MaintenanceConfigured",
        true,
        "MaintenanceConfigured",
        "now configured",
        Some(2),
    );
    let m = &out[0];
    assert_eq!(m.status, "True");
    assert_ne!(
        m.last_transition_time, t0,
        "timestamp did not advance on flip"
    );
}

#[test]
fn upsert_condition_is_order_stable() {
    // Re-upserting an existing condition must keep its POSITION, not move it to
    // the end. The old filter-out-then-append shape reordered the array on every
    // call, so a reconcile upserting `Bootstrapped` then `MaintenanceConfigured`
    // flipped the order on every pass — each status patch was a real write
    // (resourceVersion bump → watch event → re-reconcile) and the repository
    // controllers hot-looped at ~30 reconciles/s per object.
    let t0 = Time(k8s_openapi::jiff::Timestamp::from_second(1_700_000_000).unwrap());
    let cond = |type_: &str| Condition {
        type_: type_.into(),
        status: "True".into(),
        reason: "Ok".into(),
        message: "m".into(),
        last_transition_time: t0.clone(),
        observed_generation: Some(1),
    };
    let existing = vec![cond("Bootstrapped"), cond("MaintenanceConfigured")];
    let out = upsert_condition(&existing, "Bootstrapped", true, "Ok", "m", Some(1));
    assert_eq!(
        out, existing,
        "no-op upsert of an existing condition must be byte-identical (incl. order)"
    );

    // A NEW type still appends.
    let out = upsert_condition(&existing, "Ready", true, "Ok", "m", Some(1));
    assert_eq!(
        out.iter().map(|c| c.type_.as_str()).collect::<Vec<_>>(),
        ["Bootstrapped", "MaintenanceConfigured", "Ready"]
    );
}

#[test]
fn repeated_bootstrap_then_maintenance_upserts_converge() {
    // Regression for the ClusterRepository reconcile hot-loop: simulate the real
    // per-reconcile sequence — upsert `Bootstrapped` (finalize_cluster_bootstrap)
    // then `MaintenanceConfigured` (ensure_maintenance) — twice over. Pass 2 must
    // produce exactly pass 1's array, i.e. the status merge-patch is a no-op and
    // the reconciler does not re-trigger itself.
    let pass = |base: &[Condition]| {
        let conds = upsert_condition(base, "Bootstrapped", true, "Bootstrapped", "ok", Some(3));
        upsert_condition(
            &conds,
            "MaintenanceConfigured",
            true,
            "MaintenanceConfigured",
            "managed",
            Some(3),
        )
    };
    let first = pass(&[]);
    let second = pass(&first);
    assert_eq!(second, first, "identical reconcile pass churned conditions");
    // And the merge-patch predicate agrees: the second pass writes nothing.
    let current = serde_json::json!({ "conditions": first });
    let desired = serde_json::json!({ "conditions": second });
    assert!(
        status_patch_is_noop(Some(&current), &desired),
        "steady-state condition patch must be a server-side no-op"
    );
}

// --- idempotent status writes (the hot-loop fix) -------------------------

#[test]
fn status_patch_noop_when_subset_unchanged() {
    let current = serde_json::json!({
        "phase": "Failed",
        "backend": "Filesystem",
        "observedGeneration": 3,
        "conditions": [{ "type": "Bootstrapped", "status": "False", "reason": "PermissionDenied" }],
        "uniqueId": "abc",            // an extra key the desired doesn't touch
    });
    // Desired is a subset that matches → no-op (a merge patch never removes the
    // keys it omits, so we only compare the keys we'd write).
    let desired = serde_json::json!({
        "phase": "Failed",
        "backend": "Filesystem",
        "observedGeneration": 3,
        "conditions": [{ "type": "Bootstrapped", "status": "False", "reason": "PermissionDenied" }],
    });
    assert!(status_patch_is_noop(Some(&current), &desired));
}

#[test]
fn status_patch_not_noop_on_reason_or_generation_or_absent() {
    let current = serde_json::json!({
        "phase": "Failed",
        "observedGeneration": 3,
        "conditions": [{ "type": "Bootstrapped", "status": "False", "reason": "PermissionDenied" }],
    });
    // A new generation must write (the spec changed → re-attempt).
    let newer_gen = serde_json::json!({ "phase": "Failed", "observedGeneration": 4 });
    assert!(!status_patch_is_noop(Some(&current), &newer_gen));
    // A different condition reason must write.
    let new_reason = serde_json::json!({
        "conditions": [{ "type": "Bootstrapped", "status": "False", "reason": "AuthFailure" }],
    });
    assert!(!status_patch_is_noop(Some(&current), &new_reason));
    // No status at all (first reconcile) is never a no-op.
    assert!(!status_patch_is_noop(None, &newer_gen));
    assert!(!status_patch_is_noop(
        Some(&serde_json::Value::Null),
        &newer_gen
    ));
}

#[test]
fn status_patch_noop_ignores_volatile_message_only_when_message_matches() {
    // The condition message is now class-derived (stable). If two desired
    // payloads carry the SAME stable message + same reason/generation, the
    // second is a no-op. (A volatile message would differ here and force a
    // write — which is exactly the loop we removed by switching to summary().)
    let stable = "repository path is not writable by the operator's UID";
    let current = serde_json::json!({
        "phase": "Failed",
        "observedGeneration": 2,
        "conditions": [{ "type": "Bootstrapped", "status": "False", "reason": "PermissionDenied", "message": stable }],
    });
    let desired = serde_json::json!({
        "phase": "Failed",
        "observedGeneration": 2,
        "conditions": [{ "type": "Bootstrapped", "status": "False", "reason": "PermissionDenied", "message": stable }],
    });
    assert!(status_patch_is_noop(Some(&current), &desired));
}

/// The stored status a replication has after a run that finished: the previous
/// request, terminal, with its completion stamp.
fn finished_manual_run() -> serde_json::Value {
    serde_json::json!({
        "manualRun": {
            "requestedAt": "2026-06-11T12:00:00Z",
            "phase": "Succeeded",
            "completedAt": "2026-06-11T12:01:42Z",
        },
    })
}

/// The `desired` payload `patch_manual_run` builds for a FOLLOW-UP request that
/// has not started — built from the typed struct exactly as the controllers do.
fn pending_manual_run_patch() -> serde_json::Value {
    let manual = kopiur_api::common::ReplicationManualRunStatus {
        requested_at: Some("2026-06-11T13:00:00Z".into()),
        phase: Some(kopiur_api::common::ReplicationManualRunPhase::Pending),
        completed_at: None,
    };
    serde_json::json!({ "manualRun": manual })
}

#[test]
fn non_terminal_manual_run_patch_carries_an_explicit_null_completed_at() {
    // #394, the fix itself at the layer that consumes it: `Value::Index` maps a
    // MISSING key to `Null`, so this has to ask for the key with `.get()` —
    // present-and-null is the whole point, and absent is the bug.
    let desired = pending_manual_run_patch();
    assert_eq!(
        desired["manualRun"].get("completedAt"),
        Some(&serde_json::Value::Null),
        "a non-terminal manualRun must NAME completedAt so the merge-patch \
         clears the previous run's stamp; got {desired}"
    );
}

#[test]
fn manual_run_patch_converges_after_clearing_a_stale_completed_at() {
    // The #394 trap is a LOOP, not one pass, so this replays one: patch, let the
    // apiserver apply RFC-7386, rebuild `current` the way the replication
    // controllers do (re-serialize the TYPED status off the refreshed object),
    // and demand the second pass write nothing.
    //
    // Without the explicit null, `desired` simply omits `completedAt`; a merge
    // patch never removes what it omits, so the stale stamp SURVIVES, the
    // rebuilt `current` still differs from `desired`, and every queued pass
    // re-fires a PATCH the apiserver no-ops — forever.
    let desired = pending_manual_run_patch();
    let mut stored = finished_manual_run();
    assert!(
        !status_patch_is_noop(Some(&stored), &desired),
        "answering a NEW request must write"
    );

    // The apiserver applies the merge patch (this is `Patch::Merge`). Given the
    // explicit null it either DELETES the key — plain RFC-7386, what
    // `json_patch::merge` models — or STORES the null verbatim; a nullable CRD
    // field on k8s 1.33 was observed doing the latter. Both outcomes are
    // replayed below, because the guard has to converge under either.
    json_patch::merge(&mut stored, &desired);
    assert!(
        stored["manualRun"].get("completedAt").is_none(),
        "the merge-patch must have cleared the stale stamp; got {stored}"
    );

    // Next reconcile: `current` is the typed status of the refreshed object,
    // re-serialized — the exact round trip `patch_manual_run` performs.
    let converges = |stored: &serde_json::Value| {
        let typed: kopiur_api::common::ReplicationManualRunStatus =
            serde_json::from_value(stored["manualRun"].clone()).expect("stored manualRun decodes");
        let current = serde_json::json!({ "manualRun": typed });
        assert!(
            status_patch_is_noop(Some(&current), &desired),
            "the guard must CONVERGE once the stamp is cleared; current {current}, \
             desired {desired}"
        );
    };
    converges(&stored);

    // The other apiserver outcome: the null is STORED rather than deleted. It
    // decodes to `None` just the same (`#[serde(default)]` on an `Option`), so
    // the rebuilt `current` is identical and the guard converges here too.
    let stored_null = serde_json::json!({
        "manualRun": {
            "requestedAt": "2026-06-11T13:00:00Z",
            "phase": "Pending",
            "completedAt": null,
        },
    });
    converges(&stored_null);
}

#[test]
fn terminal_gate_only_on_failed_at_current_generation() {
    use kopiur_api::RepositoryPhase;
    // Failed at the current generation → terminal (hard-stop).
    assert!(is_terminal_for_generation(
        Some(&RepositoryPhase::Failed),
        Some(5),
        Some(5)
    ));
    // Failed but the spec moved on (gen bumped) → gate reopens, re-attempt.
    assert!(!is_terminal_for_generation(
        Some(&RepositoryPhase::Failed),
        Some(5),
        Some(6)
    ));
    // Degraded (a retryable failure) is never terminal — keep retrying.
    assert!(!is_terminal_for_generation(
        Some(&RepositoryPhase::Degraded),
        Some(5),
        Some(5)
    ));
    // No generation yet / no observed generation → not terminal.
    assert!(!is_terminal_for_generation(
        Some(&RepositoryPhase::Failed),
        None,
        Some(5)
    ));
    assert!(!is_terminal_for_generation(
        Some(&RepositoryPhase::Failed),
        Some(5),
        None
    ));
    // A phase written by a NEWER operator is never a hard stop: parking someone
    // else's repository forever on an unreadable phase is worse than one extra
    // idempotent connect attempt.
    assert!(!is_terminal_for_generation(
        Some(&RepositoryPhase::Unknown("Upgrading".into())),
        Some(5),
        Some(5)
    ));
}

#[test]
fn terminal_gate_reopens_when_credential_secret_changes() {
    use kopiur_api::RepositoryPhase;
    // Terminally Failed at gen 5; the password Secret recorded at failure was rv "100".
    let failed = |recorded: Option<&str>, current: &str| {
        terminal_gate_holds(
            Some(&RepositoryPhase::Failed),
            Some(5),
            Some(5),
            recorded,
            current,
        )
    };
    // Same Secret revision → gate HOLDS (quiet heartbeat, don't re-hit the backend).
    assert!(failed(Some("100"), "100"));
    // The Secret's content was edited (rv bumped) → gate REOPENS even though the
    // generation is unchanged. This is the regression fix: a fixed password Secret
    // re-triggers a connect instead of parking the repo as Failed forever.
    assert!(!failed(Some("100"), "200"));
    // First failure recorded no version (older status / upgrade) → reopen, re-attempt.
    assert!(!failed(None, "100"));
    // A non-terminal phase never holds, regardless of the version match.
    assert!(!terminal_gate_holds(
        Some(&RepositoryPhase::Degraded),
        Some(5),
        Some(5),
        Some("100"),
        "100"
    ));
    // A spec change (gen bumped) reopens regardless of the version match.
    assert!(!terminal_gate_holds(
        Some(&RepositoryPhase::Failed),
        Some(5),
        Some(6),
        Some("100"),
        "100"
    ));
}

// --- managed Maintenance projection (ADR §3.7, default-on) ---------------

fn dummy_owner(kind: &str, name: &str) -> OwnerReference {
    OwnerReference {
        api_version: API_VERSION.into(),
        kind: kind.into(),
        name: name.into(),
        uid: "uid-1".into(),
        controller: Some(true),
        block_owner_deletion: Some(false),
    }
}

#[test]
fn build_managed_maintenance_for_namespaced_repository() {
    let spec = RepositoryMaintenanceSpec::default();
    let m = build_managed_maintenance(
        RepositoryKind::Repository,
        "nas",
        "apps",
        &spec,
        dummy_owner("Repository", "nas"),
        None,
    );
    // 1:1 naming, lives in the repository's namespace, owned by the repo.
    assert_eq!(m.metadata.name.as_deref(), Some("nas"));
    assert_eq!(m.metadata.namespace.as_deref(), Some("apps"));
    assert!(is_managed_by(&m, "Repository", "nas"));
    // Same-namespace ref (namespace omitted), default schedule, default lease.
    assert_eq!(m.spec.repository.kind, RepositoryKind::Repository);
    assert_eq!(m.spec.repository.name, "nas");
    assert!(m.spec.repository.namespace.is_none());
    assert_eq!(m.spec.schedule, default_maintenance_schedule());
    assert_eq!(m.spec.ownership.owner, "kopiur/apps/nas");
    assert!(m.spec.ownership.owner_aliases.is_empty());
    assert_eq!(
        m.spec.ownership.takeover_policy,
        kopiur_api::TakeoverPolicy::Never
    );
}

#[test]
fn build_managed_maintenance_for_namespaced_repository_with_cluster_gains_alias() {
    // M6: once identityDefaults.cluster is set, the managed CR's lease is
    // cluster-qualified AND carries the pre-cluster lease as its sole alias —
    // the migration path so turning cluster identity on doesn't make the
    // repository's own prior lease look foreign to itself.
    let spec = RepositoryMaintenanceSpec::default();
    let m = build_managed_maintenance(
        RepositoryKind::Repository,
        "nas",
        "apps",
        &spec,
        dummy_owner("Repository", "nas"),
        Some("east"),
    );
    assert_eq!(m.spec.ownership.owner, "kopiur/east/apps/nas");
    assert_eq!(m.spec.ownership.owner_aliases, vec!["kopiur/apps/nas"]);
}

#[test]
fn build_managed_maintenance_for_cluster_repository_uses_overrides() {
    use kopiur_api::common::CronSpec;
    use kopiur_api::{MaintenanceSchedule, TakeoverPolicy};
    let spec = RepositoryMaintenanceSpec {
        enabled: true,
        schedule: Some(MaintenanceSchedule {
            quick: CronSpec {
                cron: "0 */2 * * *".into(),
                jitter: None,
                timezone: None,
            },
            full: CronSpec {
                cron: "0 1 * * *".into(),
                jitter: None,
                timezone: None,
            },
            timezone: Some("UTC".into()),
        }),
        takeover_policy: Some(TakeoverPolicy::Force),
        namespace: Some("kopia-system".into()),
        ..Default::default()
    };
    let m = build_managed_maintenance(
        RepositoryKind::ClusterRepository,
        "hetzner",
        "kopia-system",
        &spec,
        dummy_owner("ClusterRepository", "hetzner"),
        None,
    );
    assert_eq!(m.metadata.namespace.as_deref(), Some("kopia-system"));
    assert_eq!(m.spec.repository.kind, RepositoryKind::ClusterRepository);
    // Cluster ref must never carry a namespace.
    assert!(m.spec.repository.namespace.is_none());
    assert_eq!(m.spec.schedule.quick.cron, "0 */2 * * *");
    assert_eq!(m.spec.ownership.owner, "kopiur/clusterrepository/hetzner");
    assert!(m.spec.ownership.owner_aliases.is_empty());
    assert_eq!(m.spec.ownership.takeover_policy, TakeoverPolicy::Force);
}

#[test]
fn build_managed_maintenance_for_cluster_repository_with_cluster_gains_alias() {
    let spec = RepositoryMaintenanceSpec::default();
    let m = build_managed_maintenance(
        RepositoryKind::ClusterRepository,
        "hetzner",
        "kopia-system",
        &spec,
        dummy_owner("ClusterRepository", "hetzner"),
        Some("east"),
    );
    assert_eq!(
        m.spec.ownership.owner,
        "kopiur/east/clusterrepository/hetzner"
    );
    assert_eq!(
        m.spec.ownership.owner_aliases,
        vec!["kopiur/clusterrepository/hetzner"]
    );
}

#[test]
fn bootstrap_maintenance_owner_plan_suppressed_is_always_none_any_stale_no_aliases() {
    // Suppressed (ReadOnly / disabled / foreign-covered) must NEVER stamp or
    // restamp, regardless of cluster — this is the ReadOnly-clobbers-owner fix.
    for cluster in [None, Some("east")] {
        let (owner, policy, aliases) = bootstrap_maintenance_owner_plan(
            RepositoryKind::Repository,
            "apps",
            "nas",
            cluster,
            true,
        );
        assert_eq!(owner, None, "cluster={cluster:?}");
        assert_eq!(policy, RestampPolicy::AnyStale, "cluster={cluster:?}");
        assert!(aliases.is_empty(), "cluster={cluster:?}");
    }
}

#[test]
fn bootstrap_maintenance_owner_plan_no_cluster_is_any_stale_no_aliases() {
    let (owner, policy, aliases) =
        bootstrap_maintenance_owner_plan(RepositoryKind::Repository, "apps", "nas", None, false);
    assert_eq!(owner.as_deref(), Some("kopiur@kopiur-apps-nas"));
    assert_eq!(policy, RestampPolicy::AnyStale);
    assert!(aliases.is_empty());
}

#[test]
fn bootstrap_maintenance_owner_plan_with_cluster_is_own_formats_only_with_legacy_alias() {
    let (owner, policy, aliases) = bootstrap_maintenance_owner_plan(
        RepositoryKind::Repository,
        "apps",
        "nas",
        Some("east"),
        false,
    );
    assert_eq!(owner.as_deref(), Some("kopiur@kopiur.east.apps.nas"));
    assert_eq!(policy, RestampPolicy::OwnFormatsOnly);
    assert_eq!(aliases, vec!["kopiur@kopiur-apps-nas".to_string()]);
}

/// M6 regression (fix round 1): the in-process create path must stamp the
/// desired owner UNCONDITIONALLY — never defer to `maintenance_restamp_target`
/// with a hardcoded `created: false`, which under `OwnFormatsOnly` (whenever
/// `cluster` is set) would refuse to restamp a freshly-created repo's
/// kopia-assigned ephemeral owner forever. The create-vs-connect distinction
/// is the entire point of this test, exercised across both the cluster-qualified
/// and legacy owner shapes.
#[test]
fn in_process_create_owner_target_covers_the_create_vs_connect_matrix() {
    // created=true stamps the cluster-qualified owner unconditionally.
    let (desired, _, _) = bootstrap_maintenance_owner_plan(
        RepositoryKind::Repository,
        "apps",
        "nas",
        Some("east"),
        false,
    );
    assert_eq!(
        in_process_create_owner_target(true, desired.as_deref()),
        Some("kopiur@kopiur.east.apps.nas")
    );

    // created=true stamps the legacy owner unconditionally when cluster is unset.
    let (desired, _, _) =
        bootstrap_maintenance_owner_plan(RepositoryKind::Repository, "apps", "nas", None, false);
    assert_eq!(
        in_process_create_owner_target(true, desired.as_deref()),
        Some("kopiur@kopiur-apps-nas")
    );

    // created=false (connect-to-existing) always defers to the self-heal path,
    // regardless of cluster.
    for cluster in [None, Some("east")] {
        let (desired, _, _) = bootstrap_maintenance_owner_plan(
            RepositoryKind::Repository,
            "apps",
            "nas",
            cluster,
            false,
        );
        assert_eq!(
            in_process_create_owner_target(false, desired.as_deref()),
            None,
            "cluster={cluster:?}"
        );
    }

    // Suppressed (desired=None) never stamps, regardless of created.
    for created in [true, false] {
        assert_eq!(
            in_process_create_owner_target(created, None),
            None,
            "created={created}"
        );
    }
}

#[test]
fn maintenance_action_covers_the_matrix() {
    use MaintenanceAction::*;
    // enabled, no foreign, placement resolved -> manage.
    assert_eq!(maintenance_action(true, false, false, true), Manage);
    assert_eq!(maintenance_action(true, false, true, true), Manage);
    // enabled, no foreign, placement UNresolved -> unresolved.
    assert_eq!(maintenance_action(true, false, false, false), Unresolved);
    // foreign present -> never manage; remove a stale managed one.
    assert_eq!(maintenance_action(true, true, true, true), Unmanage);
    assert_eq!(maintenance_action(true, true, false, true), Leave);
    // disabled -> remove managed if any, else leave (never warns/ignores foreign).
    assert_eq!(maintenance_action(false, false, true, true), Unmanage);
    assert_eq!(maintenance_action(false, false, false, true), Leave);
    assert_eq!(maintenance_action(false, true, true, true), Unmanage);
    assert_eq!(maintenance_action(false, true, false, true), Leave);
}

/// #231: every NOT-covered state must say why it is not covered. The old code branded all
/// of them `MaintenanceDisabled` with a message asserting `spec.maintenance.enabled: false`
/// and "no Maintenance references it" — both false when an apply had merely failed, which
/// pointed operators at entirely the wrong knob.
#[test]
fn maintenance_condition_covers_every_coverage_state() {
    use crate::consts::{
        MAINTENANCE_APPLY_FAILED_REASON, MAINTENANCE_CONFIGURED_REASON,
        MAINTENANCE_DISABLED_REASON, MAINTENANCE_NAMESPACE_UNRESOLVED_REASON,
    };

    // Covered, by either party → True, no warning.
    let (status, reason, msg, warn) = maintenance_condition(
        &MaintenanceCoverage::CoveredByManaged,
        "ClusterRepository",
        "expanse",
    );
    assert!(status);
    assert_eq!(reason, MAINTENANCE_CONFIGURED_REASON);
    assert!(!warn);
    assert!(msg.contains("the operator manages a Maintenance"), "{msg}");

    let (status, reason, msg, warn) = maintenance_condition(
        &MaintenanceCoverage::CoveredByForeign,
        "Repository",
        "vault",
    );
    assert!(status);
    assert_eq!(reason, MAINTENANCE_CONFIGURED_REASON);
    assert!(!warn);
    assert!(msg.contains("externally-authored"), "{msg}");

    // A deliberate opt-out keeps its message — which is now provably true, since
    // `maintenance_action` only reaches this state with `enabled == false`.
    let (status, reason, msg, warn) =
        maintenance_condition(&MaintenanceCoverage::DisabledBySpec, "Repository", "vault");
    assert!(!status);
    assert_eq!(reason, MAINTENANCE_DISABLED_REASON);
    assert!(!warn, "a deliberate opt-out must not warn");
    assert!(msg.contains("spec.maintenance.enabled: false"), "{msg}");

    // THE #231 STATE: enabled, nothing covers the repo, and the apply failed. It must NOT
    // read as "disabled", and it must name the namespace so the operator can act.
    let (status, reason, msg, warn) = maintenance_condition(
        &MaintenanceCoverage::ApplyFailed {
            namespace: "kopiur-system".into(),
        },
        "ClusterRepository",
        "expanse",
    );
    assert!(!status);
    assert_eq!(reason, MAINTENANCE_APPLY_FAILED_REASON);
    assert!(warn, "a failed apply is a real problem: warn");
    assert!(msg.contains("ENABLED"), "{msg}");
    assert!(msg.contains("kopiur-system"), "{msg}");
    assert!(msg.contains("RBAC"), "{msg}");
    assert!(
        !msg.contains("spec.maintenance.enabled: false"),
        "must never claim the user disabled maintenance: {msg}"
    );
    // The message is a pure function of (coverage, kind, name) — no interpolated apply
    // error. `ensure_maintenance` now runs on EVERY reconcile and suppresses an unchanged
    // condition by BYTE COMPARISON, so an error string that varies between attempts (a
    // Conflict naming a resourceVersion) would defeat that guard and spin status writes +
    // Events at full speed. The verbatim error belongs in the log, not the condition.
    let (_, _, again, _) = maintenance_condition(
        &MaintenanceCoverage::ApplyFailed {
            namespace: "kopiur-system".into(),
        },
        "ClusterRepository",
        "expanse",
    );
    assert_eq!(msg, again, "the condition message must be byte-stable");

    // Unplaceable cluster-repo Maintenance keeps its own distinct reason.
    let (status, reason, _, warn) = maintenance_condition(
        &MaintenanceCoverage::Unresolved,
        "ClusterRepository",
        "expanse",
    );
    assert!(!status);
    assert_eq!(reason, MAINTENANCE_NAMESPACE_UNRESOLVED_REASON);
    assert!(warn);
}

fn maint_referencing(
    name: &str,
    ns: &str,
    r: RepositoryRef,
    owner: Option<OwnerReference>,
) -> Maintenance {
    let mut m = Maintenance::new(
        name,
        MaintenanceSpec {
            repository: r,
            schedule: default_maintenance_schedule(),
            ownership: Ownership {
                owner: "lease".into(),
                owner_aliases: Vec::new(),
                takeover_policy: Default::default(),
            },
            mover: None,
            failure_policy: None,
            credential_projection: None,
        },
    );
    m.metadata.namespace = Some(ns.into());
    m.metadata.owner_references = owner.map(|o| vec![o]);
    m
}

#[test]
fn repo_status_to_inputs_maps_fields_and_sentinels() {
    use kopiur_api::preflight::UNKNOWN_AGE;
    use kopiur_api::repository::{RepositoryHealthStatus, RepositoryPhase, StorageStats};
    let now = chrono::DateTime::parse_from_rfc3339("2026-01-01T01:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let storage = StorageStats {
        snapshot_count: Some(7),
        total_size: None,
        total_size_bytes: Some(4096),
        last_observed_at: None,
        index_blob_count: Some(3),
    };
    let health = RepositoryHealthStatus {
        last_healthy_at: Some("2026-01-01T00:00:00Z".into()),
        ..Default::default()
    };
    let conds = vec![Condition {
        type_: "BackendReachable".into(),
        status: "False".into(),
        reason: "RepositoryVanished".into(),
        message: "x".into(),
        last_transition_time: Time(
            k8s_openapi::jiff::Timestamp::from_second(1_700_000_000).unwrap(),
        ),
        observed_generation: None,
    }];
    let inputs = repo_status_to_inputs(
        Some(&RepositoryPhase::Ready),
        &conds,
        Some(&storage),
        Some(&health),
        Some("2026-01-01T00:30:00Z"),
        now,
    );
    assert_eq!(inputs.repository_phase, "Ready");
    assert!(inputs.repository_ready);
    assert!(!inputs.backend_reachable, "BackendReachable=False ⇒ false");
    assert_eq!(inputs.snapshot_count, 7);
    assert!(inputs.snapshot_count_known);
    assert_eq!(inputs.index_blob_count, 3);
    assert!(inputs.index_blob_count_known);
    assert_eq!(inputs.size_bytes, 4096);
    assert!(inputs.size_bytes_known);
    assert!(inputs.last_healthy_known);
    assert_eq!(inputs.last_healthy_age_seconds, 3600);
    assert!(inputs.last_reverify_known);
    assert_eq!(inputs.last_reverify_age_seconds, 1800);

    // Absent status ⇒ fail-closed sentinels; absent BackendReachable ⇒ reachable.
    let empty = repo_status_to_inputs(None, &[], None, None, None, now);
    assert!(!empty.repository_ready);
    assert!(
        empty.backend_reachable,
        "no condition ⇒ no evidence of failure"
    );
    assert_eq!(empty.snapshot_count, UNKNOWN_AGE);
    assert!(!empty.snapshot_count_known, "unobserved count ⇒ not known");
    assert_eq!(empty.size_bytes, UNKNOWN_AGE);
    assert!(!empty.size_bytes_known);
    assert!(!empty.index_blob_count_known);
    assert!(!empty.last_healthy_known);
    assert_eq!(empty.last_healthy_age_seconds, UNKNOWN_AGE);
}

#[test]
fn maintenance_recency_takes_max_across_modes_and_matches() {
    use kopiur_api::maintenance::{MaintenanceStatus, RunStatus};
    use kopiur_api::preflight::UNKNOWN_AGE;
    let now = chrono::DateTime::parse_from_rfc3339("2026-01-01T02:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);

    let mut m = maint_referencing(
        "nas",
        "apps",
        ref_of(RepositoryKind::Repository, "nas", None),
        Some(dummy_owner("Repository", "nas")),
    );
    m.status = Some(MaintenanceStatus {
        // full handled at 00:00; quick *ran* at 01:30 (the most recent — e.g. a
        // manual run-now, mover-written `lastRunAt`). Max ⇒ 30 min ago.
        full: Some(RunStatus {
            last_handled_at: Some("2026-01-01T00:00:00Z".into()),
            ..Default::default()
        }),
        quick: Some(RunStatus {
            last_run_at: Some("2026-01-01T01:30:00Z".into()),
            ..Default::default()
        }),
        ..Default::default()
    });
    // Unrelated repo — must be ignored.
    let other = maint_referencing(
        "other",
        "apps",
        ref_of(RepositoryKind::Repository, "different", None),
        None,
    );

    let (has_run, age) = maintenance_recency(
        vec![m.clone(), other],
        RepositoryKind::Repository,
        "nas",
        Some("apps"),
        now,
    );
    assert!(has_run);
    assert_eq!(
        age, 1800,
        "max of full/quick lastRun/lastHandled = 01:30 ⇒ 30m"
    );

    // No matching Maintenance ⇒ fail-closed.
    let (has_run, age) = maintenance_recency(
        Vec::<Maintenance>::new(),
        RepositoryKind::Repository,
        "nas",
        Some("apps"),
        now,
    );
    assert!(!has_run);
    assert_eq!(age, UNKNOWN_AGE);
}

#[test]
fn classify_maintenance_distinguishes_managed_foreign_and_unrelated() {
    let managed = maint_referencing(
        "nas",
        "apps",
        ref_of(RepositoryKind::Repository, "nas", None),
        Some(dummy_owner("Repository", "nas")),
    );
    let foreign = maint_referencing(
        "user-maint",
        "apps",
        ref_of(RepositoryKind::Repository, "nas", None),
        None,
    );
    let unrelated = maint_referencing(
        "other",
        "apps",
        ref_of(RepositoryKind::Repository, "different", None),
        None,
    );

    // Managed only.
    let (f, m) = classify_maintenance(
        vec![managed.clone(), unrelated.clone()],
        RepositoryKind::Repository,
        "Repository",
        "nas",
        Some("apps"),
    );
    assert!(!f);
    assert_eq!(
        m.as_ref().and_then(|m| m.metadata.name.as_deref()),
        Some("nas")
    );

    // Foreign only.
    let (f, m) = classify_maintenance(
        vec![foreign.clone()],
        RepositoryKind::Repository,
        "Repository",
        "nas",
        Some("apps"),
    );
    assert!(f);
    assert!(m.is_none());

    // Both present: foreign flagged AND managed found (so a stale managed one
    // is removed while deferring to the user's).
    let (f, m) = classify_maintenance(
        vec![managed, foreign],
        RepositoryKind::Repository,
        "Repository",
        "nas",
        Some("apps"),
    );
    assert!(f);
    assert!(m.is_some());
}

#[test]
fn classify_maintenance_matches_cluster_repository_by_owner_ref() {
    let managed = maint_referencing(
        "hetzner",
        "kopia-system",
        ref_of(RepositoryKind::ClusterRepository, "hetzner", None),
        Some(dummy_owner("ClusterRepository", "hetzner")),
    );
    let (f, m) = classify_maintenance(
        vec![managed],
        RepositoryKind::ClusterRepository,
        "ClusterRepository",
        "hetzner",
        None,
    );
    assert!(!f);
    assert!(m.is_some());
}

// --- mover RBAC minting (ADR §4.12): the controller mints a least-privilege
// mover SA + RoleBinding in each mover Job's namespace, because the Job runs in
// the workload namespace where the operator SA does not exist. The pure builders
// are asserted here; the live apply is covered by e2e. ---

#[test]
fn mover_service_account_is_named_and_namespaced_and_managed() {
    let sa = build_mover_service_account("trilium", "kopiur-mover");
    assert_eq!(sa.metadata.name.as_deref(), Some("kopiur-mover"));
    assert_eq!(sa.metadata.namespace.as_deref(), Some("trilium"));
    let labels = sa.metadata.labels.expect("managed labels");
    assert_eq!(
        labels
            .get("app.kubernetes.io/managed-by")
            .map(String::as_str),
        Some("kopiur")
    );
    assert_eq!(
        labels
            .get("app.kubernetes.io/component")
            .map(String::as_str),
        Some("mover")
    );
}

#[test]
fn mover_rolebinding_binds_the_namespaced_sa_to_the_named_role() {
    let rb = build_mover_rolebinding("trilium", "kopiur-mover", "ClusterRole", "kopiur-mover");
    // Binding lives in the workload namespace.
    assert_eq!(rb.metadata.namespace.as_deref(), Some("trilium"));
    // roleRef points at the chart-shipped ClusterRole by name.
    assert_eq!(rb.role_ref.api_group, "rbac.authorization.k8s.io");
    assert_eq!(rb.role_ref.kind, "ClusterRole");
    assert_eq!(rb.role_ref.name, "kopiur-mover");
    // The single subject is the minted SA in this namespace (not the operator's).
    let subjects = rb.subjects.expect("one subject");
    assert_eq!(subjects.len(), 1);
    assert_eq!(subjects[0].kind, "ServiceAccount");
    assert_eq!(subjects[0].name, "kopiur-mover");
    assert_eq!(subjects[0].namespace.as_deref(), Some("trilium"));
}

#[test]
fn mover_rolebinding_uses_role_kind_for_namespaced_install() {
    // A namespaced install binds to a Role (in the operator namespace), not a
    // cluster-scoped ClusterRole.
    let rb = build_mover_rolebinding("apps", "kopiur-mover", "Role", "kopiur-mover");
    assert_eq!(rb.role_ref.kind, "Role");
}

// --- workload-identity run identity (ADR §4.11): a backend with
// `auth.workloadIdentity` runs the mover as the USER'S ServiceAccount. The
// controller never creates that SA (its cloud annotations are the user's
// federation contract) — it preflights it and binds the mover role to it. ---

#[test]
fn wi_rolebinding_binds_the_user_sa_under_a_distinct_name() {
    let rb = build_wi_rolebinding("trilium", "backup-mover", "ClusterRole", "kopiur-mover");
    // A name distinct from the minted-SA binding (named after the mover SA), so
    // the two server-side applies can never clobber each other.
    assert_eq!(
        rb.metadata.name.as_deref(),
        Some("kopiur-mover-wi-backup-mover")
    );
    assert_eq!(rb.metadata.namespace.as_deref(), Some("trilium"));
    assert_eq!(rb.role_ref.name, "kopiur-mover");
    let subjects = rb.subjects.expect("one subject");
    assert_eq!(subjects.len(), 1);
    assert_eq!(subjects[0].name, "backup-mover");
    assert_eq!(subjects[0].namespace.as_deref(), Some("trilium"));
}

#[test]
fn wi_rolebinding_name_truncates_long_sa_names_with_a_stable_hash() {
    let long_sa = "a".repeat(260);
    let name = wi_rolebinding_name(&long_sa);
    assert!(name.len() <= 253, "got {} chars", name.len());
    assert!(name.starts_with("kopiur-mover-wi-a"));
    // Deterministic: the same SA always yields the same binding name (SSA idempotence).
    assert_eq!(name, wi_rolebinding_name(&long_sa));
    // Distinct long names stay distinct (the hash carries the difference).
    assert_ne!(name, wi_rolebinding_name(&format!("{}b", "a".repeat(259))));
}

#[test]
fn snapshot_replication_mover_name_derives_from_the_generic_role() {
    // The default chart wiring (`<fullname>-mover`) yields the exact name
    // `gen-rbac` ships and the chart's snapshotReplicationMoverName helper
    // renders, so the runtime binding always references an existing role.
    assert_eq!(
        snapshot_replication_mover_name("kopiur-mover"),
        "kopiur-snapshot-replication-mover"
    );
    assert_eq!(
        snapshot_replication_mover_name("myrelease-kopiur-mover"),
        "myrelease-kopiur-snapshot-replication-mover"
    );
    // A custom role name without the conventional suffix still derives
    // deterministically (and distinctly from the generic role).
    assert_eq!(
        snapshot_replication_mover_name("custom-role"),
        "custom-role-snapshot-replication"
    );
}

#[test]
fn missing_wi_sa_message_is_actionable_per_cloud() {
    use kopiur_api::creds::WorkloadIdentityCloud;
    for (cloud, annotation) in [
        (WorkloadIdentityCloud::S3, "eks.amazonaws.com/role-arn"),
        (
            WorkloadIdentityCloud::Azure,
            "azure.workload.identity/client-id",
        ),
        (WorkloadIdentityCloud::Gcs, "iam.gke.io/gcp-service-account"),
    ] {
        let msg = missing_workload_identity_sa_message("backup-mover", "trilium", cloud);
        // What: the exact SA and namespace.
        assert!(msg.contains("`backup-mover`"), "{msg}");
        assert!(msg.contains("`trilium`"), "{msg}");
        // Why: kopiur never creates it.
        assert!(msg.contains("never creates"), "{msg}");
        // Fix: the cloud-specific federation annotation.
        assert!(msg.contains(annotation), "{msg}");
    }
}

#[test]
fn mover_run_identity_decorates_the_azure_label_only_when_azure() {
    let azure = MoverRunIdentity {
        service_account: Some("backup-mover".into()),
        azure_workload_identity: true,
    };
    let mut labels = std::collections::BTreeMap::new();
    azure.decorate_labels(&mut labels);
    assert_eq!(
        labels
            .get(kopiur_api::consts::AZURE_WORKLOAD_IDENTITY_LABEL)
            .map(String::as_str),
        Some("true")
    );

    let plain = MoverRunIdentity {
        service_account: Some("kopiur-mover".into()),
        azure_workload_identity: false,
    };
    let mut labels = std::collections::BTreeMap::new();
    plain.decorate_labels(&mut labels);
    assert!(labels.is_empty());
}

// --- missing-credentials message (load-bearing UX, ADR §4.12): names the
// Secret + namespace, says WHY (namespace-local envFrom), says WHERE the repo
// keeps it, and gives concrete fixes. ---

#[test]
fn missing_creds_message_cross_namespace_is_actionable() {
    let names = vec!["kopia-rustfs-creds".to_string()];
    let ctx = CredsContext {
        secret_names: &names,
        repo_kind: "ClusterRepository",
        repo_name: "rustfs-kopiur-test",
        repo_secret_namespace: Some("kopiur-system"),
    };
    let msg = missing_creds_message("kopia-rustfs-creds", "trilium", &ctx);
    // What: the exact Secret and the namespace it is missing from.
    assert!(msg.contains("kopia-rustfs-creds"));
    assert!(msg.contains("`trilium`"));
    // Why: namespace-local envFrom.
    assert!(msg.contains("envFrom"));
    // Where it currently lives: repo kind/name + its secret namespace.
    assert!(msg.contains("ClusterRepository"));
    assert!(msg.contains("rustfs-kopiur-test"));
    assert!(msg.contains("`kopiur-system`"));
    // How to fix: create it here, or use a namespaced Repository.
    assert!(msg.contains("create a Secret"));
    assert!(msg.contains("namespaced Repository"));
}

#[test]
fn missing_creds_message_same_namespace_drops_cross_ns_clause() {
    let names = vec!["nas-creds".to_string()];
    let ctx = CredsContext {
        secret_names: &names,
        repo_kind: "Repository",
        repo_name: "nas-primary",
        // Same-namespace reference (a namespaced Repository): no explicit ns.
        repo_secret_namespace: None,
    };
    let msg = missing_creds_message("nas-creds", "billing", &ctx);
    assert!(msg.contains("nas-creds"));
    assert!(msg.contains("`billing`"));
    assert!(msg.contains("envFrom"));
    assert!(msg.contains("create a Secret"));
    // No cross-namespace "keeps that Secret in namespace" clause when same-ns.
    assert!(!msg.contains("keeps that Secret in namespace"));
    assert!(!msg.contains("namespaced Repository"));
}

#[test]
fn missing_creds_message_treats_matching_secret_ns_as_same_namespace() {
    // An explicit secret namespace equal to the job namespace is NOT a mismatch.
    let names = vec!["creds".to_string()];
    let ctx = CredsContext {
        secret_names: &names,
        repo_kind: "Repository",
        repo_name: "local",
        repo_secret_namespace: Some("billing"),
    };
    let msg = missing_creds_message("creds", "billing", &ctx);
    assert!(!msg.contains("keeps that Secret in namespace"));
}

#[test]
fn repo_kind_str_maps_both_variants() {
    assert_eq!(repo_kind_str(RepositoryKind::Repository), "Repository");
    assert_eq!(
        repo_kind_str(RepositoryKind::ClusterRepository),
        "ClusterRepository"
    );
}

#[test]
fn privileged_mover_message_is_actionable() {
    let msg = privileged_mover_message("SnapshotPolicy", "trilium-rain", "trilium", "kopiur-mover");
    // What: the owning kind + name + namespace.
    assert!(msg.contains("SnapshotPolicy `trilium-rain`"));
    assert!(msg.contains("`trilium`"));
    // Why: tenant could reuse the minted SA at that privilege.
    assert!(msg.contains("kopiur-mover"));
    assert!(msg.contains("reuse"));
    // How: the exact annotate command with the real annotation key.
    assert!(msg.contains("kubectl annotate namespace trilium"));
    assert!(msg.contains(PRIVILEGED_MOVERS_ANNOTATION));
    assert!(msg.contains("=true"));
    // Alternative fix: drop the elevated context, named for the right object.
    assert!(msg.contains("securityContext"));
    assert!(msg.contains("from the SnapshotPolicy `spec.mover`"));
}

#[test]
fn privileged_mover_message_names_restore_kind() {
    // The same gate guards restores; the message must name the Restore to fix.
    let msg = privileged_mover_message("Restore", "pg-restore", "billing", "kopiur-mover");
    assert!(msg.contains("Restore `pg-restore`"));
    assert!(msg.contains("from the Restore `spec.mover`"));
    assert!(msg.contains("kubectl annotate namespace billing"));
}

#[test]
fn label_selector_to_string_covers_labels_and_expressions() {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, LabelSelectorRequirement};

    // matchLabels render as comma-joined key=value (BTreeMap → deterministic order).
    let mut match_labels = BTreeMap::new();
    match_labels.insert("app".to_string(), "postgres".to_string());
    match_labels.insert("tier".to_string(), "db".to_string());
    let sel = LabelSelector {
        match_labels: Some(match_labels),
        match_expressions: Some(vec![
            LabelSelectorRequirement {
                key: "role".into(),
                operator: "In".into(),
                values: Some(vec!["primary".into(), "replica".into()]),
            },
            LabelSelectorRequirement {
                key: "canary".into(),
                operator: "DoesNotExist".into(),
                values: None,
            },
            LabelSelectorRequirement {
                key: "env".into(),
                operator: "NotIn".into(),
                values: Some(vec!["dev".into()]),
            },
            LabelSelectorRequirement {
                key: "managed".into(),
                operator: "Exists".into(),
                values: None,
            },
        ]),
    };
    assert_eq!(
        label_selector_to_string(&sel),
        "app=postgres,tier=db,role in (primary,replica),!canary,env notin (dev),managed"
    );

    // An empty selector renders to "" (the resolver treats this as a config error).
    assert_eq!(label_selector_to_string(&LabelSelector::default()), "");
}

// --- inherited_security_context_from_pods: the pure pick/extract core of
// `inheritSecurityContextFrom` (named-vs-first container, prefer-Running, errors). ---

#[cfg(test)]
fn pod_with(
    phase: Option<&str>,
    containers: &[(&str, Option<i64>)], // (name, container runAsUser)
    pod_fs_group: Option<i64>,          // pod-level fsGroup, if any
) -> k8s_openapi::api::core::v1::Pod {
    use k8s_openapi::api::core::v1::{
        Container, Pod, PodSecurityContext, PodSpec, PodStatus, SecurityContext,
    };
    Pod {
        spec: Some(PodSpec {
            containers: containers
                .iter()
                .map(|(name, uid)| Container {
                    name: (*name).to_string(),
                    security_context: uid.map(|u| SecurityContext {
                        run_as_user: Some(u),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
                .collect(),
            security_context: pod_fs_group.map(|g| PodSecurityContext {
                fs_group: Some(g),
                ..Default::default()
            }),
            ..Default::default()
        }),
        status: phase.map(|p| PodStatus {
            phase: Some(p.to_string()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn inherit_picks_named_container_else_first() {
    // Named container wins.
    let pod = pod_with(
        Some("Running"),
        &[("sidecar", Some(101)), ("app", Some(1000))],
        None,
    );
    let pods = std::slice::from_ref(&pod);
    let (csc, _) = inherited_security_context_from_pods(pods, Some("app"), "ns", "app=x")
        .unwrap()
        .contexts;
    assert_eq!(csc.unwrap().run_as_user, Some(1000));
    // No container named → the pod's FIRST container.
    let (csc, _) = inherited_security_context_from_pods(pods, None, "ns", "app=x")
        .unwrap()
        .contexts;
    assert_eq!(csc.unwrap().run_as_user, Some(101));
}

#[test]
fn inherit_copies_both_container_and_pod_security_context() {
    // The workload's CONTAINER runAsUser AND its POD fsGroup are both inherited, so an
    // inheriting mover matches the app at both levels (UID + writable-volume fsGroup).
    let pod = pod_with(Some("Running"), &[("app", Some(1000))], Some(1000));
    let (csc, psc) = inherited_security_context_from_pods(&[pod], Some("app"), "ns", "app=x")
        .unwrap()
        .contexts;
    assert_eq!(csc.unwrap().run_as_user, Some(1000));
    assert_eq!(psc.unwrap().fs_group, Some(1000));

    // A workload with ONLY a pod-level context (no container securityContext) still
    // inherits successfully — the pod context alone is enough.
    let pod = pod_with(Some("Running"), &[("app", None)], Some(2000));
    let (csc, psc) = inherited_security_context_from_pods(&[pod], Some("app"), "ns", "app=x")
        .unwrap()
        .contexts;
    assert!(csc.is_none());
    assert_eq!(psc.unwrap().fs_group, Some(2000));
}

#[test]
fn inherit_reports_the_pod_and_container_it_read() {
    // Provenance is load-bearing: the reconciler reports the mover's identity from WHERE the
    // context came from. Asserting a match from "the inherit branch ran" is the bug this
    // field exists to prevent, so the source must be nameable.
    let mut pod = pod_with(
        Some("Running"),
        &[("sidecar", Some(101)), ("app", Some(1000))],
        None,
    );
    pod.metadata.name = Some("app-7c9d8f5b6-h2k4p".to_string());
    let src = inherited_security_context_from_pods(&[pod], Some("app"), "ns", "app=x").unwrap();
    assert_eq!(src.pod, "app-7c9d8f5b6-h2k4p");
    assert_eq!(src.container.as_deref(), Some("app"));
    assert_eq!(src.uid(), Some(1000));
}

#[test]
fn inherit_from_a_uidless_workload_pins_no_uid() {
    // THE REPORTED BUG. A workload whose securityContext block exists but pins no runAsUser
    // (its UID comes from the image's USER line) inherits "successfully" — the block is
    // non-empty, so the extractor is happy — yet contributes NO uid. The mover then silently
    // runs as its own image's 65532 and fails to read the source with permission denied.
    //
    // This is why `SecurityContextCompatible` may never be asserted from the fact that the
    // pvcConsumer branch ran: here it would claim a UID match "by construction" that does not
    // exist. `uid() == None` is the signal the honest assessment keys on.
    let mut pod = pod_with(Some("Running"), &[("app", None)], Some(65532));
    // A hardened-but-UID-less container context — the bjw-s / restricted-PSA house style.
    pod.spec.as_mut().unwrap().containers[0].security_context =
        Some(k8s_openapi::api::core::v1::SecurityContext {
            allow_privilege_escalation: Some(false),
            ..Default::default()
        });
    let src = inherited_security_context_from_pods(&[pod], Some("app"), "ns", "app=x").unwrap();
    assert!(
        src.contexts.0.is_some(),
        "the container block exists, so the extractor accepts it — that is the trap"
    );
    assert_eq!(
        src.uid(),
        None,
        "inheriting pinned no UID: the mover would run as its own image's 65532"
    );
}

#[test]
fn inherit_uid_follows_kubelet_precedence() {
    // container.runAsUser wins over pod.runAsUser; pod-level is the fallback. Shared with
    // the invariants + compat engines via `effective_run_as_user`, so they cannot fork.
    let mut pod = pod_with(Some("Running"), &[("app", Some(1000))], None);
    pod.spec.as_mut().unwrap().security_context =
        Some(k8s_openapi::api::core::v1::PodSecurityContext {
            run_as_user: Some(2000),
            ..Default::default()
        });
    let src = inherited_security_context_from_pods(&[pod], Some("app"), "ns", "app=x").unwrap();
    assert_eq!(src.uid(), Some(1000), "container-level runAsUser wins");

    // Pod-level only (the very common chart shape) still pins the UID.
    let mut pod = pod_with(Some("Running"), &[("app", None)], None);
    pod.spec.as_mut().unwrap().security_context =
        Some(k8s_openapi::api::core::v1::PodSecurityContext {
            run_as_user: Some(568),
            ..Default::default()
        });
    let src = inherited_security_context_from_pods(&[pod], Some("app"), "ns", "app=x").unwrap();
    assert_eq!(src.uid(), Some(568), "pod-level runAsUser is the fallback");
}

// --- `inherited ⊂ explicit`: the recipe's explicit context is the HIGHER layer. ---
//
// These exercise the same merge helpers `resolve_mover_security_contexts` calls, without a
// kube::Client. The end-to-end ladder (`hardened ⊂ moverDefaults ⊂ inherited ⊂ explicit`) is
// covered in `kopiur-api`'s `resolve_mover` tests.

/// What the resolver does once a workload's contexts are in hand.
#[cfg(test)]
fn merge_explicit_over_inherited(
    inherited: (
        Option<k8s_openapi::api::core::v1::SecurityContext>,
        Option<k8s_openapi::api::core::v1::PodSecurityContext>,
    ),
    explicit: &kopiur_api::common::MoverSpec,
) -> (
    Option<k8s_openapi::api::core::v1::SecurityContext>,
    Option<k8s_openapi::api::core::v1::PodSecurityContext>,
) {
    kopiur_api::common::merge_context_pair(
        inherited.0.as_ref(),
        inherited.1.as_ref(),
        explicit.security_context.as_ref(),
        explicit.pod_security_context.as_ref(),
    )
}

#[test]
fn explicit_pod_level_uid_displaces_an_inherited_container_level_uid() {
    use k8s_openapi::api::core::v1::{PodSecurityContext, SecurityContext};

    // Cross-dimension precedence at the fold itself: the workload pins uid 1000 at the
    // container level, the recipe pins uid 2000 at the POD level. Explicit is the higher
    // layer, so the effective identity must be 2000 — the pair merge promotes it into the
    // container context so the inherited container value cannot shadow it.
    let inherited = (
        Some(SecurityContext {
            run_as_user: Some(1000),
            ..Default::default()
        }),
        None,
    );
    let explicit = kopiur_api::common::MoverSpec {
        pod_security_context: Some(PodSecurityContext {
            run_as_user: Some(2000),
            ..Default::default()
        }),
        ..Default::default()
    };
    let (sc, psc) = merge_explicit_over_inherited(inherited, &explicit);
    assert_eq!(
        kopiur_api::common::effective_run_as_user(sc.as_ref(), psc.as_ref()),
        Some(2000),
        "the explicit pod-level uid is the higher layer and must win"
    );
}

#[test]
fn explicit_security_context_overrides_the_inherited_one() {
    use k8s_openapi::api::core::v1::{PodSecurityContext, SecurityContext};

    // Workload pins uid 1000 + fsGroup 1000; the recipe forces uid 2000 and says nothing
    // about groups. What you WROTE wins; what you left blank is inherited.
    let inherited = (
        Some(SecurityContext {
            run_as_user: Some(1000),
            run_as_group: Some(1000),
            ..Default::default()
        }),
        Some(PodSecurityContext {
            fs_group: Some(1000),
            ..Default::default()
        }),
    );
    let explicit = kopiur_api::common::MoverSpec {
        security_context: Some(SecurityContext {
            run_as_user: Some(2000),
            ..Default::default()
        }),
        ..Default::default()
    };
    let (sc, psc) = merge_explicit_over_inherited(inherited, &explicit);
    assert_eq!(
        sc.as_ref().unwrap().run_as_user,
        Some(2000),
        "the explicit runAsUser must win — it is the higher layer"
    );
    assert_eq!(
        sc.unwrap().run_as_group,
        Some(1000),
        "runAsGroup was not written, so the inherited value fills it"
    );
    assert_eq!(
        psc.unwrap().fs_group,
        Some(1000),
        "the pod context is inherited wholesale when the recipe is silent"
    );
}

#[test]
fn inherited_values_fill_what_the_recipe_leaves_blank() {
    use k8s_openapi::api::core::v1::{PodSecurityContext, SecurityContext};

    // The mirror image: the recipe pins only an fsGroup (e.g. to make a cache writable) and
    // inherits the identity. Neither layer is all-or-nothing.
    let inherited = (
        Some(SecurityContext {
            run_as_user: Some(1000),
            ..Default::default()
        }),
        Some(PodSecurityContext {
            fs_group: Some(1000),
            supplemental_groups: Some(vec![3001]),
            ..Default::default()
        }),
    );
    let explicit = kopiur_api::common::MoverSpec {
        pod_security_context: Some(PodSecurityContext {
            fs_group: Some(2500),
            ..Default::default()
        }),
        ..Default::default()
    };
    let (sc, psc) = merge_explicit_over_inherited(inherited, &explicit);
    assert_eq!(
        sc.unwrap().run_as_user,
        Some(1000),
        "the inherited UID survives — the recipe never mentioned it"
    );
    let psc = psc.unwrap();
    assert_eq!(psc.fs_group, Some(2500), "the explicit fsGroup wins");
    assert_eq!(
        psc.supplemental_groups,
        Some(vec![3001]),
        "an unmentioned pod field keeps its inherited value (NFS shared-group recipe)"
    );
}

#[test]
fn explicit_context_stands_alone_when_nothing_was_inherited() {
    use k8s_openapi::api::core::v1::SecurityContext;

    // The fallback shape: inheritance yielded nothing, so the recipe's context is all there is.
    let explicit = kopiur_api::common::MoverSpec {
        security_context: Some(SecurityContext {
            run_as_user: Some(1000),
            ..Default::default()
        }),
        ..Default::default()
    };
    let (sc, psc) = merge_explicit_over_inherited((None, None), &explicit);
    assert_eq!(sc.unwrap().run_as_user, Some(1000));
    assert!(psc.is_none());
}

#[test]
fn inherit_prefers_a_running_pod() {
    // A Pending replica (uid 5) and a Running one (uid 1000) match — Running wins.
    let pending = pod_with(Some("Pending"), &[("app", Some(5))], None);
    let running = pod_with(Some("Running"), &[("app", Some(1000))], None);
    let (csc, _) =
        inherited_security_context_from_pods(&[pending, running], Some("app"), "ns", "app=x")
            .unwrap()
            .contexts;
    assert_eq!(csc.unwrap().run_as_user, Some(1000));
}

#[test]
fn inherit_errors_are_actionable() {
    // Each message must carry WHAT went wrong, WHERE (so it can be found), and a FIX the user
    // can act on — a bare "no pod matches" leaves someone staring at a held Snapshot.

    // No pod matches.
    let err =
        inherited_security_context_from_pods(&[], Some("app"), "billing", "app=x").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("no pod matches"), "what: {msg}");
    assert!(
        msg.contains("billing") && msg.contains("app=x"),
        "where — the namespace and the selector that found nothing: {msg}"
    );
    assert!(
        msg.contains("Scale it up") && msg.contains("podSelector.matchLabels"),
        "fix — the two things that actually resolve it: {msg}"
    );
    assert!(
        msg.contains("mover.securityContext.runAsUser") && msg.contains("fallback"),
        "fix — and the fallback, which is the difference between a held run and a running \
         one, so the message must not merely offer it as an 'alternative': {msg}"
    );

    // Named container absent.
    let pod = pod_with(Some("Running"), &[("app", Some(1000))], None);
    let err =
        inherited_security_context_from_pods(&[pod], Some("nope"), "billing", "app=x").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("no container `nope`"), "what + where: {msg}");
    assert!(
        msg.contains("inheritSecurityContextFrom.container"),
        "fix — name the field to correct: {msg}"
    );

    // The pod has NEITHER a container nor a pod-level securityContext to inherit.
    let bare = pod_with(Some("Running"), &[("app", None)], None);
    let err =
        inherited_security_context_from_pods(&[bare], Some("app"), "billing", "app=x").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("sets no securityContext"), "what: {msg}");
    assert!(
        msg.contains("comes from its container image"),
        "why — the non-obvious part users hit, and the reason inheriting cannot help: {msg}"
    );
    assert!(
        msg.contains("Set runAsUser on the workload")
            && msg.contains("mover.securityContext.runAsUser"),
        "fix — both remedies: {msg}"
    );
}

#[test]
fn pvc_consumer_error_names_the_claim_and_the_fallback() {
    // The `pvcConsumer` twin of the above: the message must name the claim it looked for (so
    // the user knows WHICH PVC has no consumer) and the fallback that keeps backups running.
    let err = pvc_consumer_security_context_from_pods(&[], "pgdata", "db", None).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("`pgdata`") && msg.contains("`db`"),
        "where — the claim and namespace: {msg}"
    );
    assert!(
        msg.contains("Scale the workload up"),
        "fix — the primary remedy: {msg}"
    );
    assert!(
        msg.contains("mover.securityContext.runAsUser") && msg.contains("fallback"),
        "fix — the fallback that avoids the hold entirely: {msg}"
    );
}

// --- pvc_consumer_security_context_from_pods: discover the workload mounting a
// backup source PVC, EXCLUDING kopiur mover pods, with a deterministic pick. ---

#[cfg(test)]
fn pod_mounting(
    name: &str,
    ns: &str,
    phase: Option<&str>,
    uid: Option<i64>,
    claim: &str,
    kopiur_managed: bool,
) -> k8s_openapi::api::core::v1::Pod {
    use k8s_openapi::api::core::v1::{
        Container, PersistentVolumeClaimVolumeSource, Pod, PodSpec, PodStatus, SecurityContext,
        Volume,
    };
    use kube::core::ObjectMeta;
    let labels = kopiur_managed.then(|| {
        std::collections::BTreeMap::from([(
            kopiur_api::consts::MANAGED_BY_LABEL.to_string(),
            kopiur_api::consts::MANAGED_BY_VALUE.to_string(),
        )])
    });
    Pod {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(ns.to_string()),
            labels,
            ..Default::default()
        },
        spec: Some(PodSpec {
            containers: vec![Container {
                name: "app".to_string(),
                security_context: uid.map(|u| SecurityContext {
                    run_as_user: Some(u),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            volumes: Some(vec![Volume {
                name: "data".to_string(),
                persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                    claim_name: claim.to_string(),
                    read_only: None,
                }),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        status: phase.map(|p| PodStatus {
            phase: Some(p.to_string()),
            ..Default::default()
        }),
    }
}

#[test]
fn pvc_consumer_inherits_from_the_mounting_workload() {
    let workload = pod_mounting("pg-0", "db", Some("Running"), Some(999), "pgdata", false);
    let (csc, _) = pvc_consumer_security_context_from_pods(&[workload], "pgdata", "db", None)
        .unwrap()
        .contexts;
    assert_eq!(csc.unwrap().run_as_user, Some(999));
}

#[test]
fn pvc_consumer_excludes_the_kopiur_mover_pod() {
    // The mover pod ALSO mounts the source PVC; it must never be treated as the consumer
    // (else the mover would inherit from itself). Only the real workload is eligible.
    let mover = pod_mounting(
        "mover-xyz",
        "db",
        Some("Running"),
        Some(65532),
        "pgdata",
        true,
    );
    let workload = pod_mounting("pg-0", "db", Some("Running"), Some(999), "pgdata", false);
    let (csc, _) =
        pvc_consumer_security_context_from_pods(&[mover, workload], "pgdata", "db", None)
            .unwrap()
            .contexts;
    assert_eq!(csc.unwrap().run_as_user, Some(999));

    // With ONLY the mover mounting it (workload scaled to zero) → actionable error, not
    // a self-inherit.
    let mover = pod_mounting(
        "mover-xyz",
        "db",
        Some("Running"),
        Some(65532),
        "pgdata",
        true,
    );
    let err = pvc_consumer_security_context_from_pods(&[mover], "pgdata", "db", None).unwrap_err();
    assert!(
        err.to_string().contains("no running workload pod mounts")
            && err.to_string().contains("pgdata")
    );
}

#[test]
fn pvc_consumer_pick_is_deterministic() {
    // Two Running consumers → the lexicographically smallest (namespace, name) wins,
    // regardless of input order.
    let a = pod_mounting("a-pod", "ns", Some("Running"), Some(1000), "data", false);
    let b = pod_mounting("b-pod", "ns", Some("Running"), Some(2000), "data", false);
    let (csc_fwd, _) =
        pvc_consumer_security_context_from_pods(&[a.clone(), b.clone()], "data", "ns", None)
            .unwrap()
            .contexts;
    let (csc_rev, _) = pvc_consumer_security_context_from_pods(&[b, a], "data", "ns", None)
        .unwrap()
        .contexts;
    assert_eq!(csc_fwd.unwrap().run_as_user, Some(1000));
    assert_eq!(csc_rev.unwrap().run_as_user, Some(1000));
}

#[test]
fn pvc_consumer_prefers_running_over_pending() {
    let pending = pod_mounting("a-pod", "ns", Some("Pending"), Some(5), "data", false);
    let running = pod_mounting("z-pod", "ns", Some("Running"), Some(1000), "data", false);
    let (csc, _) = pvc_consumer_security_context_from_pods(&[pending, running], "data", "ns", None)
        .unwrap()
        .contexts;
    assert_eq!(
        csc.unwrap().run_as_user,
        Some(1000),
        "a Running consumer beats a Pending one even with a larger (ns,name)"
    );
}

// --- bootstrap_outcome: the (result, job state) pair classifies into an
// exhaustive outcome whose success arm OWNS the result — the old code asserted
// the "non-failure implies readable result" invariant with `.expect()`, which a
// future refactor could silently break into a panic. ---

mod bootstrap_outcomes {
    use super::super::events::{
        BootstrapFailure, BootstrapOutcome, MoverJobTerminal, bootstrap_outcome,
    };

    /// A terminal Job failure that is NOT a deadline kill (crash/evict/etc.).
    const JOB_FAILED: MoverJobTerminal = MoverJobTerminal::Failed {
        deadline_exceeded: false,
    };
    use kopiur_kopia::KopiaErrorClass;
    use kopiur_mover::bootstrap::BootstrapResult;
    use kopiur_mover::status::FailureBlock;

    fn ok_result() -> BootstrapResult {
        BootstrapResult {
            success: true,
            created: true,
            unique_id: Some("uid-1".into()),
            snapshot_count: Some(0),
            snapshots: vec![],
            snapshots_truncated: false,
            foreign_suffix_dropped: 0,
            index_blob_count: None,
            epoch: None,
            epoch_error: None,
            blob_retention: None,
            seed: None,
            failure: None,
        }
    }

    #[test]
    fn four_way_mapping() {
        // (None, job succeeded): the result ConfigMap hasn't propagated yet.
        assert!(matches!(
            bootstrap_outcome(None, MoverJobTerminal::Complete, "boot-x", false, 120),
            BootstrapOutcome::ResultPending
        ));

        // (None, job failed): result-less terminal failure, names the Job.
        match bootstrap_outcome(None, JOB_FAILED, "boot-x", false, 120) {
            BootstrapOutcome::Failed(BootstrapFailure::JobFailedWithoutResult { job_name }) => {
                assert_eq!(job_name, "boot-x");
            }
            _ => panic!("expected JobFailedWithoutResult"),
        }

        // (Some unsuccessful, _): backend rejection carrying the mover's class.
        let mut bad = ok_result();
        bad.success = false;
        bad.failure = Some(FailureBlock {
            kopia_error_class: "AuthFailure".into(),
            message: "invalid repository password".into(),
            stderr_tail: None,
            exit_code: Some(1),
            retry_recommended: false,
            op: None,
        });
        match bootstrap_outcome(Some(bad), JOB_FAILED, "boot-x", false, 120) {
            BootstrapOutcome::Failed(BootstrapFailure::Backend { class, message }) => {
                assert_eq!(class, KopiaErrorClass::AuthFailure);
                assert_eq!(message, "invalid repository password");
            }
            _ => panic!("expected Backend failure"),
        }

        // (Some successful, _): the success arm owns the result.
        match bootstrap_outcome(
            Some(ok_result()),
            MoverJobTerminal::Complete,
            "boot-x",
            false,
            120,
        ) {
            BootstrapOutcome::Succeeded(r) => assert_eq!(r.unique_id.as_deref(), Some("uid-1")),
            _ => panic!("expected Succeeded"),
        }
    }

    #[test]
    fn unsuccessful_result_without_a_failure_block_degrades_to_unknown() {
        // A mover that wrote `success: false` but no failure block (a bug or a
        // version skew) must still classify — Unknown, with a generic message —
        // never panic or silently succeed.
        let mut bad = ok_result();
        bad.success = false;
        match bootstrap_outcome(Some(bad), JOB_FAILED, "boot-x", false, 120) {
            BootstrapOutcome::Failed(BootstrapFailure::Backend { class, message }) => {
                assert_eq!(class, KopiaErrorClass::Unknown);
                assert!(message.contains("bootstrap failed"));
            }
            _ => panic!("expected Backend failure"),
        }
    }

    #[test]
    fn not_initialized_sentinel_maps_to_its_own_outcome_not_a_bare_notfound() {
        // The mover's `BootstrapResult::not_initialized()` carries the sentinel
        // class; the controller must lift it to `RepositoryNotInitialized` (checked
        // BEFORE the generic Backend mapping) so the operator sees an actionable
        // "enable create" reason, not a bare kopia NotFound.
        let r = BootstrapResult::not_initialized();
        match bootstrap_outcome(Some(r), JOB_FAILED, "boot-x", false, 120) {
            BootstrapOutcome::Failed(BootstrapFailure::RepositoryNotInitialized) => {}
            _ => panic!("expected RepositoryNotInitialized"),
        }
    }

    #[test]
    fn every_seed_class_maps_to_a_typed_failure_and_routes_by_retryability() {
        use super::super::events::SeedFailure;
        use kopiur_mover::bootstrap as mb;

        // Every sentinel the mover can write is recognised, and the label round
        // trips — a class the controller cannot read would collapse to
        // `Unknown` and lose both its reason and its retry routing.
        for class in [
            mb::SEED_SOURCE_NOT_FOUND_CLASS,
            mb::SEED_SOURCE_EMPTY_CLASS,
            mb::SEED_INCOMPLETE_CLASS,
            mb::SEED_LEFT_EMPTY_CLASS,
        ] {
            let f = SeedFailure::from_class(class).unwrap_or_else(|| panic!("{class} unmapped"));
            // THE cross-crate pin. `reason()` reads `kopiur_api::consts` (so
            // `kopiur_api::gates` can register the same strings without
            // depending on the mover) while `from_class` reads the mover's
            // sentinels. This is the only crate that sees both, so if the two
            // ever drift, a real failure would stop selecting its gate row and
            // doctor would report it as an unknown reason from a newer
            // operator.
            assert_eq!(
                f.reason(),
                class,
                "the api reason must BE the mover's class label"
            );
            assert!(!f.action().is_empty());
            // ...and it must actually select a registry row.
            assert_eq!(
                kopiur_api::gates::STRUCTURAL_GATES
                    .iter()
                    .filter(|g| g.matches(
                        kopiur_api::consts::SEEDED_CONDITION,
                        kopiur_api::gates::CONDITION_FALSE,
                        f.reason()
                    ))
                    .count(),
                1,
                "{class} must select exactly one structural-gate row"
            );
        }
        // Nothing else is a seed failure — not the sibling create sentinel, not
        // a kopia class, not the mover's internal-inconsistency class.
        assert!(SeedFailure::from_class(mb::REPOSITORY_NOT_INITIALIZED_CLASS).is_none());
        assert!(SeedFailure::from_class(mb::BOOTSTRAP_INTERNAL_INCONSISTENCY_CLASS).is_none());
        assert!(SeedFailure::from_class("AuthFailure").is_none());

        // The two SOURCE-side failures point at spec.seed.from; the two
        // INTERRUPTED-copy ones say kopiur resumes it itself.
        assert_eq!(
            SeedFailure::SourceNotFound.action(),
            crate::consts::CHECK_SEED_SOURCE_ACTION
        );
        assert_eq!(
            SeedFailure::LeftEmpty.action(),
            crate::consts::AWAIT_SEED_RESUME_ACTION
        );

        // Routing: ALL FOUR recycle-and-retry, because the relaunch RESUMES the
        // copy (the seed-attempt marker is what makes that legitimate). If the
        // marker or the resume plumbing is ever removed, the last two must go
        // terminal with it — see `BootstrapFailure::recycles_for_retry`.
        for f in [
            SeedFailure::SourceNotFound,
            SeedFailure::SourceEmpty,
            SeedFailure::Incomplete,
            SeedFailure::LeftEmpty,
        ] {
            let bf = BootstrapFailure::Seed {
                failure: f,
                message: "m".into(),
            };
            assert!(bf.recycles_for_retry(), "{f:?} must retry");
            assert_eq!(bf.reason(), f.reason());
            assert_eq!(bf.seed_reason(), Some(f.reason()));
            // Never routed as "the repository is absent" (that path's
            // remediation copy is about a WIPE) and never through the breaker's
            // outage sensor (a seed only fires on a never-bootstrapped repo).
            assert!(!bf.is_repository_absent());
            assert!(!bf.retryable_outage_for_bootstrapped(true));
        }

        // The two TERMINAL ones: an old mover image and a broken mover
        // invariant both reproduce identically on every attempt.
        let skew = BootstrapFailure::SeedMoverTooOld;
        assert!(!skew.recycles_for_retry());
        assert_eq!(skew.reason(), kopiur_api::consts::SEED_MOVER_TOO_OLD_REASON);
        assert_eq!(
            skew.seed_reason(),
            Some(kopiur_api::consts::SEED_MOVER_TOO_OLD_REASON)
        );
        let inconsistent = BootstrapFailure::InternalInconsistency {
            message: "contradiction".into(),
        };
        assert!(!inconsistent.recycles_for_retry());
        assert_eq!(
            inconsistent.reason(),
            mb::BOOTSTRAP_INTERNAL_INCONSISTENCY_CLASS
        );
        // ...and it is NOT a seed statement, so it writes no `Seeded` condition.
        assert_eq!(inconsistent.seed_reason(), None);
        // Neither is an ordinary backend failure.
        assert_eq!(
            BootstrapFailure::JobFailedWithoutResult {
                job_name: "j".into()
            }
            .seed_reason(),
            None
        );
    }

    #[test]
    fn a_seed_armed_success_without_a_seed_outcome_is_refused_as_mover_skew() {
        use kopiur_mover::bootstrap::SeedOutcome;
        use kopiur_mover::workspec::SeedModeSpec;

        // THE GUARD (#380 D4). `BootstrapResult.seed` is mover-authored and
        // every seed-armed success path emits one — the AlreadyInitialized
        // no-op included — so its absence proves the running image dropped the
        // unknown `seed` field, fell into the create fallback, and initialized
        // an EMPTY repository. Accepting it would report Ready over a
        // repository with no history.
        match bootstrap_outcome(
            Some(ok_result()),
            MoverJobTerminal::Complete,
            "boot-x",
            true,
            120,
        ) {
            BootstrapOutcome::Failed(BootstrapFailure::SeedMoverTooOld) => {}
            _ => panic!("a seed-armed success with no seed outcome must be refused"),
        }

        // The SAME result with no seed armed is an ordinary success — the guard
        // must not fire on every bootstrap in the fleet.
        match bootstrap_outcome(
            Some(ok_result()),
            MoverJobTerminal::Complete,
            "boot-x",
            false,
            120,
        ) {
            BootstrapOutcome::Succeeded(_) => {}
            _ => panic!("an unarmed bootstrap is unaffected"),
        }

        // A seed-armed success that DOES acknowledge the seed passes through,
        // for both the real copy and the documented no-op.
        for outcome in [
            SeedOutcome::performed(SeedModeSpec::Blob, "S3".into(), 4, None),
            SeedOutcome::already_initialized(SeedModeSpec::Blob, "S3".into()),
        ] {
            let r = ok_result().with_seed(Some(outcome));
            match bootstrap_outcome(Some(r), MoverJobTerminal::Complete, "boot-x", true, 120) {
                BootstrapOutcome::Succeeded(r) => assert!(r.seed.is_some()),
                _ => panic!("an acknowledged seed must succeed"),
            }
        }

        // The skew message is actionable and volatile-free (so the guarded
        // status write stays a no-op across repeats).
        let msg = BootstrapFailure::SeedMoverTooOld.condition_message();
        assert!(!msg.contains("   "), "wrapped source whitespace: {msg}");
        assert!(msg.contains("upgrade the mover image"), "{msg}");
        assert!(msg.contains("empty"), "{msg}");
    }

    #[test]
    fn a_genuine_kopia_notfound_stays_a_backend_failure() {
        // A real kopia `NotFound` (not the sentinel) must still map to Backend —
        // the sentinel check keys on the exact label, not the NotFound class.
        let mut bad = ok_result();
        bad.success = false;
        bad.failure = Some(FailureBlock {
            kopia_error_class: KopiaErrorClass::NotFound.as_str().into(),
            message: "not found".into(),
            stderr_tail: None,
            exit_code: Some(1),
            retry_recommended: false,
            op: None,
        });
        match bootstrap_outcome(Some(bad), JOB_FAILED, "boot-x", false, 120) {
            BootstrapOutcome::Failed(BootstrapFailure::Backend { class, .. }) => {
                assert_eq!(class, KopiaErrorClass::NotFound);
            }
            _ => panic!("expected Backend NotFound"),
        }
    }
}

// --- mover_job_terminal + the deadline-kill classification (#414) and the
// failure-route table (#415): the Job `Failed` condition's `reason` is finally
// read, so "connect slower than activeDeadlineSeconds" stops masquerading as
// "backend down / bad credentials", and route precedence is decided in one
// tested place instead of two hand-copied `if` chains. ---

mod job_terminal_and_routes {
    use super::super::events::{
        BootstrapFailure, BootstrapOutcome, FailureRoute, MoverJobTerminal, SeedFailure,
        bootstrap_outcome, mover_job_terminal,
    };
    use k8s_openapi::api::batch::v1::{Job, JobCondition, JobStatus};
    use kopiur_kopia::KopiaErrorClass;

    fn job_with_condition(type_: &str, status: &str, reason: Option<&str>) -> Job {
        Job {
            status: Some(JobStatus {
                conditions: Some(vec![JobCondition {
                    type_: type_.into(),
                    status: status.into(),
                    reason: reason.map(Into::into),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn mover_job_terminal_reads_the_failed_conditions_reason() {
        // #414: a deadline kill is typed apart from every other failure mode.
        assert_eq!(
            mover_job_terminal(&job_with_condition(
                "Failed",
                "True",
                Some("DeadlineExceeded")
            )),
            Some(MoverJobTerminal::Failed {
                deadline_exceeded: true
            })
        );
        // Backoff exhaustion (pod-level crashes) is NOT a deadline kill.
        assert_eq!(
            mover_job_terminal(&job_with_condition(
                "Failed",
                "True",
                Some("BackoffLimitExceeded")
            )),
            Some(MoverJobTerminal::Failed {
                deadline_exceeded: false
            })
        );
        // A reason-less Failed condition falls back to the generic bucket.
        assert_eq!(
            mover_job_terminal(&job_with_condition("Failed", "True", None)),
            Some(MoverJobTerminal::Failed {
                deadline_exceeded: false
            })
        );
        assert_eq!(
            mover_job_terminal(&job_with_condition("Complete", "True", None)),
            Some(MoverJobTerminal::Complete)
        );
        // A False condition is not terminal; no status at all is still running.
        assert_eq!(
            mover_job_terminal(&job_with_condition("Failed", "False", None)),
            None
        );
        assert_eq!(mover_job_terminal(&Job::default()), None);
        // Succeeded-count fallback when conditions aren't populated yet.
        let counted = Job {
            status: Some(JobStatus {
                succeeded: Some(1),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            mover_job_terminal(&counted),
            Some(MoverJobTerminal::Complete)
        );
    }

    #[test]
    fn deadline_killed_job_is_its_own_bootstrap_failure() {
        // #414: previously this Job produced `JobFailedWithoutResult` and, on
        // the probe path, the "unreachable, the path/mount is missing, or
        // credentials/lock failed" alert — none of which were true for a
        // backend that was merely slow. The message must carry the what (the
        // deadline kill and the real limit), the why (cold-cache connect
        // scales with index blobs), and the fix (raise the deadline; run
        // maintenance).
        let out = bootstrap_outcome(
            None,
            MoverJobTerminal::Failed {
                deadline_exceeded: true,
            },
            "repo-discovery",
            false,
            300,
        );
        let failure = match out {
            BootstrapOutcome::Failed(f) => f,
            _ => panic!("expected a Failed outcome"),
        };
        assert!(matches!(
            failure,
            BootstrapFailure::JobDeadlineExceeded { ref job_name, deadline_secs: 300 }
                if job_name == "repo-discovery"
        ));
        assert_eq!(
            failure.reason(),
            crate::consts::BOOTSTRAP_DEADLINE_EXCEEDED_REASON
        );
        assert_ne!(failure.reason(), crate::consts::BOOTSTRAP_JOB_FAILED_REASON);
        let msg = failure.condition_message();
        assert!(msg.contains("activeDeadlineSeconds"), "{msg}");
        assert!(msg.contains("(300s)"), "{msg}");
        assert!(
            msg.contains("spec.bootstrap.failurePolicy.activeDeadlineSeconds"),
            "{msg}"
        );
        assert!(msg.contains("maintenance"), "{msg}");
        assert!(msg.contains("progressively longer deadline"), "{msg}");

        // A mover-written result always outranks the Job's infrastructure
        // verdict: a deadline-killed Job that still managed to persist a
        // typed failure keeps that classification.
        let mut with_result = kopiur_mover::bootstrap::BootstrapResult::not_initialized();
        with_result.success = false;
        match bootstrap_outcome(
            Some(with_result),
            MoverJobTerminal::Failed {
                deadline_exceeded: true,
            },
            "repo-discovery",
            false,
            300,
        ) {
            BootstrapOutcome::Failed(BootstrapFailure::RepositoryNotInitialized) => {}
            _ => panic!("a persisted result must outrank the Job's deadline verdict"),
        }
    }

    /// Every `BootstrapFailure` variant × bootstrapped ∈ {true, false} → its
    /// retry route, in the ONE place precedence is decided (#415: the outage
    /// sensor is checked first so it can never be shadowed into the
    /// sensor-less recycle arm; the seed carve-out keeps the documented
    /// flat/prompt DR retry cadence out of the exponential backoff).
    #[test]
    fn failure_route_truth_table() {
        let backend = |class| BootstrapFailure::Backend {
            class,
            message: "m".into(),
        };
        let seed = BootstrapFailure::Seed {
            failure: SeedFailure::SourceNotFound,
            message: "m".into(),
        };
        let no_result = BootstrapFailure::JobFailedWithoutResult {
            job_name: "j".into(),
        };
        let deadline = BootstrapFailure::JobDeadlineExceeded {
            job_name: "j".into(),
            deadline_secs: 120,
        };
        for bootstrapped in [false, true] {
            // A backend outage feeds the sensor only once bootstrapped.
            assert_eq!(
                backend(KopiaErrorClass::RepositoryUnavailable).route(bootstrapped),
                if bootstrapped {
                    FailureRoute::OutageSensor
                } else {
                    FailureRoute::Terminal
                }
            );
            // Non-outage backend verdicts park terminal either way.
            assert_eq!(
                backend(KopiaErrorClass::AuthFailure).route(bootstrapped),
                FailureRoute::Terminal
            );
            // Result-less infrastructure failures recycle (with backoff, #415).
            assert_eq!(no_result.route(bootstrapped), FailureRoute::Recycle);
            // A deadline kill feeds the outage sensor once bootstrapped
            // (#414: streak/backoff/breaker + deadline escalation see it);
            // a never-bootstrapped repo keeps the plain recycle route.
            assert_eq!(
                deadline.route(bootstrapped),
                if bootstrapped {
                    FailureRoute::OutageSensor
                } else {
                    FailureRoute::Recycle
                }
            );
            // Seed failures keep their own PROMPT retry route.
            assert_eq!(seed.route(bootstrapped), FailureRoute::SeedRetry);
            // The rest are terminal verdicts.
            assert_eq!(
                BootstrapFailure::RepositoryNotInitialized.route(bootstrapped),
                FailureRoute::Terminal
            );
            assert_eq!(
                BootstrapFailure::SeedMoverTooOld.route(bootstrapped),
                FailureRoute::Terminal
            );
            assert_eq!(
                BootstrapFailure::InternalInconsistency {
                    message: "m".into()
                }
                .route(bootstrapped),
                FailureRoute::Terminal
            );
        }
    }
}

// --- reconcile_failure_event: every reconcile `Error` variant maps to a
// Warning Event with a stable machine-readable reason, a remediation action,
// and a what/why/fix note. The match is exhaustive (no `_ =>`), so these
// tests pin the full reason/action table — a new Error variant shows up here.

mod reconcile_failure_events {
    use super::super::events::{
        EVENT_NOTE_MAX_BYTES, TRUNCATION_MARKER, event_ref, reconcile_failure_event,
    };
    use crate::consts::{
        APPLY_GRANT_ACTION, BLOCKED_ON_GRANT_REASON, CHECK_API_SERVER_ACTION,
        CHECK_CREDENTIALS_ACTION, CHECK_REFERENCES_ACTION, CHECK_WEBHOOK_CONFIGURATION_ACTION,
        FIX_SCHEDULE_ACTION, FIX_SPEC_ACTION, INVALID_SCHEDULE_REASON, INVALID_SPEC_REASON,
        INVARIANT_VIOLATED_REASON, KUBE_API_ERROR_REASON, MISSING_DEPENDENCY_REASON,
        REPORT_ISSUE_ACTION, SERIALIZATION_FAILED_REASON, WEBHOOK_SETUP_FAILED_REASON,
    };
    use crate::error::Error;
    use kopiur_kopia::{KopiaError, KopiaErrorClass};

    const TEST_UID: u32 = 65532;

    fn kube_error() -> kube::Error {
        kube::Error::Api(
            kube::core::Status::failure(
                "the server is currently unable to handle the request",
                "ServiceUnavailable",
            )
            .boxed(),
        )
    }

    fn serde_error() -> serde_json::Error {
        serde_json::from_str::<serde_json::Value>("{not json").unwrap_err()
    }

    /// The full reason/action table, one row per `Error` variant. Constructing
    /// every variant here means a new variant cannot ship without an explicit
    /// row (mirroring the exhaustive `match` in `reconcile_failure_event`).
    #[test]
    fn every_error_variant_has_a_reason_action_and_actionable_note() {
        let cases: Vec<(Error, &str, &str, &str)> = vec![
            (
                Error::Kube(kube_error()),
                KUBE_API_ERROR_REASON,
                CHECK_API_SERVER_ACTION,
                "retries automatically",
            ),
            (
                Error::Validation("spec.retention.daily must be >= 1".into()),
                INVALID_SPEC_REASON,
                FIX_SPEC_ACTION,
                "fix the field",
            ),
            (
                Error::MissingDependency("Repository apps/nas".into()),
                MISSING_DEPENDENCY_REASON,
                CHECK_REFERENCES_ACTION,
                "create it, or fix the reference",
            ),
            (
                Error::BlockedOnGrant(
                    "namespace `app` has not opted in to privileged movers".into(),
                ),
                BLOCKED_ON_GRANT_REASON,
                APPLY_GRANT_ACTION,
                "reconciles automatically",
            ),
            (
                Error::Serialization(serde_error()),
                SERIALIZATION_FAILED_REASON,
                REPORT_ISSUE_ACTION,
                "report it",
            ),
            (
                Error::InvalidSchedule("bad cron `* *`".into()),
                INVALID_SCHEDULE_REASON,
                FIX_SCHEDULE_ACTION,
                "Fix the cron expression",
            ),
            (
                Error::Invariant("Snapshot has no namespace".into()),
                INVARIANT_VIOLATED_REASON,
                REPORT_ISSUE_ACTION,
                "report it",
            ),
            (
                Error::WebhookSetup("no such webhook configuration".into()),
                WEBHOOK_SETUP_FAILED_REASON,
                CHECK_WEBHOOK_CONFIGURATION_ACTION,
                "Admission stays untrusted",
            ),
            (
                Error::WebhookCert(crate::webhook_tls::CertError::Generate(
                    rcgen::Error::CouldNotParseCertificate,
                )),
                WEBHOOK_SETUP_FAILED_REASON,
                CHECK_WEBHOOK_CONFIGURATION_ACTION,
                "Admission stays untrusted",
            ),
        ];
        for (err, reason, action, note_phrase) in cases {
            let ev = reconcile_failure_event(&err, TEST_UID);
            assert_eq!(ev.reason, reason, "reason for {err}");
            assert_eq!(ev.action, action, "action for {err}");
            assert!(
                ev.note.contains(note_phrase),
                "note for {err} should contain {note_phrase:?}: {}",
                ev.note
            );
            // The note always leads with the error's own message.
            assert!(
                ev.note.contains(&err.to_string()),
                "note for {err} should embed the error message"
            );
        }
    }

    #[test]
    fn kopia_failures_reuse_the_class_reason_and_backend_remediation() {
        // A kopia failure must surface exactly like the bootstrap-failure
        // Events: reason = the kopia class label, note = the per-class
        // remediation (here: credentials hint for AuthFailure).
        let err = Error::Kopia(KopiaError::NonZeroExit {
            args: "repository connect".into(),
            code: Some(1),
            class: KopiaErrorClass::AuthFailure,
            stderr_tail: "invalid repository password".into(),
        });
        let ev = reconcile_failure_event(&err, TEST_UID);
        assert_eq!(ev.reason, KopiaErrorClass::AuthFailure.as_str());
        assert_eq!(ev.action, CHECK_CREDENTIALS_ACTION);
        assert!(ev.note.contains("password was rejected"));
        assert!(ev.note.contains("KOPIA_PASSWORD"));
    }

    #[test]
    fn failure_notes_are_clamped_to_the_event_limit() {
        // An unbounded upstream message (huge kube error / dependency list)
        // must not blow the 1024-byte Event note cap, or the apiserver
        // rejects the Event and the user sees nothing at all.
        let err = Error::MissingDependency("x".repeat(5000));
        let ev = reconcile_failure_event(&err, TEST_UID);
        assert!(ev.note.len() <= EVENT_NOTE_MAX_BYTES);
        assert!(ev.note.contains(TRUNCATION_MARKER));
    }

    #[test]
    fn event_ref_strips_the_resource_version() {
        // The Recorder's dedup-cache key hashes the reference WITHOUT
        // resourceVersion but compares it WITH — a churning rv would mint a
        // new Event object per repeat instead of aggregating series.count.
        let mut m = kopiur_api::Maintenance::new(
            "nas-maintenance",
            kopiur_api::MaintenanceSpec {
                repository: super::ref_of(
                    kopiur_api::common::RepositoryKind::Repository,
                    "nas",
                    None,
                ),
                schedule: kopiur_api::maintenance::default_maintenance_schedule(),
                ownership: kopiur_api::Ownership {
                    owner: "lease".into(),
                    owner_aliases: Vec::new(),
                    takeover_policy: Default::default(),
                },
                mover: None,
                failure_policy: None,
                credential_projection: None,
            },
        );
        m.metadata.namespace = Some("apps".into());
        m.metadata.uid = Some("uid-1234".into());
        m.metadata.resource_version = Some("987654".into());

        let r = event_ref(&m);
        assert_eq!(r.resource_version, None, "resourceVersion must be stripped");
        assert_eq!(r.name.as_deref(), Some("nas-maintenance"));
        assert_eq!(r.namespace.as_deref(), Some("apps"));
        assert_eq!(r.uid.as_deref(), Some("uid-1234"));
    }
}

// --- `InheritSource::pins_identity`: did inheriting actually copy anything usable? ---

#[test]
fn pins_identity_is_true_for_a_uid() {
    let pod = pod_with(Some("Running"), &[("app", Some(1000))], None);
    let src = inherited_security_context_from_pods(&[pod], Some("app"), "ns", "app=x").unwrap();
    assert!(src.pins_identity());
}

#[test]
fn pins_identity_is_true_for_groups_alone() {
    use k8s_openapi::api::core::v1::{PodSecurityContext, SecurityContext};

    // A workload pinning ONLY runAsGroup (UID from its image) still lets the mover read
    // 0640 group-readable data through the group bit. Treating "no UID" as "inherit did
    // nothing" would warn — falsely — about a setup that works.
    let mut pod = pod_with(Some("Running"), &[("app", None)], None);
    pod.spec.as_mut().unwrap().containers[0].security_context = Some(SecurityContext {
        run_as_group: Some(1000),
        ..Default::default()
    });
    let src = inherited_security_context_from_pods(&[pod], Some("app"), "ns", "app=x").unwrap();
    assert_eq!(src.uid(), None, "no UID was pinned");
    assert!(
        src.pins_identity(),
        "but a group WAS — inheriting was not a no-op"
    );

    // Same for an fsGroup-only workload (the blessed restore shape).
    let mut pod = pod_with(Some("Running"), &[("app", None)], Some(2500));
    pod.spec.as_mut().unwrap().containers[0].security_context = Some(SecurityContext::default());
    let src = inherited_security_context_from_pods(&[pod], Some("app"), "ns", "app=x").unwrap();
    assert!(
        src.pins_identity(),
        "fsGroup is an identity worth inheriting"
    );

    // …and supplementalGroups (the NFS shared-group recipe).
    let mut pod = pod_with(Some("Running"), &[("app", None)], None);
    pod.spec.as_mut().unwrap().containers[0].security_context = Some(SecurityContext::default());
    pod.spec.as_mut().unwrap().security_context = Some(PodSecurityContext {
        supplemental_groups: Some(vec![3001]),
        ..Default::default()
    });
    let src = inherited_security_context_from_pods(&[pod], Some("app"), "ns", "app=x").unwrap();
    assert!(src.pins_identity());
}

#[test]
fn pins_identity_is_false_when_the_workload_pins_nothing() {
    use k8s_openapi::api::core::v1::SecurityContext;

    // THE REPORTED SHAPE: a hardened block that pins no identity at all. Inheriting from
    // this is a provable no-op — the mover falls back to its own image's 65532.
    let mut pod = pod_with(Some("Running"), &[("app", None)], None);
    pod.spec.as_mut().unwrap().containers[0].security_context = Some(SecurityContext {
        allow_privilege_escalation: Some(false),
        read_only_root_filesystem: Some(true),
        ..Default::default()
    });
    let src = inherited_security_context_from_pods(&[pod], Some("app"), "ns", "app=x").unwrap();
    assert_eq!(src.uid(), None);
    assert!(
        !src.pins_identity(),
        "no UID and no group: inheriting copied nothing the mover can act on"
    );
}

// --- §3's fallback predicate: an explicit context is a fallback only if it pins an identity.
//
// `resolve_mover_security_contexts` needs a kube::Client, so these exercise the predicate it
// keys on directly. Keying on "a context field exists" instead would make the fallback and the
// pins-nothing warning fire for the SAME run — the reconciler would report "proceeding with
// your explicit context" and "nothing was pinned" together, which is incoherent.

#[test]
fn a_context_pinning_a_uid_is_a_usable_fallback() {
    use k8s_openapi::api::core::v1::SecurityContext;
    let sc = SecurityContext {
        run_as_user: Some(1000),
        ..Default::default()
    };
    assert!(kopiur_api::common::effective_run_as_user(Some(&sc), None).is_some());
}

#[test]
fn a_context_pinning_no_identity_is_not_a_fallback() {
    use k8s_openapi::api::core::v1::{PodSecurityContext, SecurityContext};

    // Newly legal alongside inherit once the exclusion is lifted: a recipe that sets only
    // seccomp/caps. It cannot stand in for a workload's identity, so a failed inherit must
    // still hold the run rather than silently proceed at the mover image's UID.
    let sc = SecurityContext {
        allow_privilege_escalation: Some(false),
        ..Default::default()
    };
    assert!(
        kopiur_api::common::effective_run_as_user(Some(&sc), None).is_none(),
        "seccomp/caps pin no identity — not a fallback"
    );

    // An fsGroup-only pod context likewise pins no UID for the *backup* fallback decision.
    let psc = PodSecurityContext {
        fs_group: Some(2500),
        ..Default::default()
    };
    assert!(kopiur_api::common::effective_run_as_user(None, Some(&psc)).is_none());

    // Pod-level runAsUser DOES count (kubelet precedence).
    let psc = PodSecurityContext {
        run_as_user: Some(568),
        ..Default::default()
    };
    assert_eq!(
        kopiur_api::common::effective_run_as_user(None, Some(&psc)),
        Some(568)
    );
}

#[test]
fn pvc_consumer_inherits_the_container_that_mounts_the_claim_not_the_first() {
    use k8s_openapi::api::core::v1::{
        Container, PersistentVolumeClaimVolumeSource, Pod, PodSpec, PodStatus, SecurityContext,
        Volume, VolumeMount,
    };

    // A sidecar-injected pod: istio-proxy (uid 1337) is listed FIRST and mounts nothing of
    // ours; the app (uid 1000) mounts the source claim and is what actually wrote the data.
    // Inheriting the sidecar yields a mover that cannot read the app's files — and, before the
    // condition was made honest, one that claimed it could.
    let pod = Pod {
        metadata: kube::core::ObjectMeta {
            name: Some("app-7c9d8f5b6-h2k4p".into()),
            ..Default::default()
        },
        spec: Some(PodSpec {
            containers: vec![
                Container {
                    name: "istio-proxy".into(),
                    security_context: Some(SecurityContext {
                        run_as_user: Some(1337),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                Container {
                    name: "app".into(),
                    security_context: Some(SecurityContext {
                        run_as_user: Some(1000),
                        ..Default::default()
                    }),
                    volume_mounts: Some(vec![VolumeMount {
                        name: "data".into(),
                        mount_path: "/data".into(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                },
            ],
            volumes: Some(vec![Volume {
                name: "data".into(),
                persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                    claim_name: "app-data".into(),
                    read_only: None,
                }),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        status: Some(PodStatus {
            phase: Some("Running".into()),
            ..Default::default()
        }),
    };

    let src = pvc_consumer_security_context_from_pods(&[pod], "app-data", "app", None).unwrap();
    assert_eq!(
        src.container.as_deref(),
        Some("app"),
        "must pick the container that mounts the claim, not the injected first one"
    );
    assert_eq!(src.uid(), Some(1000), "and inherit the app's UID, not 1337");
}

/// Two condition writers in one reconcile must not erase each other.
///
/// A `status.conditions` patch REPLACES the whole array. `assess_backup_security_context` and
/// `report_inherit_outcome` both run in a single Snapshot reconcile, and both build their array
/// with `upsert_condition`. If the second builds from the object as it looked at the START of
/// the reconcile, it drops whatever the first just wrote — silently.
///
/// This is not hypothetical: it is exactly what made the e2e regression guard PASS against a
/// deliberately-reintroduced `SecurityContextCompatible=True` bug. The `True` was written, then
/// wiped by the InheritPinnedNoUid write moments later, so the guard saw no `True` and was
/// green for the wrong reason. The fix is that the second writer re-reads the live object; this
/// test pins the property that makes that necessary.
#[test]
fn upsert_from_stale_conditions_drops_a_concurrent_write() {
    use crate::io::upsert_condition;

    // Reconcile starts: no conditions.
    let at_reconcile_start: Vec<k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition> = vec![];

    // Writer 1 (assess) patches SecurityContextCompatible=True.
    let after_first = upsert_condition(
        &at_reconcile_start,
        "SecurityContextCompatible",
        true,
        "SecurityContextCompatible",
        "the mover's uid matches",
        Some(1),
    );
    assert_eq!(after_first.len(), 1);

    // Writer 2 building from the STALE snapshot — the bug.
    let stale = upsert_condition(
        &at_reconcile_start,
        "SecurityContextInherited",
        false,
        "InheritOverridden",
        "explicit uid displaced the inherited one",
        Some(1),
    );
    assert!(
        !stale.iter().any(|c| c.type_ == "SecurityContextCompatible"),
        "building from the reconcile-start copy DROPS the first writer's condition — this is \
         why the second writer must re-read the live object"
    );

    // Writer 2 building from what is actually on the object — correct.
    let fresh = upsert_condition(
        &after_first,
        "SecurityContextInherited",
        false,
        "InheritOverridden",
        "explicit uid displaced the inherited one",
        Some(1),
    );
    assert_eq!(fresh.len(), 2, "both conditions must survive");
    assert!(
        fresh
            .iter()
            .any(|c| c.type_ == "SecurityContextCompatible" && c.status == "True")
    );
    assert!(
        fresh
            .iter()
            .any(|c| c.type_ == "SecurityContextInherited" && c.status == "False")
    );
}

#[test]
fn pins_identity_is_false_for_a_group_the_hardened_default_already_gives() {
    use k8s_openapi::api::core::v1::{PodSecurityContext, SecurityContext};

    // A workload pinning nothing but `fsGroup: 65532` — the exact value
    // `hardened_pod_security_context()` already supplies. Inheriting it produces a mover
    // byte-identical to the no-inherit default (uid 65532 from its own image, fsGroup 65532
    // from the hardened base), so inheritance achieved nothing and the user must be told.
    // Testing "are there any groups" instead of "any groups BEYOND the baseline" would call
    // this a contribution and stay silent.
    let mut pod = pod_with(Some("Running"), &[("app", None)], None);
    pod.spec.as_mut().unwrap().containers[0].security_context = Some(SecurityContext::default());
    pod.spec.as_mut().unwrap().security_context = Some(PodSecurityContext {
        fs_group: Some(kopiur_api::common::MOVER_NONROOT_ID),
        ..Default::default()
    });
    let src = inherited_security_context_from_pods(&[pod], Some("app"), "ns", "app=x").unwrap();
    assert_eq!(src.uid(), None);
    assert!(
        !src.pins_identity(),
        "fsGroup 65532 is what the hardened default already gives — inheriting it contributed \
         nothing, so this must still warn"
    );

    // One above the baseline IS a contribution.
    let mut pod = pod_with(Some("Running"), &[("app", None)], None);
    pod.spec.as_mut().unwrap().containers[0].security_context = Some(SecurityContext::default());
    pod.spec.as_mut().unwrap().security_context = Some(PodSecurityContext {
        fs_group: Some(1000),
        ..Default::default()
    });
    let src = inherited_security_context_from_pods(&[pod], Some("app"), "ns", "app=x").unwrap();
    assert!(
        src.pins_identity(),
        "fsGroup 1000 is not the default — inheriting it changed the mover"
    );
}

// --- bounded failure-Event publishing (apiserver-outage EMFILE fix): the
// error-policy publish used to be an UNBOUNDED fire-and-forget spawn per failed
// reconcile. During an outage every reconcile fails at once, so the spawns —
// each opening a socket to the dead apiserver — were one of the fd-exhaustion
// amplifiers. The helper bounds in-flight COUNT (permits; drop, never queue)
// and per-publish HOLD TIME (timeout), because a half-alive apiserver answers
// 429/500/503 (which still publish) — the permit bound is load-bearing even
// with the transport-error suppression in place. ---
mod bounded_failure_publish {
    use super::*;

    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use http::{Request, Response, StatusCode};
    use kube::Client;
    use kube::client::Body;
    use kube::runtime::events::{Recorder, Reporter};
    use tokio::sync::Semaphore;

    use crate::error::Error;
    use crate::metrics::Metrics;

    /// A client that records request methods and answers POSTs by echoing the
    /// body (a created Event parses as itself).
    fn counting_client(log: Arc<Mutex<Vec<String>>>) -> Client {
        let svc = tower::service_fn(move |req: Request<Body>| {
            let log = log.clone();
            async move {
                let method = req.method().as_str().to_string();
                let bytes = http_body_util::BodyExt::collect(req.into_body())
                    .await
                    .expect("collect request body")
                    .to_bytes();
                log.lock().unwrap().push(method.clone());
                let (status, body) = if method == "POST" {
                    (StatusCode::CREATED, bytes.to_vec())
                } else {
                    (StatusCode::OK, b"{}".to_vec())
                };
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
            }
        });
        Client::new(svc, "test-ns")
    }

    /// A client whose responses never arrive — a stalled/half-dead apiserver.
    fn hanging_client() -> Client {
        let svc = tower::service_fn(move |_req: Request<Body>| async move {
            std::future::pending::<()>().await;
            unreachable!("the hanging mock never responds");
            #[allow(unreachable_code)]
            Ok::<Response<Body>, std::convert::Infallible>(
                Response::builder().body(Body::empty()).unwrap(),
            )
        });
        Client::new(svc, "test-ns")
    }

    fn leaked_semaphore(permits: usize) -> &'static Semaphore {
        Box::leak(Box::new(Semaphore::new(permits)))
    }

    fn some_failure() -> (
        k8s_openapi::api::core::v1::ObjectReference,
        events::FailureEvent,
    ) {
        let obj = k8s_openapi::api::core::v1::ConfigMap {
            metadata: kube::core::ObjectMeta {
                name: Some("victim".into()),
                namespace: Some("test-ns".into()),
                uid: Some("00000000-0000-0000-0000-000000000000".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        (
            events::event_ref(&obj),
            events::reconcile_failure_event(
                &Error::MissingDependency("repo `x` not found".into()),
                TEST_UID,
            ),
        )
    }

    #[tokio::test]
    async fn dropped_when_permits_are_saturated() {
        // A private leaked semaphore, NOT the global: saturation is set up
        // deterministically with zero timing dependence.
        let sem = leaked_semaphore(1);
        let _held = sem.try_acquire().expect("free permit");
        let log = Arc::new(Mutex::new(Vec::new()));
        let recorder = Recorder::new(counting_client(log.clone()), Reporter::from("kopiur-test"));
        let (regarding, event) = some_failure();
        let spawned =
            events::try_spawn_failure_publish_with(sem, Metrics::new(), recorder, regarding, event);
        assert!(!spawned, "a saturated permit pool must DROP, never queue");
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(
            log.lock().unwrap().is_empty(),
            "no request may be issued for a dropped publish"
        );
    }

    #[tokio::test]
    async fn spawned_publish_posts_and_releases_its_permit() {
        let sem = leaked_semaphore(2);
        let log = Arc::new(Mutex::new(Vec::new()));
        let recorder = Recorder::new(counting_client(log.clone()), Reporter::from("kopiur-test"));
        let (regarding, event) = some_failure();
        let spawned =
            events::try_spawn_failure_publish_with(sem, Metrics::new(), recorder, regarding, event);
        assert!(spawned);
        for _ in 0..64 {
            if sem.available_permits() == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sem.available_permits(),
            2,
            "the permit must return after the POST"
        );
        assert_eq!(log.lock().unwrap().as_slice(), ["POST"]);
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_publish_times_out_and_releases_its_permit() {
        let sem = leaked_semaphore(2);
        let recorder = Recorder::new(hanging_client(), Reporter::from("kopiur-test"));
        let (regarding, event) = some_failure();
        let spawned =
            events::try_spawn_failure_publish_with(sem, Metrics::new(), recorder, regarding, event);
        assert!(spawned);
        // Let the task start and hit the never-responding IO: the permit is held.
        tokio::task::yield_now().await;
        assert_eq!(sem.available_permits(), 1);
        // Past the deadline the timeout must reap the stalled publish — without
        // it, a permit (and its socket) would be pinned until the write timeout
        // (295s) or forever, silently re-shrinking the pool to zero.
        tokio::time::advance(crate::config::FAILURE_EVENT_PUBLISH_TIMEOUT + Duration::from_secs(1))
            .await;
        for _ in 0..16 {
            if sem.available_permits() == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sem.available_permits(),
            2,
            "a stalled publish must release its permit at the deadline"
        );
    }
}

// --- `inheritSecurityContextFrom.snapshot`: the recorded identity IS the inherited layer ---

mod resolved_from_recorded_meta {
    use k8s_openapi::api::core::v1::{PodSecurityContext, SecurityContext};
    use kopiur_api::common::MoverSpec;
    use kopiur_api::recorded::{KOPIUR_META_SCHEMA_V1, RecordedSnapshotMeta, RecordedSrc};

    use crate::io::{InheritOutcome, resolved_from_recorded};

    fn meta(
        uid: Option<i64>,
        gid: Option<i64>,
        fs: Option<i64>,
        src: RecordedSrc,
    ) -> RecordedSnapshotMeta {
        RecordedSnapshotMeta {
            schema: KOPIUR_META_SCHEMA_V1,
            src,
            uid,
            gid,
            fs_group: fs,
        }
    }

    #[test]
    fn recorded_identity_synthesizes_the_inherited_layer_and_outcome() {
        let m = MoverSpec::default();
        let r = resolved_from_recorded(
            &m,
            "app/pg-b1",
            &meta(Some(3001), Some(3001), Some(2000), RecordedSrc::Inherited),
        );
        let (sc, psc) = &r.contexts;
        assert_eq!(
            kopiur_api::common::effective_run_as_user(sc.as_ref(), psc.as_ref()),
            Some(3001)
        );
        assert_eq!(
            kopiur_api::common::effective_run_as_group(sc.as_ref(), psc.as_ref()),
            Some(3001)
        );
        assert_eq!(psc.as_ref().and_then(|p| p.fs_group), Some(2000));
        assert_eq!(
            r.outcome,
            InheritOutcome::InheritedFromSnapshot {
                snapshot: "app/pg-b1".into(),
                uid: Some(3001),
                src: RecordedSrc::Inherited,
            }
        );
        assert!(r.unfiltered_pods.is_none(), "no pod list on this path");
    }

    #[test]
    fn explicit_context_is_the_higher_layer_over_the_recorded_one() {
        // Exactly like live-pod inherit: what the recipe writes wins, the record
        // fills in the rest — including cross-dimension (explicit POD-level uid
        // beats the recorded container-level one via the pair-merge promotion).
        let m = MoverSpec {
            pod_security_context: Some(PodSecurityContext {
                run_as_user: Some(2000),
                ..Default::default()
            }),
            ..Default::default()
        };
        let r = resolved_from_recorded(
            &m,
            "app/pg-b1",
            &meta(Some(3001), Some(3001), Some(3001), RecordedSrc::Explicit),
        );
        let (sc, psc) = &r.contexts;
        assert_eq!(
            kopiur_api::common::effective_run_as_user(sc.as_ref(), psc.as_ref()),
            Some(2000),
            "the explicit uid is the higher layer"
        );
        // Fields the recipe leaves blank still come from the record.
        assert_eq!(
            kopiur_api::common::effective_run_as_group(sc.as_ref(), psc.as_ref()),
            Some(3001)
        );
        assert_eq!(psc.as_ref().and_then(|p| p.fs_group), Some(3001));
        // The outcome still names the RECORDED uid (the layer's own contribution),
        // so reporting can detect a displaced record.
        assert!(matches!(
            r.outcome,
            InheritOutcome::InheritedFromSnapshot {
                uid: Some(3001),
                ..
            }
        ));
    }

    #[test]
    fn recorded_root_uid_lands_in_the_contexts_for_the_privileged_gate() {
        // A forged/legit uid-0 record must flow into the resolved contexts so the
        // downstream `requires_privilege_resolved` gate sees it — identical to an
        // inherited-from-a-live-root-pod context.
        let r = resolved_from_recorded(
            &MoverSpec::default(),
            "app/pg-b1",
            &meta(Some(0), None, None, RecordedSrc::Explicit),
        );
        let (sc, psc) = &r.contexts;
        assert_eq!(
            kopiur_api::common::effective_run_as_user(sc.as_ref(), psc.as_ref()),
            Some(0)
        );
        assert!(kopiur_api::common::requires_privilege_resolved(
            sc.as_ref(),
            psc.as_ref(),
            None
        ));
    }

    #[test]
    fn empty_record_contributes_nothing_and_invents_no_identity() {
        // uid/gid/fsGroup all absent: the synthesized layer must not conjure an
        // identity out of thin air (promotion never invents one).
        let r = resolved_from_recorded(
            &MoverSpec::default(),
            "app/pg-b1",
            &meta(None, None, None, RecordedSrc::Defaults),
        );
        let (sc, psc) = &r.contexts;
        assert_eq!(
            kopiur_api::common::effective_run_as_user(sc.as_ref(), psc.as_ref()),
            None
        );
        assert_eq!(psc.as_ref().and_then(|p| p.fs_group), None);
        assert!(matches!(
            r.outcome,
            InheritOutcome::InheritedFromSnapshot { uid: None, .. }
        ));
    }

    // The full SecurityContext type carries no Eq; comparing the whole
    // ResolvedMoverSecurity works because it derives PartialEq.
    #[test]
    fn contexts_match_a_hand_built_pair_merge() {
        let m = MoverSpec {
            security_context: Some(SecurityContext {
                run_as_non_root: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let rec = meta(Some(3001), None, Some(65532), RecordedSrc::Inherited);
        let r = resolved_from_recorded(&m, "app/pg-b1", &rec);
        let expected = kopiur_api::common::merge_context_pair(
            Some(&SecurityContext {
                run_as_user: Some(3001),
                ..Default::default()
            }),
            Some(&PodSecurityContext {
                fs_group: Some(65532),
                ..Default::default()
            }),
            m.security_context.as_ref(),
            m.pod_security_context.as_ref(),
        );
        assert_eq!(r.contexts, expected);
    }
}

// --- gate ↔ writer drift guard ----------------------------------------------

/// Every row of the shared structural-gate registry, paired with the reconciler
/// site that actually stamps it.
///
/// The registry is the contract `kubectl kopiur doctor` reads (#359). It is
/// data, so nothing stops the two halves drifting apart in either direction: a
/// row nobody writes (doctor watches for a condition that can never appear), or
/// a writer nobody registered (the CLI is blind to a real gate — the original
/// bug). This table is the pinned second opinion. Adding a gate means touching
/// the registry, the writer, AND this table.
///
/// Tuple: (condition `type`, blocked status as the `bool` `upsert_condition`
/// takes, `reason`, the writer). Rows marked `upsert_gate` are written FROM the
/// registry const and are re-derived below; the two dynamic writers upsert both
/// polarities from computed state, so they name the test that pins them.
const GATE_WRITERS: &[(&str, bool, &str, &str)] = &[
    // `snapshot::reconcile_inner` + `restore::run_restore_mover`, both via
    // io::upsert_gate(&PRIVILEGED_MOVER_GATE, …).
    (
        crate::consts::MOVER_PERMITTED_CONDITION,
        false,
        crate::consts::PRIVILEGED_MOVER_NOT_PERMITTED_REASON,
        "snapshot::reconcile_inner + restore::run_restore_mover (upsert_gate)",
    ),
    // The `Error::MissingDependency` credential arm in `snapshot::reconcile_inner`
    // and `restore::run_restore_mover`, via
    // io::upsert_gate(&MISSING_CREDENTIALS_GATE, …).
    (
        crate::consts::CREDENTIALS_AVAILABLE_CONDITION,
        false,
        crate::consts::MISSING_CREDENTIALS_REASON,
        "snapshot::reconcile_inner + restore::run_restore_mover creds arm (upsert_gate)",
    ),
    // The `Error::MissingDependency` arm around `io::ensure_mover_identity` in
    // both reconcilers, via io::upsert_gate(&MISSING_SERVICE_ACCOUNT_GATE, …).
    (
        crate::consts::CREDENTIALS_AVAILABLE_CONDITION,
        false,
        crate::consts::MISSING_SERVICE_ACCOUNT_REASON,
        "snapshot::reconcile_inner + restore::run_restore_mover SA arm (upsert_gate)",
    ),
    // The `Error::MissingCaBundle` arm around repo resolution in both
    // reconcilers (snapshot::reconcile_inner's resolve_recipe +
    // restore::run_restore_mover's resolve_restore_repository), via
    // io::upsert_gate(&MISSING_CA_BUNDLE_GATE, …).
    (
        crate::consts::CREDENTIALS_AVAILABLE_CONDITION,
        false,
        crate::consts::MISSING_CA_BUNDLE_REASON,
        "snapshot::reconcile_inner + restore::run_restore_mover CA-bundle arm (upsert_gate)",
    ),
    // `snapshot::hold_deletion`, via io::upsert_gate(&DELETION_HELD_GATE, …).
    (
        crate::consts::DELETION_HELD_CONDITION,
        true,
        crate::consts::MASS_DELETION_BREAKER_REASON,
        "snapshot::hold_deletion (upsert_gate)",
    ),
    // `snapshot::plan::repo_mass_deletion_condition` (held arm) folded into the
    // repository status write. Both polarities are computed, so it keeps its own
    // `upsert_condition`; its reason is pinned by
    // `snapshot::tests::repo_mass_deletion_condition_held_at_or_above_threshold`.
    (
        crate::consts::MASS_DELETION_HELD_CONDITION,
        true,
        crate::consts::MASS_DELETION_THRESHOLD_EXCEEDED_REASON,
        "snapshot::plan::repo_mass_deletion_condition (computed polarity)",
    ),
    // `snapshot::reconcile_inner`'s ReadOnly refusal, via
    // io::upsert_gate(&REPOSITORY_READ_ONLY_GATE, …).
    (
        crate::consts::REPOSITORY_WRITABLE_CONDITION,
        false,
        crate::consts::REPOSITORY_READ_ONLY_REASON,
        "snapshot::reconcile_inner ReadOnly refusal (upsert_gate)",
    ),
    // `snapshot_schedule::schedule_ready_status`, which upserts the runnable
    // gate either way so it CLEARS; pinned by
    // `snapshot_schedule::tests::the_runnable_gate_is_set_and_cleared_from_the_same_fact`.
    (
        crate::consts::SCHEDULE_RUNNABLE_CONDITION,
        false,
        crate::consts::BLOCKED_ON_UNREADABLE_RUN_REASON,
        "snapshot_schedule::schedule_ready_status (computed polarity)",
    ),
    // `snapshot_schedule::schedule_ready_status` (fire passes), which asserts
    // BOTH polarities so a slot that mints fully clears the gate. Promoted into
    // the registry by the #368 M10 gates/doctor checklist.
    (
        crate::consts::SCHEDULE_FANOUT_CAPPED_CONDITION,
        true,
        crate::consts::FANOUT_TOO_LARGE_REASON,
        "snapshot_schedule::schedule_ready_status fan-out cap (computed polarity)",
    ),
    // `snapshot_policy::policy_ready_conditions`, via
    // io::upsert_gate(&POLICY_REPOSITORY_NOT_READY_GATE, …) on the not-ready
    // side; the all-ready side clears it (True) when present.
    (
        crate::consts::REPOSITORIES_READY_CONDITION,
        false,
        crate::consts::REPOSITORY_NOT_READY_REASON,
        "snapshot_policy::policy_ready_conditions (upsert_gate)",
    ),
    // `snapshot::handle_missing_source_pvc`, which folds the gate into the
    // park/Failed status write (`snapshot_ready_status_with_condition` carries
    // the row's exact type/polarity/reason); the successful-resolution path
    // clears it (True) only when the condition already exists. Pinned by
    // `snapshot::tests::missing_source_pvc_status_write_matches_the_gate_row`.
    (
        crate::consts::SOURCE_PVC_AVAILABLE_CONDITION,
        false,
        crate::consts::SOURCE_PVC_MISSING_REASON,
        "snapshot::handle_missing_source_pvc (computed polarity)",
    ),
    // `restore::park_on_missing_referent` via
    // io::upsert_gate(&RESTORE_REFERENT_MISSING_GATE, ...) — the tri-state
    // readiness gate's `Undetermined` arm (#393); cleared (True, reason
    // `RestoreReferentFound`) by `restore::proceed_past_gate` /
    // `plan::cleared_referent_conditions` once the referent exists.
    (
        crate::consts::RESTORE_REFERENT_AVAILABLE_CONDITION,
        false,
        crate::consts::RESTORE_REFERENT_MISSING_REASON,
        "restore::park_on_missing_referent",
    ),
    // `repository::park_on_seed_source` +
    // `cluster_repository::park_cluster_on_seed_source`, both via
    // io::upsert_gate(&SEED_SOURCE_NOT_READY_GATE, ...); cleared by
    // finalize_bootstrap's `Seeded=True` fold once the seed runs.
    (
        kopiur_api::consts::SEEDED_CONDITION,
        false,
        kopiur_api::consts::WAITING_FOR_SEED_SOURCE_REASON,
        "repository::park_on_seed_source + cluster_repository::park_cluster_on_seed_source \
         (upsert_gate)",
    ),
    // The SAME two park writers, reached with the OTHER park gate: a
    // migrate-mode seed whose local and source backends disagree on workload
    // identity (`repo_seed::arm_migrate_seed`'s `validate_replication_auth`
    // arm). The gate row rides `SeedArming::Park`, so the reason is chosen
    // where the problem is diagnosed, not at the writer.
    (
        kopiur_api::consts::SEEDED_CONDITION,
        false,
        kopiur_api::consts::SEED_SOURCE_AUTH_CONFLICT_REASON,
        "repository::park_on_seed_source + cluster_repository::park_cluster_on_seed_source, \
         armed by repo_seed::arm_migrate_seed (upsert_gate)",
    ),
    // `repository::write_seeding_condition` +
    // `cluster_repository::write_cluster_seeding_condition`, both via
    // io::upsert_gate(&SEEDING_GATE, ...) while the seeding Job is in flight;
    // cleared by the same `Seeded=True` fold.
    (
        kopiur_api::consts::SEEDED_CONDITION,
        false,
        kopiur_api::consts::SEEDING_REASON,
        "repository::write_seeding_condition + \
         cluster_repository::write_cluster_seeding_condition (upsert_gate)",
    ),
    // The five `Seeded=False` FAILURE reasons, all folded by
    // `repository::finalize_bootstrap_failure` +
    // `cluster_repository::finalize_cluster_bootstrap_failure` from
    // `BootstrapFailure::seed_reason()`. One writer, five rows: the reason is
    // computed from the typed failure, so the fold keeps its own
    // `upsert_condition` rather than naming a row.
    (
        kopiur_api::consts::SEEDED_CONDITION,
        false,
        kopiur_api::consts::SEED_SOURCE_NOT_FOUND_REASON,
        "repository::finalize_bootstrap_failure seed fold (computed reason)",
    ),
    (
        kopiur_api::consts::SEEDED_CONDITION,
        false,
        kopiur_api::consts::SEED_SOURCE_EMPTY_REASON,
        "repository::finalize_bootstrap_failure seed fold (computed reason)",
    ),
    (
        kopiur_api::consts::SEEDED_CONDITION,
        false,
        kopiur_api::consts::SEED_INCOMPLETE_REASON,
        "repository::finalize_bootstrap_failure seed fold (computed reason)",
    ),
    (
        kopiur_api::consts::SEEDED_CONDITION,
        false,
        kopiur_api::consts::SEED_LEFT_EMPTY_REASON,
        "repository::finalize_bootstrap_failure seed fold (computed reason)",
    ),
    (
        kopiur_api::consts::SEEDED_CONDITION,
        false,
        kopiur_api::consts::SEED_MOVER_TOO_OLD_REASON,
        "repository::finalize_bootstrap_failure seed fold, mover-skew guard \
         (computed reason)",
    ),
];

#[test]
fn every_registered_gate_has_a_writer() {
    use kopiur_api::gates::STRUCTURAL_GATES;
    // Registry → writer: a row doctor watches for that nothing ever stamps.
    for gate in STRUCTURAL_GATES {
        assert!(
            GATE_WRITERS.iter().any(|(condition, blocked, reason, _)| {
                *condition == gate.condition
                    && *blocked == gate.blocked_is_true()
                    && *reason == gate.reason
            }),
            "{gate:?} is registered but no writer is pinned for it — either a reconciler \
             stamps it (add it to GATE_WRITERS) or nothing does (drop the row)"
        );
    }
    // Writer → registry: a gate a reconciler stamps that doctor cannot see.
    for (condition, blocked, reason, writer) in GATE_WRITERS {
        assert!(
            STRUCTURAL_GATES.iter().any(|g| {
                g.condition == *condition && g.blocked_is_true() == *blocked && g.reason == *reason
            }),
            "{writer} writes {condition}={blocked} ({reason}), which is NOT in \
             STRUCTURAL_GATES — kubectl kopiur doctor is blind to it (#359)"
        );
    }
    assert_eq!(
        GATE_WRITERS.len(),
        STRUCTURAL_GATES.len(),
        "one writer entry per registry row"
    );
}

#[test]
fn upsert_gate_writes_exactly_what_the_row_declares() {
    use kopiur_api::gates::STRUCTURAL_GATES;
    // The registry-driven writer path: the emitted Condition must be the row,
    // including the inverted polarity of `DeletionHeld=True`.
    for gate in STRUCTURAL_GATES {
        let conds = upsert_gate(&[], gate, "why this is blocked", Some(7));
        let [written] = conds.as_slice() else {
            panic!("{gate:?}: exactly one condition");
        };
        assert_eq!(written.type_, gate.condition, "{gate:?}");
        assert_eq!(written.status, gate.blocked_status, "{gate:?}");
        assert_eq!(written.reason, gate.reason, "{gate:?}");
        assert_eq!(written.message, "why this is blocked", "{gate:?}");
        assert_eq!(written.observed_generation, Some(7), "{gate:?}");
    }
    // …and it upserts in place rather than appending a duplicate.
    let first = upsert_gate(&[], &kopiur_api::gates::PRIVILEGED_MOVER_GATE, "a", None);
    let again = upsert_gate(&first, &kopiur_api::gates::PRIVILEGED_MOVER_GATE, "b", None);
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].message, "b");
}
