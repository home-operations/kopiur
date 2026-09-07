//! Streamed command sources: exec a command in a running workload pod and move its
//! bytes to or from kopia without ever touching a filesystem.
//!
//! # Where the correctness lives
//!
//! A backup here has TWO ways to fail that a filesystem backup does not: the
//! producer can fail after writing bytes, and the exec connection can drop
//! mid-transfer. Both must be impossible to mistake for success, because kopia
//! cannot tell the difference on its own — from its side, a dropped connection and
//! a clean finish both look like EOF on stdin.
//!
//! The guard is the exec STATUS, not the byte stream:
//!
//! * `Some(status == "Success")` — the command exited 0. Only then do we let kopia
//!   see EOF and commit ([`kopiur_kopia::StdinOutcome::Commit`]).
//! * `Some(other)` — the command failed. Abort.
//! * `None` — the status channel closed without a verdict, i.e. the websocket died.
//!   The stdout reader will have returned a perfectly clean EOF, so this is the ONLY
//!   signal distinguishing "finished" from "connection dropped mid-dump". Abort.
//! * timeout / IO error — abort.
//!
//! Aborting kills kopia with its stdin still open, so no manifest is ever written
//! (see [`kopiur_kopia::KopiaClient::snapshot_create_stdin_outcome_with`]).
//!
//! # Where the data does NOT go
//!
//! The producer's stdout is a database dump. It is copied pipe-to-pipe and is never
//! collected into a `String`, logged, attached to an error, or written to a CR
//! status. Only stderr is captured, and only a bounded tail of it.

use std::time::Duration;

use k8s_openapi::api::core::v1::Pod;
use kube::api::{AttachParams, ListParams};
use kube::{Api, ResourceExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::{MoverError, Result};
use crate::workspec::{StreamConsumerSpec, StreamProducerSpec};
use kopiur_kopia::StdinOutcome;

/// Bytes of exec stderr kept for diagnostics. Bounded because the point of the cap
/// is that an operator sees WHY a dump failed without the failure message becoming
/// unbounded — and because stderr is the one stream from a database command that
/// could plausibly echo data.
pub const EXEC_STDERR_CAP: usize = 8 * 1024;

/// Buffer for the exec stdout/stdin duplex pipes.
///
/// kube-rs defaults these to 1 KiB (`remote_command.rs: MAX_BUF_SIZE`), which would
/// make a multi-gigabyte logical dump crawl — one wakeup per kilobyte. 4 MiB keeps
/// the websocket saturated. Backpressure still works end to end: a full duplex
/// blocks the message loop, which stops reading the socket, which TCP-backpressures
/// the kubelet, which blocks the producer's `write(2)`.
pub const EXEC_STREAM_BUF: usize = 4 * 1024 * 1024;

/// Exec stderr never exceeds a line or two of diagnostics; keep its buffer small.
pub const EXEC_STDERR_BUF: usize = 64 * 1024;

/// Pick the one pod a stream source may exec into.
///
/// Requires EXACTLY ONE `Running`, non-terminating match. Zero, several, or
/// only-not-running are each a distinct named failure rather than an arbitrary
/// pick: a backup that silently dumped a different replica than the operator
/// intended is worse than a backup that stops and says why.
///
/// Pure over the listed pods so every message is unit-testable without a cluster.
pub fn pick_stream_pod<'a>(
    pods: &'a [Pod],
    selector: &str,
    namespace: &str,
) -> std::result::Result<&'a Pod, String> {
    if pods.is_empty() {
        return Err(format!(
            "no pod matches podSelector `{selector}` in namespace `{namespace}`. The workload \
             must be running for a stream source to dump from it — scale it up, or fix the \
             selector"
        ));
    }
    let live: Vec<&Pod> = pods
        .iter()
        .filter(|p| pod_is_running(p) && p.metadata.deletion_timestamp.is_none())
        .collect();
    match live.as_slice() {
        [] => Err(format!(
            "podSelector `{selector}` matched {} pod(s) in namespace `{namespace}`, but none is \
             Running and not terminating. A stream source needs a live container to exec into; \
             wait for the workload to become ready, or fix the selector",
            pods.len()
        )),
        [one] => Ok(one),
        many => Err(format!(
            "podSelector `{selector}` matched {} RUNNING pods in namespace `{namespace}` ({}). A \
             stream source must identify exactly one pod — dumping an arbitrary replica would \
             make the backup's contents depend on scheduling. Narrow the selector (e.g. add a \
             role/primary label) so it matches only the pod you mean to dump",
            many.len(),
            many.iter()
                .map(|p| p.name_any())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Whether a pod is in the `Running` phase.
fn pod_is_running(p: &Pod) -> bool {
    p.status
        .as_ref()
        .and_then(|s| s.phase.as_deref())
        .is_some_and(|ph| ph == "Running")
}

/// The verdict an exec ended with, derived from the websocket status channel.
///
/// `None` from `take_status()` is deliberately its own arm rather than folded into
/// failure-with-no-message: it is the connection-dropped case, and telling an
/// operator "the exec connection closed without reporting an exit status" instead of
/// "the command failed" is the difference between suspecting the network and
/// suspecting their dump command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecVerdict {
    /// The command exited 0.
    Success,
    /// The command exited non-zero (or the API server reported a failure).
    Failed(String),
    /// The status channel closed without a verdict — the connection dropped.
    NoStatus,
}

/// Turn a websocket exec status into an [`ExecVerdict`]. Pure, so the mapping is
/// unit-testable without a cluster.
pub fn verdict_of(
    status: Option<k8s_openapi::apimachinery::pkg::apis::meta::v1::Status>,
) -> ExecVerdict {
    match status {
        Some(s) if s.status.as_deref() == Some("Success") => ExecVerdict::Success,
        Some(s) => ExecVerdict::Failed(s.message.unwrap_or_else(|| "non-zero exit".to_string())),
        None => ExecVerdict::NoStatus,
    }
}

/// The actionable failure message for a producer that did not succeed.
pub fn producer_failure_message(
    verdict: &ExecVerdict,
    pod: &str,
    command: &[String],
    stderr_tail: &str,
) -> String {
    let head = match verdict {
        ExecVerdict::Success => "the producer succeeded".to_string(),
        ExecVerdict::Failed(detail) => {
            format!("the stream command {command:?} in pod `{pod}` failed: {detail}")
        }
        ExecVerdict::NoStatus => format!(
            "the exec connection to pod `{pod}` closed without reporting an exit status, so the \
             dump cannot be assumed complete (a dropped connection is indistinguishable from a \
             clean end-of-output on its own). The snapshot was discarded"
        ),
    };
    if stderr_tail.trim().is_empty() {
        head
    } else {
        format!("{head}; stderr: {}", stderr_tail.trim())
    }
}

/// Timeout message for a producer/consumer that overran its budget.
pub fn timeout_message(pod: &str, command: &[String], timeout: Duration, field: &str) -> String {
    format!(
        "the stream command {command:?} in pod `{pod}` did not finish within {}s. Raise \
         `{field}.timeout`, or make the command faster. The snapshot was discarded, so no \
         partial dump was kept",
        timeout.as_secs()
    )
}

/// Resolve the single workload pod for `selector` in `namespace`.
pub async fn resolve_pod(client: &kube::Client, namespace: &str, selector: &str) -> Result<Pod> {
    let api: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let listed = api
        .list(&ListParams::default().labels(selector))
        .await
        .map_err(|e| MoverError::StreamPodResolve {
            detail: format!("listing pods with selector `{selector}` in `{namespace}` failed: {e}"),
        })?;
    pick_stream_pod(&listed.items, selector, namespace)
        .map(|p| p.clone())
        .map_err(|detail| MoverError::StreamPodResolve { detail })
}

/// [`AttachParams`] for a producer exec: read stdout and stderr, no stdin, with
/// buffers sized for bulk transfer rather than kube-rs's 1 KiB default.
pub fn producer_attach_params(container: Option<&str>) -> AttachParams {
    let mut p = AttachParams::default()
        .stdin(false)
        .stdout(true)
        .stderr(true)
        .max_stdout_buf_size(EXEC_STREAM_BUF)
        .max_stderr_buf_size(EXEC_STDERR_BUF);
    if let Some(c) = container {
        p = p.container(c.to_string());
    }
    p
}

/// [`AttachParams`] for a consumer exec: write stdin, read stderr only.
///
/// `stdout(false)` is deliberate. The restore target is something like `psql`, whose
/// stdout is chatter we have no use for — and not requesting it means kube-rs never
/// even opens that channel, so there is no path by which restored data could be
/// echoed back into our process.
pub fn consumer_attach_params(container: Option<&str>) -> AttachParams {
    let mut p = AttachParams::default()
        .stdin(true)
        .stdout(false)
        .stderr(true)
        .max_stdin_buf_size(EXEC_STREAM_BUF)
        .max_stderr_buf_size(EXEC_STDERR_BUF);
    if let Some(c) = container {
        p = p.container(c.to_string());
    }
    p
}

/// Read at most [`EXEC_STDERR_CAP`] bytes of `reader` into a string.
///
/// Bounded so a chatty command can neither stall on a full pipe nor turn a failure
/// message into a log dump.
pub async fn drain_stderr<R>(reader: &mut R) -> String
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf = Vec::new();
    let _ = reader
        .take(EXEC_STDERR_CAP as u64)
        .read_to_end(&mut buf)
        .await;
    String::from_utf8_lossy(&buf).into_owned()
}

/// Copy `reader` into `writer` until EOF, returning the byte count.
///
/// A thin wrapper so the backup path has ONE place where the dump bytes are handled,
/// and it is obvious by inspection that they only ever move between the two pipes.
pub async fn pump<R, W>(reader: &mut R, writer: &mut W) -> std::io::Result<u64>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let n = tokio::io::copy(reader, writer).await?;
    writer.flush().await?;
    Ok(n)
}

/// Run a stream producer and feed its stdout into `stdin`, returning the outcome
/// kopia's runner needs.
///
/// The contract that matters: this returns [`StdinOutcome::Commit`] ONLY when the
/// exec reported `Success`. Everything else — non-zero exit, a dropped connection
/// (`NoStatus`), a timeout, or an IO error — returns `Abort`, which kills kopia with
/// stdin still open so no manifest is ever written.
///
/// `stdout` is copied pipe-to-pipe and never inspected. `on_failure` receives the
/// operator-facing message, which contains only the verdict and a bounded stderr
/// tail.
pub async fn feed_from_pod(
    client: &kube::Client,
    spec: &StreamProducerSpec,
    stdin: &mut tokio::process::ChildStdin,
    failure: &mut Option<String>,
) -> StdinOutcome {
    let timeout = Duration::from_secs(spec.timeout_seconds);
    let pod = match resolve_pod(client, &spec.namespace, &spec.pod_selector).await {
        Ok(p) => p,
        Err(e) => {
            *failure = Some(e.to_string());
            return StdinOutcome::Abort;
        }
    };
    let pod_name = pod.name_any();

    let api: Api<Pod> = Api::namespaced(client.clone(), &spec.namespace);
    let params = producer_attach_params(spec.container.as_deref());
    let attach = api.exec(&pod_name, spec.command.clone(), &params);
    let mut attached = match tokio::time::timeout(timeout, attach).await {
        Ok(Ok(a)) => a,
        Ok(Err(e)) => {
            *failure = Some(format!(
                "could not exec into pod `{pod_name}`: {e}. Check that the kopiur mover \
                 ServiceAccount is allowed `pods/exec` in namespace `{}`",
                spec.namespace
            ));
            return StdinOutcome::Abort;
        }
        Err(_) => {
            *failure = Some(format!(
                "the exec into pod `{pod_name}` did not start within {}s",
                timeout.as_secs()
            ));
            return StdinOutcome::Abort;
        }
    };

    let Some(mut out) = attached.stdout() else {
        *failure = Some("the exec stream provided no stdout channel".to_string());
        return StdinOutcome::Abort;
    };
    let mut errs = attached.stderr();
    let status_fut = attached.take_status();

    // Pump stdout→kopia and drain stderr TOGETHER. Both channels ride one websocket,
    // so leaving stderr unread would eventually block the message loop and stall the
    // dump — a hang that would look exactly like a slow database.
    let mut stderr_tail = String::new();
    let transfer = async {
        let pumped = match errs.as_mut() {
            Some(e) => {
                let (pumped, tail) = tokio::join!(pump(&mut out, stdin), drain_stderr(e));
                stderr_tail = tail;
                pumped
            }
            None => pump(&mut out, stdin).await,
        };
        pumped
    };

    let pumped = match tokio::time::timeout(timeout, transfer).await {
        Ok(r) => r,
        Err(_) => {
            *failure = Some(timeout_message(
                &pod_name,
                &spec.command,
                timeout,
                "spec.sources[].stream.workloadExec",
            ));
            return StdinOutcome::Abort;
        }
    };
    if let Err(e) = pumped {
        *failure = Some(format!(
            "the dump stream from pod `{pod_name}` broke mid-transfer: {e}. The snapshot was \
             discarded"
        ));
        return StdinOutcome::Abort;
    }

    // THE decision point. stdout reaching EOF proves nothing — a dropped websocket
    // ends the stream just as cleanly as a finished command — so the verdict comes
    // from the status channel and nowhere else.
    let verdict = match status_fut {
        Some(f) => match tokio::time::timeout(timeout, f).await {
            Ok(s) => verdict_of(s),
            Err(_) => {
                *failure = Some(timeout_message(
                    &pod_name,
                    &spec.command,
                    timeout,
                    "spec.sources[].stream.workloadExec",
                ));
                return StdinOutcome::Abort;
            }
        },
        None => ExecVerdict::NoStatus,
    };

    match verdict {
        ExecVerdict::Success => {
            tracing::info!(
                pod = %pod_name,
                bytes = pumped.unwrap_or(0),
                file = %spec.file_name,
                "stream producer finished; committing the snapshot"
            );
            StdinOutcome::Commit
        }
        other => {
            *failure = Some(producer_failure_message(
                &other,
                &pod_name,
                &spec.command,
                &stderr_tail,
            ));
            StdinOutcome::Abort
        }
    }
}

/// Feed a restored virtual file into a command's stdin in a running pod.
///
/// The mirror of [`feed_from_pod`]. Both halves must succeed: kopia must read the
/// object cleanly AND the consuming command must exit 0 — a `psql` that failed
/// halfway leaves a half-loaded database, and reporting that as a completed restore
/// would be worse than reporting nothing.
pub async fn restore_into_pod(
    client: &kube::Client,
    kopia: &kopiur_kopia::KopiaClient,
    spec: &StreamConsumerSpec,
    object_id: &str,
) -> Result<()> {
    let timeout = Duration::from_secs(spec.timeout_seconds);
    let pod = resolve_pod(client, &spec.namespace, &spec.pod_selector).await?;
    let pod_name = pod.name_any();

    let api: Api<Pod> = Api::namespaced(client.clone(), &spec.namespace);
    let params = consumer_attach_params(spec.container.as_deref());
    let attach = api.exec(&pod_name, spec.command.clone(), &params);
    let mut attached = tokio::time::timeout(timeout, attach)
        .await
        .map_err(|_| MoverError::StreamExecFailed {
            detail: format!(
                "the exec into pod `{pod_name}` did not start within {}s",
                timeout.as_secs()
            ),
        })?
        .map_err(|e| MoverError::StreamExecFailed {
            detail: format!(
                "could not exec into pod `{pod_name}`: {e}. Check that the kopiur mover \
                 ServiceAccount is allowed `pods/exec` in namespace `{}`",
                spec.namespace
            ),
        })?;

    let mut stdin = attached
        .stdin()
        .ok_or_else(|| MoverError::StreamExecFailed {
            detail: "the exec stream provided no stdin channel".to_string(),
        })?;
    let mut errs = attached.stderr();
    let status_fut = attached.take_status();

    let mut stderr_tail = String::new();
    let shipped = {
        let ship = async {
            // `show_to` streams the object straight into the exec's stdin.
            let r = kopia.show_to(object_id, &mut stdin).await;
            // Close stdin so the consumer sees EOF and can finish (psql will not exit
            // while its input is still open).
            let _ = stdin.shutdown().await;
            drop(stdin);
            r
        };
        match errs.as_mut() {
            Some(e) => {
                let (r, tail) =
                    tokio::time::timeout(timeout, async { tokio::join!(ship, drain_stderr(e)) })
                        .await
                        .map_err(|_| MoverError::StreamExecFailed {
                            detail: timeout_message(
                                &pod_name,
                                &spec.command,
                                timeout,
                                "spec.target.streamExec.workloadExec",
                            ),
                        })?;
                stderr_tail = tail;
                r
            }
            None => tokio::time::timeout(timeout, ship).await.map_err(|_| {
                MoverError::StreamExecFailed {
                    detail: timeout_message(
                        &pod_name,
                        &spec.command,
                        timeout,
                        "spec.target.streamExec.workloadExec",
                    ),
                }
            })?,
        }
    };
    shipped.map_err(|e| MoverError::StreamExecFailed {
        detail: format!(
            "reading `{}` out of the snapshot failed: {e}",
            spec.file_name
        ),
    })?;

    let verdict = match status_fut {
        Some(f) => match tokio::time::timeout(timeout, f).await {
            Ok(s) => verdict_of(s),
            Err(_) => {
                return Err(MoverError::StreamExecFailed {
                    detail: timeout_message(
                        &pod_name,
                        &spec.command,
                        timeout,
                        "spec.target.streamExec.workloadExec",
                    ),
                });
            }
        },
        None => ExecVerdict::NoStatus,
    };
    match verdict {
        ExecVerdict::Success => {
            tracing::info!(pod = %pod_name, file = %spec.file_name, "stream restore consumer finished");
            Ok(())
        }
        other => Err(MoverError::StreamExecFailed {
            detail: producer_failure_message(&other, &pod_name, &spec.command, &stderr_tail),
        }),
    }
}

#[cfg(test)]
mod tests;
