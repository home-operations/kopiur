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

#[test]
fn terminal_gate_only_on_failed_at_current_generation() {
    use kopiur_api::RepositoryPhase;
    // Failed at the current generation → terminal (hard-stop).
    assert!(is_terminal_for_generation(
        Some(RepositoryPhase::Failed),
        Some(5),
        Some(5)
    ));
    // Failed but the spec moved on (gen bumped) → gate reopens, re-attempt.
    assert!(!is_terminal_for_generation(
        Some(RepositoryPhase::Failed),
        Some(5),
        Some(6)
    ));
    // Degraded (a retryable failure) is never terminal — keep retrying.
    assert!(!is_terminal_for_generation(
        Some(RepositoryPhase::Degraded),
        Some(5),
        Some(5)
    ));
    // No generation yet / no observed generation → not terminal.
    assert!(!is_terminal_for_generation(
        Some(RepositoryPhase::Failed),
        None,
        Some(5)
    ));
    assert!(!is_terminal_for_generation(
        Some(RepositoryPhase::Failed),
        Some(5),
        None
    ));
}

#[test]
fn terminal_gate_reopens_when_credential_secret_changes() {
    use kopiur_api::RepositoryPhase;
    // Terminally Failed at gen 5; the password Secret recorded at failure was rv "100".
    let failed = |recorded: Option<&str>, current: &str| {
        terminal_gate_holds(
            Some(RepositoryPhase::Failed),
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
        Some(RepositoryPhase::Degraded),
        Some(5),
        Some(5),
        Some("100"),
        "100"
    ));
    // A spec change (gen bumped) reopens regardless of the version match.
    assert!(!terminal_gate_holds(
        Some(RepositoryPhase::Failed),
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
    assert_eq!(
        m.spec.ownership.takeover_policy,
        kopiur_api::TakeoverPolicy::Never
    );
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
    );
    assert_eq!(m.metadata.namespace.as_deref(), Some("kopia-system"));
    assert_eq!(m.spec.repository.kind, RepositoryKind::ClusterRepository);
    // Cluster ref must never carry a namespace.
    assert!(m.spec.repository.namespace.is_none());
    assert_eq!(m.spec.schedule.quick.cron, "0 */2 * * *");
    assert_eq!(m.spec.ownership.owner, "kopiur/clusterrepository/hetzner");
    assert_eq!(m.spec.ownership.takeover_policy, TakeoverPolicy::Force);
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

    // THE #231 STATE: enabled, but the apply failed. It must NOT read as "disabled", and it
    // must name the namespace and the underlying error so the operator can act.
    let (status, reason, msg, warn) = maintenance_condition(
        &MaintenanceCoverage::ApplyFailed {
            namespace: "kopiur-system".into(),
            error: "maintenances.kopiur.home-operations.com is forbidden".into(),
        },
        "ClusterRepository",
        "expanse",
    );
    assert!(!status);
    assert_eq!(reason, MAINTENANCE_APPLY_FAILED_REASON);
    assert!(warn, "a failed apply is a real problem: warn");
    assert!(msg.contains("ENABLED"), "{msg}");
    assert!(msg.contains("kopiur-system"), "{msg}");
    assert!(msg.contains("is forbidden"), "{msg}");
    assert!(msg.contains("RBAC"), "{msg}");
    assert!(
        !msg.contains("spec.maintenance.enabled: false"),
        "must never claim the user disabled maintenance: {msg}"
    );

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
        Some(RepositoryPhase::Ready),
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
    let (csc, _) = inherited_security_context_from_pods(pods, Some("app"), "ns", "app=x").unwrap();
    assert_eq!(csc.unwrap().run_as_user, Some(1000));
    // No container named → the pod's FIRST container.
    let (csc, _) = inherited_security_context_from_pods(pods, None, "ns", "app=x").unwrap();
    assert_eq!(csc.unwrap().run_as_user, Some(101));
}

#[test]
fn inherit_copies_both_container_and_pod_security_context() {
    // The workload's CONTAINER runAsUser AND its POD fsGroup are both inherited, so an
    // inheriting mover matches the app at both levels (UID + writable-volume fsGroup).
    let pod = pod_with(Some("Running"), &[("app", Some(1000))], Some(1000));
    let (csc, psc) =
        inherited_security_context_from_pods(&[pod], Some("app"), "ns", "app=x").unwrap();
    assert_eq!(csc.unwrap().run_as_user, Some(1000));
    assert_eq!(psc.unwrap().fs_group, Some(1000));

    // A workload with ONLY a pod-level context (no container securityContext) still
    // inherits successfully — the pod context alone is enough.
    let pod = pod_with(Some("Running"), &[("app", None)], Some(2000));
    let (csc, psc) =
        inherited_security_context_from_pods(&[pod], Some("app"), "ns", "app=x").unwrap();
    assert!(csc.is_none());
    assert_eq!(psc.unwrap().fs_group, Some(2000));
}

#[test]
fn inherit_prefers_a_running_pod() {
    // A Pending replica (uid 5) and a Running one (uid 1000) match — Running wins.
    let pending = pod_with(Some("Pending"), &[("app", Some(5))], None);
    let running = pod_with(Some("Running"), &[("app", Some(1000))], None);
    let (csc, _) =
        inherited_security_context_from_pods(&[pending, running], Some("app"), "ns", "app=x")
            .unwrap();
    assert_eq!(csc.unwrap().run_as_user, Some(1000));
}

#[test]
fn inherit_errors_are_actionable() {
    // No pod matches.
    let err =
        inherited_security_context_from_pods(&[], Some("app"), "billing", "app=x").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("no pod matches") && msg.contains("billing") && msg.contains("app=x"));

    // Named container absent.
    let pod = pod_with(Some("Running"), &[("app", Some(1000))], None);
    let err =
        inherited_security_context_from_pods(&[pod], Some("nope"), "billing", "app=x").unwrap_err();
    assert!(err.to_string().contains("no container `nope`"));

    // The pod has NEITHER a container nor a pod-level securityContext to inherit.
    let bare = pod_with(Some("Running"), &[("app", None)], None);
    let err =
        inherited_security_context_from_pods(&[bare], Some("app"), "billing", "app=x").unwrap_err();
    assert!(
        err.to_string().contains("sets no securityContext")
            && err.to_string().contains("to inherit")
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
    let (csc, _) =
        pvc_consumer_security_context_from_pods(&[workload], "pgdata", "db", None).unwrap();
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
        pvc_consumer_security_context_from_pods(&[mover, workload], "pgdata", "db", None).unwrap();
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
            .unwrap();
    let (csc_rev, _) =
        pvc_consumer_security_context_from_pods(&[b, a], "data", "ns", None).unwrap();
    assert_eq!(csc_fwd.unwrap().run_as_user, Some(1000));
    assert_eq!(csc_rev.unwrap().run_as_user, Some(1000));
}

#[test]
fn pvc_consumer_prefers_running_over_pending() {
    let pending = pod_mounting("a-pod", "ns", Some("Pending"), Some(5), "data", false);
    let running = pod_mounting("z-pod", "ns", Some("Running"), Some(1000), "data", false);
    let (csc, _) =
        pvc_consumer_security_context_from_pods(&[pending, running], "data", "ns", None).unwrap();
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
    use super::super::events::{BootstrapFailure, BootstrapOutcome, bootstrap_outcome};
    use kopiur_kopia::KopiaErrorClass;
    use kopiur_mover::bootstrap::BootstrapResult;
    use kopiur_mover::status::FailureBlock;

    fn ok_result() -> BootstrapResult {
        BootstrapResult {
            success: true,
            created: true,
            unique_id: Some("uid-1".into()),
            snapshot_count: 0,
            snapshots: vec![],
            snapshots_truncated: false,
            index_blob_count: None,
            failure: None,
        }
    }

    #[test]
    fn four_way_mapping() {
        // (None, job succeeded): the result ConfigMap hasn't propagated yet.
        assert!(matches!(
            bootstrap_outcome(None, true, "boot-x"),
            BootstrapOutcome::ResultPending
        ));

        // (None, job failed): result-less terminal failure, names the Job.
        match bootstrap_outcome(None, false, "boot-x") {
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
        });
        match bootstrap_outcome(Some(bad), false, "boot-x") {
            BootstrapOutcome::Failed(BootstrapFailure::Backend { class, message }) => {
                assert_eq!(class, KopiaErrorClass::AuthFailure);
                assert_eq!(message, "invalid repository password");
            }
            _ => panic!("expected Backend failure"),
        }

        // (Some successful, _): the success arm owns the result.
        match bootstrap_outcome(Some(ok_result()), true, "boot-x") {
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
        match bootstrap_outcome(Some(bad), false, "boot-x") {
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
        match bootstrap_outcome(Some(r), false, "boot-x") {
            BootstrapOutcome::Failed(BootstrapFailure::RepositoryNotInitialized) => {}
            _ => panic!("expected RepositoryNotInitialized"),
        }
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
        });
        match bootstrap_outcome(Some(bad), false, "boot-x") {
            BootstrapOutcome::Failed(BootstrapFailure::Backend { class, .. }) => {
                assert_eq!(class, KopiaErrorClass::NotFound);
            }
            _ => panic!("expected Backend NotFound"),
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
