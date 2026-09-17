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
    const {
        assert!(
            EXEC_STREAM_BUF >= 1024 * 1024,
            "the exec stream buffer must be well above kube-rs's 1 KiB default"
        )
    };
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

// --- drain_stderr must not throttle the shared websocket (#451) -------------

/// THE regression guard. `drain_stderr` used to be `take(CAP).read_to_end(..)`,
/// which STOPS READING at the cap. stderr and stdout ride the same websocket, so
/// a producer printing more than 8 KiB of diagnostics (any `pg_dumpall
/// --verbose`) would fill the stderr channel, block kube-rs's message loop and
/// stall the stdout pump — a hang indistinguishable from a slow database until the
/// transfer timeout fired and blamed the user's command.
///
/// Proven by exhausting the reader: a `Cursor` whose position reaches its length
/// was read to EOF.
#[tokio::test]
async fn drain_stderr_reads_to_eof_even_far_past_the_cap() {
    let big = vec![b'x'; EXEC_STDERR_CAP * 5];
    let total = big.len() as u64;
    let mut r = std::io::Cursor::new(big);
    let got = drain_stderr(&mut r).await;
    assert_eq!(
        r.position(),
        total,
        "drain_stderr must consume the whole channel, not stop at the cap — \
         stopping stalls the stdout pump on the shared websocket"
    );
    assert!(
        got.len() <= EXEC_STDERR_CAP,
        "what is RETAINED is still bounded: {}",
        got.len()
    );
}

/// The cap applies to the TAIL: the last lines of a failed command say why it
/// failed, the first 8 KiB of a verbose dump are its banner. The field this feeds
/// is called `stderr_tail`.
#[tokio::test]
async fn drain_stderr_keeps_the_tail_not_the_head() {
    let mut blob = String::new();
    for i in 0..4000 {
        blob.push_str(&format!("pg_dump: dumping contents of table number {i}\n"));
    }
    blob.push_str("pg_dump: error: query failed: permission denied for table secrets\n");
    assert!(blob.len() > EXEC_STDERR_CAP * 2, "must actually truncate");

    let mut r = std::io::Cursor::new(blob.into_bytes());
    let got = drain_stderr(&mut r).await;
    assert!(
        got.contains("permission denied for table secrets"),
        "the actual error must survive truncation: {got}"
    );
    assert!(
        !got.contains("table number 0\n"),
        "the banner must be the part that is dropped"
    );
    // Truncation never opens mid-line.
    assert!(
        got.starts_with("pg_dump: "),
        "a truncated tail must start at a line boundary: {:?}",
        &got[..got.len().min(60)]
    );
}

/// A short stderr is returned verbatim — the line-boundary trim only applies when
/// the tail was actually truncated.
#[tokio::test]
async fn drain_stderr_keeps_a_short_stderr_whole() {
    let mut r = std::io::Cursor::new(b"line one\nline two\n".to_vec());
    assert_eq!(drain_stderr(&mut r).await, "line one\nline two\n");
}

// --- the exec-start budget is not the dump budget (#451) --------------------

/// The exec UPGRADE gets a short fixed budget, not the user's
/// `workloadExec.timeout`. Spending a two-hour dump budget waiting for an attach
/// that will never succeed turns an immediate "cannot exec into this pod" into a
/// two-hour silent hang and then points at the wrong knob.
#[test]
fn the_exec_start_budget_is_short_and_fixed() {
    use std::time::Duration;
    assert!(EXEC_START_TIMEOUT <= Duration::from_secs(60));
    assert!(EXEC_START_TIMEOUT >= Duration::from_secs(10));
    // And it is emphatically NOT the default dump budget.
    assert_ne!(
        EXEC_START_TIMEOUT,
        Duration::from_secs(kopiur_api::snapshot_policy::DEFAULT_STREAM_TIMEOUT_SECS)
    );
}

/// The message must send the operator to the control plane, not to
/// `workloadExec.timeout` — which this failure never consumed.
#[test]
fn the_exec_start_timeout_message_names_the_right_cause() {
    let msg = exec_start_timeout_message("postgres-0", "db");
    assert!(msg.contains("postgres-0"), "{msg}");
    assert!(msg.contains("db/postgres-0"), "{msg}");
    assert!(msg.contains("no snapshot was written"), "{msg}");
    assert!(msg.contains("pods/exec"), "{msg}");
    assert!(
        msg.contains("NOT `workloadExec.timeout`"),
        "must steer away from the wrong knob: {msg}"
    );
}
