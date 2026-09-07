//! Unit coverage for the pure halves of the stream path: pod selection, exec
//! verdicts, and the messages an operator actually reads when it goes wrong.

use super::*;
use k8s_openapi::api::core::v1::{Pod, PodStatus};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Status, Time};

fn pod(name: &str, phase: &str, terminating: bool) -> Pod {
    Pod {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            // A `Time` without adding a date-library dev-dependency: it deserializes
            // from an RFC3339 string, which is exactly what the API server sends.
            deletion_timestamp: terminating.then(|| {
                serde_json::from_str::<Time>("\"2026-01-01T00:00:00Z\"").expect("valid Time")
            }),
            ..Default::default()
        },
        status: Some(PodStatus {
            phase: Some(phase.to_string()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn exactly_one_running_pod_is_selected() {
    let pods = vec![pod("pg-0", "Running", false)];
    let got = pick_stream_pod(&pods, "app=postgres", "bundlecop").expect("one running pod");
    assert_eq!(got.metadata.name.as_deref(), Some("pg-0"));
}

#[test]
fn zero_matches_says_the_workload_must_be_running() {
    let err = pick_stream_pod(&[], "app=postgres", "bundlecop").unwrap_err();
    assert!(
        err.contains("no pod matches podSelector `app=postgres`"),
        "{err}"
    );
    assert!(err.contains("bundlecop"), "{err}");
    assert!(
        err.contains("scale it up") || err.contains("fix the selector"),
        "{err}"
    );
}

#[test]
fn matches_but_none_running_is_its_own_message() {
    let pods = vec![
        pod("pg-0", "Pending", false),
        pod("pg-1", "Succeeded", false),
    ];
    let err = pick_stream_pod(&pods, "app=postgres", "bundlecop").unwrap_err();
    assert!(err.contains("none is Running"), "{err}");
    assert!(err.contains("2 pod(s)"), "{err}");
}

#[test]
fn a_terminating_pod_does_not_count_as_live() {
    let pods = vec![pod("pg-0", "Running", true)];
    let err = pick_stream_pod(&pods, "app=postgres", "bundlecop").unwrap_err();
    assert!(err.contains("none is Running and not terminating"), "{err}");
}

#[test]
fn several_running_pods_refuses_and_names_them() {
    let pods = vec![pod("pg-0", "Running", false), pod("pg-1", "Running", false)];
    let err = pick_stream_pod(&pods, "app=postgres", "bundlecop").unwrap_err();
    assert!(err.contains("matched 2 RUNNING pods"), "{err}");
    assert!(err.contains("pg-0, pg-1"), "{err}");
    assert!(err.contains("arbitrary replica"), "{err}");
}

#[test]
fn a_terminating_pod_is_ignored_leaving_one_winner() {
    let pods = vec![pod("pg-0", "Running", true), pod("pg-1", "Running", false)];
    let got = pick_stream_pod(&pods, "app=postgres", "ns").expect("the live one wins");
    assert_eq!(got.metadata.name.as_deref(), Some("pg-1"));
}

#[test]
fn success_status_is_the_only_commit_verdict() {
    assert_eq!(
        verdict_of(Some(Status {
            status: Some("Success".into()),
            ..Default::default()
        })),
        ExecVerdict::Success
    );
}

#[test]
fn a_failure_status_carries_its_message() {
    let v = verdict_of(Some(Status {
        status: Some("Failure".into()),
        message: Some("command terminated with exit code 1".into()),
        ..Default::default()
    }));
    assert_eq!(
        v,
        ExecVerdict::Failed("command terminated with exit code 1".into())
    );
}

/// The case that makes truncation detectable at all: no status means the websocket
/// died, and the byte stream cannot tell us that on its own.
#[test]
fn absent_status_is_no_status_not_success() {
    assert_eq!(verdict_of(None), ExecVerdict::NoStatus);
    assert_ne!(verdict_of(None), ExecVerdict::Success);
}

#[test]
fn no_status_message_explains_why_the_snapshot_was_discarded() {
    let msg = producer_failure_message(
        &ExecVerdict::NoStatus,
        "pg-0",
        &["pg_dumpall".to_string()],
        "",
    );
    assert!(
        msg.contains("closed without reporting an exit status"),
        "{msg}"
    );
    assert!(msg.contains("cannot be assumed complete"), "{msg}");
    assert!(msg.contains("discarded"), "{msg}");
}

#[test]
fn producer_failure_appends_bounded_stderr() {
    let msg = producer_failure_message(
        &ExecVerdict::Failed("exit code 1".into()),
        "pg-0",
        &["pg_dumpall".to_string()],
        "  FATAL: role does not exist\n",
    );
    assert!(msg.contains("exit code 1"), "{msg}");
    assert!(msg.contains("stderr: FATAL: role does not exist"), "{msg}");
}

#[test]
fn timeout_message_names_the_field_to_raise() {
    let msg = timeout_message(
        "pg-0",
        &["pg_dumpall".to_string()],
        Duration::from_secs(7200),
        "spec.sources[].stream.workloadExec",
    );
    assert!(msg.contains("7200s"), "{msg}");
    assert!(
        msg.contains("spec.sources[].stream.workloadExec.timeout"),
        "{msg}"
    );
    assert!(msg.contains("no partial dump was kept"), "{msg}");
}

/// kube-rs defaults these pipes to 1 KiB; a multi-GB dump through that would be
/// pathologically slow. Pin the override so a future refactor cannot silently drop it.
#[test]
fn exec_buffers_are_raised_above_the_kube_default() {
    assert!(
        EXEC_STREAM_BUF >= 1024 * 1024,
        "the exec stream buffer must be well above kube-rs's 1 KiB default"
    );
}

#[tokio::test]
async fn drain_stderr_is_bounded() {
    let big = vec![b'x'; EXEC_STDERR_CAP * 3];
    let mut r = std::io::Cursor::new(big);
    let got = drain_stderr(&mut r).await;
    assert_eq!(got.len(), EXEC_STDERR_CAP);
}

#[tokio::test]
async fn pump_moves_every_byte() {
    let payload: Vec<u8> = (0..100_000u32).map(|i| (i % 256) as u8).collect();
    let mut src = std::io::Cursor::new(payload.clone());
    let mut dst: Vec<u8> = Vec::new();
    let n = pump(&mut src, &mut dst).await.expect("pump");
    assert_eq!(n as usize, payload.len());
    assert_eq!(dst, payload);
}
