//! Streaming one file out of a snapshot.
//!
//! The kopia exec writes into a `tokio::io::duplex` pipe that becomes the
//! response body, capped at exactly the entry's recorded size: a short or overlong
//! copy is counted and surfaced as an error rather than handed to the browser as a
//! silently-truncated file with a plausible name.
//!
//! # Why the size is a contract and not a hint
//!
//! `Content-Length` is set from the snapshot manifest before a single byte is
//! read, because that is the only number the browser will believe — and because
//! promising a length the body then fails to deliver is how a backup restore
//! quietly loses data. A file that arrives one byte short looks, to every tool
//! downstream, exactly like a file that was one byte short in the backup. So
//! [`ExactSink`] refuses the first byte past the promised size, the copy is
//! checked against it afterwards, and a mismatch increments
//! [`DownloadIncomplete`].
//!
//! # …and why a cancelled download is not one of those
//!
//! `kopiur_ui_download_incomplete_total{cause="download-short"}` is meant to be
//! an integrity alarm, so it must not ring for a person hitting Escape. A client
//! that goes away arrives here as `BrokenPipe`/`ConnectionReset`/`UnexpectedEof`
//! and is counted as `client-cancelled` instead — a separate, expected-nonzero
//! bucket ([`classify`] is the single place that decision is made, and it is
//! pure, so the table is a unit test rather than a claim).
//!
//! # The watchdog is the only thing bounding this route
//!
//! `GET …/file` is deliberately exempt from the API's request timeout — a
//! legitimate multi-gigabyte restore outlives any fixed deadline. What replaces
//! it is a *progress* watchdog ([`under_watchdog`]): if [`ExactSink`] accepts no
//! byte for a whole `KOPIUR_UI_DOWNLOAD_CHUNK_TIMEOUT` window, the copy future is
//! dropped. That covers both directions of silence with one mechanism — a hung
//! kopia or a wedged exec websocket (the source stops producing) and a closed
//! laptop or half-open TCP connection (the client stops draining, the pipe fills,
//! and writes stop completing) — because neither one advances the byte count.
//! Without it, one stalled transfer pins a `pods/exec` slot, an apiserver
//! websocket and a kopia process indefinitely.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use tokio::io::AsyncWrite;
use tokio_util::io::ReaderStream;

use kopiur_kopia::{ObjectId, SessionCmd};
use kopiur_ops::browse::session::ExecSession;
use kopiur_ops::error::OpsError;

use crate::browse::session_pool::ExecPermit;
use crate::metrics::{DownloadIncomplete, UiMetrics};

/// Bytes buffered between the kopia exec and the HTTP response.
///
/// Back-pressure is the point: when the client stops reading, this fills, the
/// copy blocks, and the session pod stops producing. A larger buffer would only
/// let a stalled download hold more memory per connection.
const PIPE_BYTES: usize = 64 * 1024;

/// Build the response body for one file download.
///
/// The exec runs in a spawned task rather than inline, because the body must be
/// returned to axum before the first byte exists — a handler that awaited the
/// copy would buffer the whole file in memory to produce a `Body` from it. The
/// task owns `permit` for its whole life, so the exec cap counts a download for
/// as long as it is actually running rather than for the instant that started
/// it, and every exit path from the task — success, error, stall, or a panic
/// the runtime unwinds — drops it.
///
/// The task never fails the response: by the time the body is streaming, the
/// status and headers are already on the wire. A failure therefore shows up as a
/// short body (which the client sees as a truncated download against the
/// promised `Content-Length`), a `warn!` naming the object, and — unless the
/// client was the one who left — a [`DownloadIncomplete`] count.
pub fn stream_file(
    session: ExecSession,
    oid: ObjectId,
    size: u64,
    permit: ExecPermit,
    metrics: Arc<UiMetrics>,
    chunk_timeout: Duration,
) -> Body {
    let (writer, reader) = tokio::io::duplex(PIPE_BYTES);

    tokio::spawn(async move {
        // Held for the whole copy: the exec cap bounds concurrent transfers, not
        // concurrent requests that start one.
        let _permit = permit;
        let mut sink = ExactSink::new(writer, size);
        // Cloned out before the copy borrows the sink — it is the only way the
        // watchdog can see progress while the copy holds `&mut sink`.
        let progress = sink.progress();

        let copied = under_watchdog(
            session.exec_stream(SessionCmd::ShowObject { oid: oid.clone() }, &mut sink),
            progress,
            chunk_timeout,
        )
        .await;

        let (stalled, error) = match &copied {
            Copied::Stalled => (true, None),
            Copied::Finished(Ok(_)) => (false, None),
            Copied::Finished(Err(e)) => (false, Some(e)),
        };
        let outcome = classify(&CopyResult {
            expected: size,
            written: sink.written(),
            overran: sink.overran(),
            stalled,
            error,
        });

        // Drop the writer half before reporting, so the client's body ends now
        // rather than after the log line and the metric.
        let written = sink.written();
        drop(sink);

        if let Some(cause) = outcome.metric() {
            metrics.inc_download_incomplete(cause);
        }
        if !matches!(outcome, DownloadOutcome::Complete) {
            tracing::warn!(
                oid = oid.as_str(),
                expected = size,
                written,
                outcome = outcome.as_str(),
                error = error.map(|e| e.to_string()).unwrap_or_default(),
                "{}",
                outcome.summary()
            );
        }
    });

    Body::from_stream(ReaderStream::new(reader))
}

// --- classification ---------------------------------------------------------

/// Everything a finished copy knows about itself. Grouped into a struct so
/// [`classify`] reads as a table rather than as five positional booleans.
#[derive(Debug)]
pub struct CopyResult<'a> {
    /// The size the response committed to in `Content-Length`.
    pub expected: u64,
    /// Bytes [`ExactSink`] accepted.
    pub written: u64,
    /// Whether the sink refused a byte past `expected`.
    pub overran: bool,
    /// Whether the watchdog abandoned the copy for lack of progress.
    pub stalled: bool,
    /// The copy's error, when it ended in one.
    pub error: Option<&'a OpsError>,
}

/// How a download ended.
///
/// A closed enum so [`DownloadOutcome::metric`] is exhaustive: a new way for a
/// transfer to end must decide, at compile time, whether it is an integrity
/// event or an expected one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadOutcome {
    /// Exactly the promised bytes reached the client.
    Complete,
    /// The client went away mid-transfer. Not an integrity event.
    ClientCancelled,
    /// Neither side moved a byte for a whole watchdog window.
    Stalled,
    /// kopia produced more than the snapshot entry records.
    Overrun,
    /// The copy ended before the promised size, for any other reason.
    Short,
}

impl DownloadOutcome {
    /// The metric this outcome counts as, or `None` when nothing is wrong.
    ///
    /// A stall counts as `download-short` because that is what the client is
    /// left holding — a body that stopped before its committed length. The
    /// *cause* is in the log line; the metric records the consequence.
    pub fn metric(self) -> Option<DownloadIncomplete> {
        match self {
            Self::Complete => None,
            Self::ClientCancelled => Some(DownloadIncomplete::ClientCancelled),
            Self::Stalled | Self::Short => Some(DownloadIncomplete::Short),
            Self::Overrun => Some(DownloadIncomplete::Overrun),
        }
    }

    /// A stable token for the log field.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::ClientCancelled => "client-cancelled",
            Self::Stalled => "stalled",
            Self::Overrun => "download-overrun",
            Self::Short => "download-short",
        }
    }

    /// The log line's message: what an operator reading it needs to know.
    fn summary(self) -> &'static str {
        match self {
            Self::Complete => "a file download completed",
            Self::ClientCancelled => {
                "a file download was abandoned by the client; nothing is wrong with the backup"
            }
            Self::Stalled => {
                "a file download made no progress for a whole \
                 KOPIUR_UI_DOWNLOAD_CHUNK_TIMEOUT window and was abandoned; the client holds a \
                 truncated file"
            }
            Self::Overrun => {
                "a file download produced more bytes than the snapshot entry records; the copy \
                 was cut off at the recorded size"
            }
            Self::Short => {
                "a file download ended before the size recorded in the snapshot; the client \
                 holds a truncated file"
            }
        }
    }
}

/// **Pure.** Decide what a finished copy was.
///
/// Order matters and is the argument:
///
/// 1. **Overrun first.** It is the only outcome the sink itself diagnosed, and
///    it means kopia and the manifest disagree — a repository-level fact that
///    outranks whatever the transport then did about it.
/// 2. **Stall next**, because a watchdog abort also surfaces as a short count,
///    and the reason is worth keeping.
/// 3. **Client-gone next**, so an ordinary cancellation never lands in the
///    integrity bucket.
/// 4. Then the size comparison.
pub fn classify(result: &CopyResult<'_>) -> DownloadOutcome {
    if result.overran {
        return DownloadOutcome::Overrun;
    }
    if result.stalled {
        return DownloadOutcome::Stalled;
    }
    if result.error.is_some_and(client_went_away) {
        return DownloadOutcome::ClientCancelled;
    }
    if result.error.is_none() && result.written == result.expected {
        return DownloadOutcome::Complete;
    }
    DownloadOutcome::Short
}

/// Whether an error means the *client* hung up rather than the snapshot being
/// wrong.
///
/// The signal is the duplex pipe's write half: when axum drops the response body
/// the reader half drops, and the next write returns `BrokenPipe`. `tokio::io::copy`
/// inside `exec_stream` turns that into [`OpsError::StreamIo`], so the IO error
/// kind is the whole of the evidence. `ConnectionReset` and `UnexpectedEof` are
/// the same event seen through a socket that was reset or truncated instead.
fn client_went_away(error: &OpsError) -> bool {
    match error {
        OpsError::StreamIo { source, .. } => matches!(
            source.kind(),
            io::ErrorKind::BrokenPipe
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::UnexpectedEof
        ),
        _other => false,
    }
}

// --- the progress watchdog --------------------------------------------------

/// What [`under_watchdog`] observed.
#[derive(Debug)]
pub enum Copied<T, E> {
    /// The copy ran to its own conclusion, successful or not.
    Finished(Result<T, E>),
    /// No byte was accepted for a whole window, so the copy future was dropped.
    Stalled,
}

/// Drive `copy` to completion, abandoning it if `progress` stops advancing.
///
/// Generic over the future rather than written inline against `exec_stream`, so
/// the abandonment rule is testable with a future that simply never resolves —
/// which is exactly the failure it exists for, and which no fake `ExecSession`
/// could reproduce (`ExecSession` has no public constructor).
///
/// Dropping the future is what releases the resources: it drops the apiserver
/// exec, which closes the websocket and ends the kopia process, and it returns
/// the borrow of the sink so the caller can read the byte count.
///
/// Detection latency is between one and two windows — progress that lands just
/// after a tick resets the baseline for the following one. That is deliberate:
/// a watchdog that fired on the *first* quiet window would abort transfers that
/// merely paused, and the cost of one extra window is a minute of a held permit.
pub async fn under_watchdog<F, T, E>(
    copy: F,
    progress: Arc<AtomicU64>,
    window: Duration,
) -> Copied<T, E>
where
    F: Future<Output = Result<T, E>>,
{
    tokio::pin!(copy);
    let mut last = progress.load(Ordering::Relaxed);
    loop {
        tokio::select! {
            result = &mut copy => return Copied::Finished(result),
            () = tokio::time::sleep(window) => {
                let now = progress.load(Ordering::Relaxed);
                if now == last {
                    return Copied::Stalled;
                }
                last = now;
            }
        }
    }
}

// --- the sink ---------------------------------------------------------------

/// An [`AsyncWrite`] that accepts exactly `expected` bytes and refuses the
/// first one past it.
///
/// **Overrun** is the failure it exists to catch: kopia produced more than the
/// manifest recorded. The extra bytes cannot be sent (the `Content-Length` is
/// already committed), so the only choices are to truncate silently or to fail
/// loudly. It fails.
///
/// A *short* copy is not the sink's to detect — it can only be seen once the
/// source is exhausted — so [`ExactSink::written`] reports the count and the
/// caller compares. A *stall* is not the sink's either: it is
/// [`under_watchdog`]'s, reading the same count through
/// [`ExactSink::progress`].
pub struct ExactSink<W> {
    inner: W,
    expected: u64,
    /// Published as well as stored, so the watchdog can observe progress while
    /// the copy holds `&mut self`.
    written: Arc<AtomicU64>,
    overran: bool,
}

impl<W> ExactSink<W> {
    /// Wrap `inner`, accepting at most `expected` bytes.
    pub fn new(inner: W, expected: u64) -> Self {
        Self {
            inner,
            expected,
            written: Arc::new(AtomicU64::new(0)),
            overran: false,
        }
    }

    /// A handle on the running byte count, for [`under_watchdog`].
    pub fn progress(&self) -> Arc<AtomicU64> {
        self.written.clone()
    }

    /// Bytes accepted so far. Compared against the promised size once the copy
    /// ends, which is the only place a short read can be seen.
    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }

    /// Whether a write was refused for exceeding the promised size.
    pub fn overran(&self) -> bool {
        self.overran
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for ExactSink<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.written().saturating_add(buf.len() as u64) > self.expected {
            self.overran = true;
            return Poll::Ready(Err(overrun_error(self.expected)));
        }
        match Pin::new(&mut self.inner).poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => {
                self.written.fetch_add(n as u64, Ordering::Relaxed);
                Poll::Ready(Ok(n))
            }
            other => other,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// The `download-overrun` IO error, worded for the log line it ends up in.
fn overrun_error(expected: u64) -> io::Error {
    io::Error::other(format!(
        "download-overrun: kopia produced more than the {expected} bytes the snapshot entry \
         records for this file. The response length was already committed, so the copy was \
         stopped instead of sending a body that contradicts it."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt as _;

    fn stream_io(kind: io::ErrorKind) -> OpsError {
        OpsError::StreamIo {
            what: "streaming a test download".to_string(),
            source: io::Error::new(kind, "test"),
        }
    }

    // --- the sink -----------------------------------------------------------

    #[tokio::test]
    async fn an_exact_copy_is_accepted_and_counted() {
        let mut sink = ExactSink::new(Vec::new(), 5);
        sink.write_all(b"hello").await.expect("exactly the size");
        assert_eq!(sink.written(), 5);
        assert!(!sink.overran());
    }

    #[tokio::test]
    async fn a_short_copy_is_visible_only_in_the_count() {
        // The sink cannot know the source ended early — that is the caller's
        // comparison, and this is the number it compares.
        let mut sink = ExactSink::new(Vec::new(), 11);
        sink.write_all(b"hello")
            .await
            .expect("a partial write is fine");
        assert_eq!(sink.written(), 5);
        assert!(!sink.overran(), "stopping early is not an overrun");
    }

    #[tokio::test]
    async fn the_first_byte_past_the_promised_size_is_refused() {
        let mut sink = ExactSink::new(Vec::new(), 4);
        sink.write_all(b"abcd").await.expect("up to the size");
        let error = sink.write_all(b"e").await.expect_err("one byte too many");
        assert!(sink.overran());
        assert_eq!(sink.written(), 4, "nothing past the cap is accepted");
        assert!(error.to_string().contains("download-overrun"), "{error}");
        assert!(error.to_string().contains('4'), "{error}");
    }

    #[tokio::test]
    async fn an_oversized_single_write_is_refused_whole_rather_than_truncated() {
        // A partial accept would put a torn record in the body and still leave
        // the count short; refusing the write is what makes the failure visible.
        let mut sink = ExactSink::new(Vec::new(), 3);
        let error = sink
            .write_all(b"abcdef")
            .await
            .expect_err("larger than the whole promise");
        assert!(sink.overran());
        assert_eq!(sink.written(), 0);
        assert!(error.to_string().contains("download-overrun"), "{error}");
    }

    #[tokio::test]
    async fn the_progress_handle_tracks_the_same_count_the_sink_reports() {
        let mut sink = ExactSink::new(Vec::new(), 16);
        let progress = sink.progress();
        assert_eq!(progress.load(Ordering::Relaxed), 0);
        sink.write_all(b"1234").await.expect("write");
        assert_eq!(progress.load(Ordering::Relaxed), 4);
        assert_eq!(progress.load(Ordering::Relaxed), sink.written());
    }

    // --- the watchdog -------------------------------------------------------

    #[tokio::test]
    async fn a_source_that_stops_producing_is_abandoned() {
        // The failure the watchdog exists for: kopia (or the exec websocket)
        // wedges, so `poll_write` is never even called and no byte-count moves.
        // Without this the task would hold its ExecPermit forever.
        let progress = Arc::new(AtomicU64::new(0));
        let outcome: Copied<u64, OpsError> =
            under_watchdog(std::future::pending(), progress, Duration::from_millis(30)).await;
        assert!(matches!(outcome, Copied::Stalled), "{outcome:?}");
    }

    #[tokio::test]
    async fn a_client_that_stops_draining_is_abandoned_by_the_same_watchdog() {
        // The other direction of silence: the reader never reads, the duplex
        // fills, writes stop completing, and the count therefore stops moving.
        // One mechanism covers both.
        let (writer, _never_read) = tokio::io::duplex(8);
        let mut sink = ExactSink::new(writer, 1024);
        let progress = sink.progress();
        let copy = async {
            sink.write_all(&[0u8; 512])
                .await
                .map_err(|source| OpsError::StreamIo {
                    what: "test".to_string(),
                    source,
                })
        };
        let outcome: Copied<(), OpsError> =
            under_watchdog(copy, progress, Duration::from_millis(30)).await;
        assert!(matches!(outcome, Copied::Stalled), "{outcome:?}");
    }

    #[tokio::test]
    async fn a_copy_that_keeps_moving_is_never_abandoned() {
        let progress = Arc::new(AtomicU64::new(0));
        let ticker = progress.clone();
        let copy = async move {
            // Six advances across ~120ms, against a 30ms window: a transfer
            // slower than the window must still survive as long as it moves.
            for _ in 0..6 {
                tokio::time::sleep(Duration::from_millis(20)).await;
                ticker.fetch_add(1, Ordering::Relaxed);
            }
            Ok::<_, OpsError>(6u64)
        };
        let outcome = under_watchdog(copy, progress, Duration::from_millis(30)).await;
        match outcome {
            Copied::Finished(Ok(n)) => assert_eq!(n, 6),
            other => panic!("a moving copy must not be abandoned: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_copy_that_finishes_immediately_reports_its_own_result() {
        let progress = Arc::new(AtomicU64::new(0));
        let outcome = under_watchdog(
            async { Err::<(), _>(stream_io(io::ErrorKind::Other)) },
            progress,
            Duration::from_secs(60),
        )
        .await;
        assert!(matches!(outcome, Copied::Finished(Err(_))), "{outcome:?}");
    }

    // --- classification -----------------------------------------------------

    fn classify_with(
        written: u64,
        overran: bool,
        stalled: bool,
        error: Option<&OpsError>,
    ) -> DownloadOutcome {
        classify(&CopyResult {
            expected: 100,
            written,
            overran,
            stalled,
            error,
        })
    }

    #[test]
    fn a_complete_transfer_counts_nothing() {
        let outcome = classify_with(100, false, false, None);
        assert_eq!(outcome, DownloadOutcome::Complete);
        assert_eq!(outcome.metric(), None);
    }

    #[test]
    fn a_cancelled_download_is_never_an_integrity_failure() {
        // The whole point of the ClientCancelled bucket: someone hitting Escape
        // on a 4 GB file must not ring the "the backup may be corrupt" alarm.
        for kind in [
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::UnexpectedEof,
        ] {
            let error = stream_io(kind);
            let outcome = classify_with(7, false, false, Some(&error));
            assert_eq!(outcome, DownloadOutcome::ClientCancelled, "{kind:?}");
            assert_eq!(
                outcome.metric(),
                Some(DownloadIncomplete::ClientCancelled),
                "{kind:?}"
            );
            assert!(
                !outcome.metric().expect("a cause").is_integrity_failure(),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn every_other_mid_stream_failure_still_counts_as_short() {
        // Only the client-gone kinds are excused. A wedged pod, a refused exec
        // or an unclassified IO error all leave a truncated file.
        for kind in [
            io::ErrorKind::Other,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::TimedOut,
        ] {
            let error = stream_io(kind);
            let outcome = classify_with(7, false, false, Some(&error));
            assert_eq!(outcome, DownloadOutcome::Short, "{kind:?}");
            assert_eq!(
                outcome.metric(),
                Some(DownloadIncomplete::Short),
                "{kind:?}"
            );
        }

        // A non-StreamIo failure is never a cancellation either.
        let exec = OpsError::SessionExec {
            what: "show kabc".into(),
            stderr: "unable to open object".into(),
        };
        assert_eq!(
            classify_with(7, false, false, Some(&exec)),
            DownloadOutcome::Short
        );
    }

    #[test]
    fn a_clean_but_short_copy_is_short() {
        assert_eq!(classify_with(7, false, false, None), DownloadOutcome::Short);
    }

    #[test]
    fn a_stall_is_reported_as_a_stall_but_counted_as_short() {
        // The client is left holding a body that stopped before its committed
        // length — the consequence is a short download; the cause is the log.
        let outcome = classify_with(7, false, true, None);
        assert_eq!(outcome, DownloadOutcome::Stalled);
        assert_eq!(outcome.metric(), Some(DownloadIncomplete::Short));
        assert_eq!(outcome.as_str(), "stalled");
        assert!(
            outcome
                .summary()
                .contains("KOPIUR_UI_DOWNLOAD_CHUNK_TIMEOUT"),
            "the log line must name the knob: {}",
            outcome.summary()
        );
    }

    #[test]
    fn an_overrun_outranks_everything_else() {
        // kopia and the manifest disagreeing is a repository fact; it must not
        // be masked by whatever the transport did about it afterwards.
        let error = stream_io(io::ErrorKind::BrokenPipe);
        for (stalled, err) in [(false, None), (true, None), (false, Some(&error))] {
            let outcome = classify_with(100, true, stalled, err);
            assert_eq!(outcome, DownloadOutcome::Overrun);
            assert_eq!(outcome.metric(), Some(DownloadIncomplete::Overrun));
        }
    }

    #[test]
    fn every_outcome_has_a_distinct_token_and_a_summary() {
        let all = [
            DownloadOutcome::Complete,
            DownloadOutcome::ClientCancelled,
            DownloadOutcome::Stalled,
            DownloadOutcome::Overrun,
            DownloadOutcome::Short,
        ];
        let tokens: std::collections::BTreeSet<_> = all.iter().map(|o| o.as_str()).collect();
        assert_eq!(tokens.len(), all.len(), "{tokens:?}");
        for outcome in all {
            assert!(!outcome.summary().is_empty(), "{outcome:?}");
        }
    }
}
