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
//! checked against it afterwards, and either mismatch increments
//! [`DownloadIncomplete`] — a metric whose whole purpose is to be zero.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use tokio::io::AsyncWrite;
use tokio::time::{Instant, Sleep};
use tokio_util::io::ReaderStream;

use kopiur_kopia::{ObjectId, SessionCmd};
use kopiur_ops::browse::session::ExecSession;

use crate::browse::session_pool::ExecPermit;
use crate::metrics::{DownloadIncomplete, UiMetrics};

/// Bytes buffered between the kopia exec and the HTTP response.
///
/// Back-pressure is the point: when the client stops reading, this fills, the
/// copy blocks, and the session pod stops producing. A larger buffer would only
/// let a stalled download hold more memory per connection.
const PIPE_BYTES: usize = 64 * 1024;

/// How long a single write may sit blocked before the transfer is abandoned.
///
/// A write only blocks when the pipe is full, which only happens when the client
/// has stopped reading — a closed laptop, a dropped connection the runtime has
/// not noticed. Without this the task would hold its [`ExecPermit`], its
/// apiserver websocket, and a kopia process for as long as the peer stays
/// silent, which for a half-open TCP connection is a very long time.
const STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Build the response body for one file download.
///
/// The exec runs in a spawned task rather than inline, because the body must be
/// returned to axum before the first byte exists — a handler that awaited the
/// copy would buffer the whole file in memory to produce a `Body` from it. The
/// task owns `permit` for its whole life, so the exec cap counts a download for
/// as long as it is actually running rather than for the instant that started
/// it.
///
/// The task never fails the response: by the time the body is streaming, the
/// status and headers are already on the wire. A failure therefore shows up as a
/// short body (which the client sees as a truncated download against the
/// promised `Content-Length`), a `warn!` naming the file, and a
/// [`DownloadIncomplete`] count.
pub fn stream_file(
    session: ExecSession,
    oid: ObjectId,
    size: u64,
    permit: ExecPermit,
    metrics: Arc<UiMetrics>,
) -> Body {
    let (writer, reader) = tokio::io::duplex(PIPE_BYTES);

    tokio::spawn(async move {
        // Held for the whole copy: the exec cap bounds concurrent transfers, not
        // concurrent requests that start one.
        let _permit = permit;
        let mut sink = ExactSink::new(writer, size, Some(STALL_TIMEOUT));
        let outcome = session
            .exec_stream(SessionCmd::ShowObject { oid: oid.clone() }, &mut sink)
            .await;
        let written = sink.written();

        match outcome {
            Ok(_) if written == size => {}
            Ok(_) => {
                metrics.inc_download_incomplete(DownloadIncomplete::Short);
                tracing::warn!(
                    oid = oid.as_str(),
                    expected = size,
                    written,
                    "a file download ended before the size recorded in the snapshot; the \
                     client holds a truncated file"
                );
            }
            Err(error) if sink.overran() => {
                metrics.inc_download_incomplete(DownloadIncomplete::Overrun);
                tracing::warn!(
                    oid = oid.as_str(),
                    expected = size,
                    %error,
                    "a file download produced more bytes than the snapshot entry records; \
                     the copy was cut off at the recorded size"
                );
            }
            Err(error) => {
                if written < size {
                    metrics.inc_download_incomplete(DownloadIncomplete::Short);
                }
                tracing::warn!(
                    oid = oid.as_str(),
                    expected = size,
                    written,
                    %error,
                    "a file download failed mid-stream"
                );
            }
        }
    });

    Body::from_stream(ReaderStream::new(reader))
}

/// An [`AsyncWrite`] that accepts exactly `expected` bytes and refuses the
/// first one past it, optionally abandoning a write that stays blocked.
///
/// Two failures it exists to catch, both of which end with a user holding a file
/// that does not match the backup:
///
/// * **Overrun.** kopia produced more than the manifest recorded. The extra
///   bytes cannot be sent (the `Content-Length` is already committed), so the
///   only choices are to truncate silently or to fail loudly. It fails.
/// * **Stall.** The reader stopped consuming. Without a deadline the write stays
///   `Pending` forever and the task leaks its exec permit; see [`STALL_TIMEOUT`].
///
/// A *short* copy is not the sink's to detect — it can only be seen once the
/// source is exhausted — so [`ExactSink::written`] reports the count and the
/// caller compares.
pub struct ExactSink<W> {
    inner: W,
    expected: u64,
    written: u64,
    overran: bool,
    stall: Option<Duration>,
    /// Boxed so the sink stays `Unpin` for `W: Unpin`, which is what
    /// `exec_stream`'s `&mut (dyn AsyncWrite + Unpin + Send)` requires. Created
    /// on the first blocked write, so constructing a sink needs no runtime.
    deadline: Option<Pin<Box<Sleep>>>,
}

impl<W> ExactSink<W> {
    /// Wrap `inner`, accepting at most `expected` bytes.
    ///
    /// `stall` is `None` in tests that drive the sink directly and
    /// [`STALL_TIMEOUT`] in the download path.
    pub fn new(inner: W, expected: u64, stall: Option<Duration>) -> Self {
        Self {
            inner,
            expected,
            written: 0,
            overran: false,
            stall,
            deadline: None,
        }
    }

    /// Bytes accepted so far. Compared against the promised size once the copy
    /// ends, which is the only place a short read can be seen.
    pub fn written(&self) -> u64 {
        self.written
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
        if self.written.saturating_add(buf.len() as u64) > self.expected {
            self.overran = true;
            return Poll::Ready(Err(overrun_error(self.expected)));
        }
        match Pin::new(&mut self.inner).poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => {
                self.written += n as u64;
                // Progress: the stall clock starts again from here.
                self.deadline = None;
                Poll::Ready(Ok(n))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => self.poll_stall(cx),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl<W> ExactSink<W> {
    /// Arm (or check) the stall deadline for a write that could not proceed.
    ///
    /// Polling the `Sleep` is also what registers the timer's waker, so a task
    /// blocked on a reader that never reads is still woken to fail.
    fn poll_stall(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let Some(stall) = self.stall else {
            return Poll::Pending;
        };
        let deadline = self
            .deadline
            .get_or_insert_with(|| Box::pin(tokio::time::sleep_until(Instant::now() + stall)));
        match deadline.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "the client stopped reading this download for over {}s, so kopiur-ui \
                     abandoned it rather than hold a session exec open indefinitely",
                    stall.as_secs()
                ),
            ))),
            Poll::Pending => Poll::Pending,
        }
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

    #[tokio::test]
    async fn an_exact_copy_is_accepted_and_counted() {
        let mut sink = ExactSink::new(Vec::new(), 5, None);
        sink.write_all(b"hello").await.expect("exactly the size");
        assert_eq!(sink.written(), 5);
        assert!(!sink.overran());
    }

    #[tokio::test]
    async fn a_short_copy_is_visible_only_in_the_count() {
        // The sink cannot know the source ended early — that is the caller's
        // comparison, and this is the number it compares.
        let mut sink = ExactSink::new(Vec::new(), 11, None);
        sink.write_all(b"hello")
            .await
            .expect("a partial write is fine");
        assert_eq!(sink.written(), 5);
        assert!(!sink.overran(), "stopping early is not an overrun");
    }

    #[tokio::test]
    async fn the_first_byte_past_the_promised_size_is_refused() {
        let mut sink = ExactSink::new(Vec::new(), 4, None);
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
        let mut sink = ExactSink::new(Vec::new(), 3, None);
        let error = sink
            .write_all(b"abcdef")
            .await
            .expect_err("larger than the whole promise");
        assert!(sink.overran());
        assert_eq!(sink.written(), 0);
        assert!(error.to_string().contains("download-overrun"), "{error}");
    }

    #[tokio::test]
    async fn a_write_that_stays_blocked_past_the_stall_timeout_fails() {
        // A reader that never reads: the duplex fills and the write blocks.
        // A short deadline stands in for STALL_TIMEOUT so the test is fast; the
        // behaviour under test is that the deadline exists and fires at all.
        let (writer, _reader) = tokio::io::duplex(8);
        let mut sink = ExactSink::new(writer, 1024, Some(Duration::from_millis(50)));

        let error = sink
            .write_all(&[0u8; 64])
            .await
            .expect_err("the reader never drains");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        assert!(error.to_string().contains("stopped reading"), "{error}");
        assert!(!sink.overran(), "a stall is not an overrun");
    }

    #[tokio::test]
    async fn progress_resets_the_stall_clock() {
        let (writer, mut reader) = tokio::io::duplex(8);
        let mut sink = ExactSink::new(writer, 32, Some(Duration::from_secs(60)));

        let pump = tokio::spawn(async move {
            use tokio::io::AsyncReadExt as _;
            let mut buf = vec![0u8; 32];
            reader.read_exact(&mut buf).await.expect("drained");
        });

        // Four writes that each have to wait for the reader: none of them may
        // trip the deadline, because each one makes progress.
        for _ in 0..4 {
            sink.write_all(&[7u8; 8]).await.expect("a draining reader");
        }
        assert_eq!(sink.written(), 32);
        pump.await.expect("pump");
    }
}
