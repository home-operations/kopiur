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
        Duration::from_secs(kopiur_api::consts::DEFAULT_STREAM_TIMEOUT_SECS)
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

/// A stderr of EXACTLY the cap was never truncated, so it must keep its first
/// line. Inferring truncation from `len() == CAP` dropped it.
#[tokio::test]
async fn drain_stderr_keeps_the_first_line_of_an_exactly_cap_sized_stderr() {
    // "first\n" then filler, totalling exactly the cap.
    let head = b"first\n";
    let mut blob = head.to_vec();
    blob.resize(EXEC_STDERR_CAP, b'x');
    assert_eq!(blob.len(), EXEC_STDERR_CAP);

    let mut r = std::io::Cursor::new(blob);
    let got = drain_stderr(&mut r).await;
    assert_eq!(got.len(), EXEC_STDERR_CAP, "nothing was truncated");
    assert!(
        got.starts_with("first\n"),
        "an untruncated stderr must keep its first line: {:?}",
        &got[..got.len().min(20)]
    );
}

/// A kopia-side timeout during a stream restore must name
/// `spec.target.streamExec.workloadExec.timeout`, not kopia's argv (#451).
///
/// Deleting `run_streaming_stdout` in favour of `run_raw_streaming` handed
/// `show_to` a second, invisible budget: the mover derives `default_timeout` from
/// `spec.options.operationTimeout`, so a user who set that for unrelated reasons
/// would have seen a large stream restore fail with a `Timeout` quoting
/// `show <oid>` and a seconds count — the exact blames-the-wrong-knob failure the
/// exec-start split exists to eliminate. `show_to` now takes the CONSUMER's own
/// budget, and the expiry is translated by `object_read_failure_message`.
#[test]
fn a_kopia_read_timeout_names_the_stream_target_field() {
    let msg = object_read_failure_message(
        "postgres.sql",
        &kopiur_kopia::KopiaError::Timeout {
            args: "show k1234/postgres.sql".into(),
            seconds: 7200,
        },
    );
    // what
    assert!(msg.contains("postgres.sql"), "{msg}");
    assert!(msg.contains("7200s"), "{msg}");
    assert!(msg.contains("abandoned mid-stream"), "{msg}");
    // why the operator should care: the consumer got a truncated input
    assert!(msg.contains("PARTIAL load"), "{msg}");
    // fix — the field the user actually set, NOT kopia's argv
    assert!(
        msg.contains("spec.target.streamExec.workloadExec.timeout"),
        "{msg}"
    );
    assert!(
        !msg.contains("operationTimeout"),
        "the consumer's own budget bounds this, not the client-wide one: {msg}"
    );
    assert!(
        !msg.contains("show k1234"),
        "kopia's argv is not a knob the user can turn: {msg}"
    );
}

/// Every other read failure passes the underlying error through verbatim — the
/// translation is for the timeout alone, which is the only one whose cause is a
/// kopiur-imposed budget rather than something kopia reports.
#[test]
fn other_object_read_failures_pass_the_kopia_error_through() {
    let msg = object_read_failure_message(
        "postgres.sql",
        &kopiur_kopia::KopiaError::EmptyOutput {
            context: "show".into(),
            stderr_tail: "error: object not found".into(),
        },
    );
    assert!(msg.contains("postgres.sql"), "{msg}");
    assert!(msg.contains("object not found"), "{msg}");
    assert!(
        !msg.contains("workloadExec.timeout"),
        "a non-timeout must not point at the timeout knob: {msg}"
    );
}

/// The `>295 s consumer is not killed` guarantee, pinned without a cluster.
///
/// kube 4.x infers a 295 s `write_timeout`, which `hyper_timeout::TimeoutConnector`
/// applies PER WRITE and enforces by tearing the connection down. On a `streamExec`
/// restore kopiur is the writer (`kopia show` piped into the consumer's stdin), so a
/// `psql` legitimately backpressuring on one enormous statement for five minutes
/// would kill the restore — and the transport error would read as though the user's
/// command had timed out, when the budget was kopiur's own.
///
/// The real e2e proof costs five minutes of wall clock on every CI shard, for a
/// property that is entirely decided by two field assignments. This asserts those
/// instead: the default really does carry a finite write timeout (so the clearing is
/// load-bearing, not decoration), and after `clear_exec_timeouts` both per-request
/// timeouts are gone while `connect_timeout` survives.
#[test]
fn the_exec_client_config_carries_no_write_timeout() {
    let mut config = kube::Config::new("https://example.invalid".parse().expect("a valid url"));

    // Guard against the test silently becoming vacuous: if a future kube release
    // defaults `write_timeout` to None, clearing it proves nothing and this test
    // must be re-pointed at whatever the new bound is.
    assert!(
        config.write_timeout.is_some(),
        "kube still defaults to a finite write_timeout; if this fails, the timeout \
         this guards moved and the comment above is stale"
    );
    let connect = config.connect_timeout;

    clear_exec_timeouts(&mut config);

    assert!(
        config.write_timeout.is_none(),
        "a consumer that backpressures longer than kube's default must not be killed"
    );
    assert!(
        config.read_timeout.is_none(),
        "an exec/attach session must not carry a per-read timeout either"
    );
    assert_eq!(
        config.connect_timeout, connect,
        "connect_timeout is deliberately kept: a connection that cannot be \
         established should fail fast"
    );
}

/// The consumer's stderr must reach the error on a broken sink.
///
/// Regression guard with a scar: a `streamExec` restore reported only a transport
/// error while the command's own explanation sat captured-and-discarded in a local.
#[test]
fn a_broken_sink_carries_the_consumers_last_words() {
    let err = kopiur_kopia::KopiaError::OutputSink {
        args: "show kfile".into(),
        source: std::io::Error::from(std::io::ErrorKind::BrokenPipe),
    };
    let msg = super::consumer_failure_detail(
        "dump.sql",
        &err,
        "sh: can't create /tmp/restored.sql: Read-only file system\n",
        &super::ExecVerdict::NoStatus,
    );
    assert!(
        msg.contains("can't create /tmp/restored.sql"),
        "the consumer's stderr must survive: {msg}"
    );
    assert!(msg.contains("dump.sql"), "names the file: {msg}");
}

/// A broken sink with a SILENT consumer still says where to look next, rather
/// than leaving the reader with a bare "broken pipe".
#[test]
fn a_silent_broken_sink_still_points_somewhere() {
    let err = kopiur_kopia::KopiaError::OutputSink {
        args: "show kfile".into(),
        source: std::io::Error::from(std::io::ErrorKind::BrokenPipe),
    };
    let msg =
        super::consumer_failure_detail("dump.sql", &err, "   \n  ", &super::ExecVerdict::NoStatus);
    assert!(
        msg.contains("exited before the transfer finished"),
        "explains the mechanism: {msg}"
    );
    assert!(
        msg.contains("reported no status"),
        "says the exec status was unavailable rather than staying silent: {msg}"
    );
    assert!(
        msg.contains("mover Job's logs"),
        "names the next place to look: {msg}"
    );
}

/// A kopia-side TIMEOUT keeps naming the user's own field, and does not acquire
/// consumer-exit language it has not earned.
#[test]
fn a_timeout_is_not_reframed_as_a_consumer_exit() {
    let err = kopiur_kopia::KopiaError::Timeout {
        args: "show kfile".into(),
        seconds: 300,
    };
    let msg = super::consumer_failure_detail("dump.sql", &err, "", &super::ExecVerdict::NoStatus);
    assert!(
        msg.contains("workloadExec.timeout"),
        "still names the field the user set: {msg}"
    );
    assert!(
        !msg.contains("exited before the transfer finished"),
        "must not claim the consumer exited: {msg}"
    );
}

/// The exec verdict is the authoritative "why" and must reach the message.
#[test]
fn the_exec_verdict_explains_a_broken_sink() {
    let err = kopiur_kopia::KopiaError::OutputSink {
        args: "show kfile".into(),
        source: std::io::Error::from(std::io::ErrorKind::BrokenPipe),
    };
    let msg = super::consumer_failure_detail(
        "dump.sql",
        &err,
        "",
        &super::ExecVerdict::Failed("command terminated with exit code 2".into()),
    );
    assert!(
        msg.contains("exit code 2"),
        "carries the exec status: {msg}"
    );
    assert!(
        !msg.contains("reported no status"),
        "must not also claim there was no status: {msg}"
    );
}

/// A command that SUCCEEDS while our write is still going is the "stopped reading
/// early" shape, not a contradiction — and the message must say which.
#[test]
fn a_successful_command_beside_a_broken_sink_names_the_early_exit() {
    let err = kopiur_kopia::KopiaError::OutputSink {
        args: "show kfile".into(),
        source: std::io::Error::from(std::io::ErrorKind::BrokenPipe),
    };
    let msg = super::consumer_failure_detail("dump.sql", &err, "", &super::ExecVerdict::Success);
    assert!(
        msg.contains("stopped reading early") || msg.contains("consume its stdin"),
        "names the real shape: {msg}"
    );
}

/// The consumer's argv is wrapped so a shell stays alive holding stdin.
///
/// Not cosmetic: unwrapped, Kubernetes exec silently truncated an 8 MiB payload to
/// 131,072 bytes while `write_all` returned Ok. See `stdin_holding_command`.
#[test]
fn the_consumer_command_is_wrapped_to_hold_stdin_open() {
    let argv = vec!["psql".to_string(), "-d".to_string(), "mydb".to_string()];
    let got = super::stdin_holding_command(&argv);
    assert_eq!(got[0], "sh");
    assert_eq!(got[1], "-c");
    // TWO statements: the trailing `exit $?` both propagates the exit code and
    // defeats the shell's exec-the-last-command optimisation, which is what
    // re-creates the truncating shape.
    assert_eq!(got[2], r#""$@"; exit $?"#);
    assert!(
        got[2].contains("; exit $?"),
        "must stay two statements: {:?}",
        got[2]
    );
    assert_eq!(got[3], super::STREAM_WRAPPER_ARGV0, "$0 names itself");
    assert_eq!(&got[4..], &argv[..], "argv follows verbatim as $1..");
}

/// argv rides as positional parameters, never interpolated into the script, so an
/// argument carrying shell metacharacters cannot be re-parsed as shell syntax.
#[test]
fn a_hostile_argument_cannot_become_shell_syntax() {
    let argv = vec![
        "psql".to_string(),
        "-c".to_string(),
        "SELECT 1; rm -rf /".to_string(),
    ];
    let got = super::stdin_holding_command(&argv);
    // The script text is a fixed string — it never grows with user input.
    assert_eq!(got[2], r#""$@"; exit $?"#);
    assert!(
        !got[2].contains("rm -rf"),
        "user input must not reach the script text: {:?}",
        got[2]
    );
    // And the hostile argument survives intact as ONE argument.
    assert_eq!(got.last().unwrap(), "SELECT 1; rm -rf /");
    assert_eq!(got.len(), 4 + argv.len(), "no splitting, no extra args");
}

/// An empty consumer argv is still a well-formed wrapper (admission rejects empty
/// commands, so this pins the shape rather than endorsing the input).
#[test]
fn wrapping_an_empty_argv_is_still_well_formed() {
    let got = super::stdin_holding_command(&[]);
    assert_eq!(got.len(), 4);
    assert_eq!(got[0], "sh");
    assert_eq!(got[3], super::STREAM_WRAPPER_ARGV0);
}
